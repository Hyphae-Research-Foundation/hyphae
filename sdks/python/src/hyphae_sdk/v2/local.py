# SPDX-License-Identifier: AGPL-3.0-only
"""Serial AF_UNIX and Windows named-pipe HYPHLCL1 transport."""

from __future__ import annotations

import os
import socket
import struct
import threading
import time
from typing import BinaryIO

from .models import ClientError, ProductError, RequestOptions, Response, product_error
from .protocol import (
    API_KEY_AUTH_CAPABILITY,
    FRAME_HEADER_SIZE,
    FRAME_KINDS,
    G6_CAPABILITIES,
    PROTOCOL_MINOR,
    blake3,
    decode_end,
    decode_frame,
    decode_product_error,
    decode_product_response,
    decode_welcome,
    encode_cancel,
    encode_authenticated_hello,
    encode_frame,
    encode_hello,
    encode_product_request,
    encode_window_update,
)

_WINDOWS_PIPE_PREFIX = "\\\\.\\pipe\\"
_MAXIMUM_FRAME_PAYLOAD = 16 * 1024 * 1024


class LocalTransport:
    """Exact local byte-stream transport with no wrapper protocol."""

    def __init__(
        self,
        endpoint: str,
        *,
        client_identity: str = "hyphae-python-sdk-v2",
        api_key: str | None = None,
    ) -> None:
        if not endpoint:
            raise ClientError("local endpoint must not be empty")
        if not client_identity or len(client_identity.encode()) > 4096:
            raise ClientError("local client identity is invalid")
        credential: bytearray | None = None
        if api_key is not None:
            encode_authenticated_hello(api_key, client_identity)
            credential = bytearray(api_key, "utf-8")
        self._endpoint = _windows_pipe_namespace(endpoint) if os.name == "nt" else endpoint
        self._client_identity = client_identity
        self._managed = credential is not None
        self._api_key = credential
        self._closed = False
        self._stream: socket.socket | BinaryIO | None = None
        self._lock = threading.Lock()
        self._next_stream_id = 1
        self._initial_window = 64 * 1024
        self._maximum_frame_payload = _MAXIMUM_FRAME_PAYLOAD
        self._negotiated_minor: int | None = None

    def __repr__(self) -> str:
        authentication = "managed" if self._managed else "unmanaged"
        return f"LocalTransport(endpoint={self._endpoint!r}, authentication={authentication!r})"

    def __enter__(self) -> LocalTransport:
        return self

    def __exit__(self, *exc_info: object) -> None:
        del exc_info
        self.close()

    def __del__(self) -> None:
        stream = getattr(self, "_stream", None)
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass
        api_key = getattr(self, "_api_key", None)
        if api_key is not None:
            api_key[:] = b"\0" * len(api_key)

    @property
    def negotiated_minor(self) -> int | None:
        """Minor selected by the server after the first operation."""

        return self._negotiated_minor

    def execute(self, operation: str, arguments: dict[str, object], options: RequestOptions) -> Response:
        with self._lock:
            if self._closed:
                raise ClientError("local transport is closed")
            request_id = options.checked_request_id()
            if options.cancellation.cancelled:
                raise product_error("cancelled", request_id)
            if self._stream is None:
                self._connect(((request_id + 1) & ((1 << 64) - 1)) or 1)
            assert self._stream is not None
            stream_id = self._next_stream_id
            self._next_stream_id = self._next_stream_id % 0xFFFFFFFF + 1
            assert self._negotiated_minor is not None
            payload = encode_product_request(
                operation,
                arguments,
                options,
                negotiated_minor=self._negotiated_minor,
            )
            frame_kind = FRAME_KINDS["prepare"] if operation == "sql_prepare" else FRAME_KINDS["deallocate"] if operation == "sql_deallocate" else FRAME_KINDS["execute"]
            self._write(encode_frame(frame_kind, stream_id, request_id, payload))
            provisional = bytearray()
            credited = 0
            maximum = min(options.limits["max_response_bytes"], 16 * 1024 * 1024)
            while True:
                if options.cancellation.cancelled:
                    self._write(encode_frame(FRAME_KINDS["cancel"], stream_id, request_id, encode_cancel()))
                    self._disconnect()
                    raise product_error("cancelled", request_id)
                try:
                    self._check_deadline(options)
                except ProductError:
                    self._write(encode_frame(FRAME_KINDS["cancel"], stream_id, request_id, encode_cancel(2)))
                    self._disconnect()
                    raise
                frame = self._read_frame(options)
                if frame.stream_id != stream_id or frame.request_id != request_id:
                    self._disconnect()
                    raise ClientError("local response correlation mismatch")
                if frame.kind == FRAME_KINDS["failure"]:
                    raise ProductError(decode_product_error(frame.payload))
                if frame.kind == FRAME_KINDS["data"]:
                    if len(provisional) + len(frame.payload) > maximum:
                        raise ClientError("local response exceeds the configured maximum")
                    provisional.extend(frame.payload)
                    credited += len(frame.payload)
                    if credited >= max(1, self._initial_window // 2):
                        self._write(encode_frame(FRAME_KINDS["window_update"], stream_id, request_id, encode_window_update(credited)))
                        credited = 0
                    continue
                if frame.kind == FRAME_KINDS["end"]:
                    total, digest = decode_end(frame.payload)
                    if total != len(provisional) or digest != blake3(provisional):
                        raise ClientError("local provisional response completion mismatch")
                    return decode_product_response(
                        bytes(provisional),
                        request_id,
                        negotiated_minor=self._negotiated_minor,
                    )
                self._disconnect()
                raise ClientError("local server returned an invalid response frame")

    def close(self) -> None:
        self._disconnect()
        self._closed = True
        if self._api_key is not None:
            self._api_key[:] = b"\0" * len(self._api_key)
            self._api_key = None

    def _disconnect(self) -> None:
        stream, self._stream = self._stream, None
        self._negotiated_minor = None
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass

    def _connect(self, request_id: int) -> None:
        try:
            if os.name == "nt":
                self._stream = open(_WINDOWS_PIPE_PREFIX + self._endpoint, "r+b", buffering=0)
            else:
                stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                stream.connect(self._endpoint)
                stream.settimeout(0.05)
                self._stream = stream
            hello = (
                encode_hello(self._client_identity)
                if not self._managed
                else encode_authenticated_hello(
                    self._api_key or b"",
                    self._client_identity,
                    maximum_minor=PROTOCOL_MINOR,
                )
            )
            self._write(encode_frame(FRAME_KINDS["hello"], 0, request_id, hello))
            frame = self._read_frame(RequestOptions(request_id=request_id))
            if frame.kind == FRAME_KINDS["failure"]:
                raise ProductError(decode_product_error(frame.payload))
            if frame.kind != FRAME_KINDS["welcome"] or frame.stream_id != 0 or frame.request_id != request_id:
                raise ClientError("local handshake response mismatch")
            welcome = decode_welcome(frame.payload)
            if welcome["capabilities"] & G6_CAPABILITIES != G6_CAPABILITIES:
                raise ClientError("local server omitted required Native capabilities")
            if self._managed and not welcome["capabilities"] & API_KEY_AUTH_CAPABILITY:
                raise ClientError("local server downgraded managed API-key authentication")
            self._negotiated_minor = welcome["minor"]
            self._initial_window = welcome["initial_window"]
            maximum_frame_payload = welcome["maximum_frame_payload"]
            if not 0 < maximum_frame_payload <= _MAXIMUM_FRAME_PAYLOAD:
                raise ClientError("local handshake frame limit is invalid")
            self._maximum_frame_payload = maximum_frame_payload
        except BaseException:
            self._disconnect()
            raise

    def _read_frame(self, options: RequestOptions):  # type: ignore[no-untyped-def]
        header = self._read_exact(FRAME_HEADER_SIZE, options)
        length = struct.unpack_from("<I", header, 24)[0]
        if length > self._maximum_frame_payload:
            raise ClientError("local frame exceeds the negotiated maximum")
        return decode_frame(header + self._read_exact(length, options))

    def _read_exact(self, length: int, options: RequestOptions) -> bytes:
        assert self._stream is not None
        output = bytearray()
        while len(output) < length:
            if options.cancellation.cancelled:
                raise product_error("cancelled", options.checked_request_id())
            self._check_deadline(options)
            try:
                if isinstance(self._stream, socket.socket):
                    chunk = self._stream.recv(length - len(output))
                else:
                    chunk = self._stream.read(length - len(output))
            except socket.timeout:
                continue
            except OSError as error:
                self._disconnect()
                raise ClientError("native-local transport failed") from error
            if not chunk:
                self._disconnect()
                raise ClientError("native-local stream closed before completion")
            output.extend(chunk)
        return bytes(output)

    def _write(self, encoded: bytes) -> None:
        assert self._stream is not None
        if len(encoded) < FRAME_HEADER_SIZE:
            raise ClientError("local frame is truncated")
        if struct.unpack_from("<I", encoded, 24)[0] > self._maximum_frame_payload:
            raise ClientError("local frame exceeds the negotiated maximum")
        try:
            if isinstance(self._stream, socket.socket):
                self._stream.sendall(encoded)
            else:
                _write_all(self._stream, encoded)
        except OSError as error:
            self._disconnect()
            raise ClientError("native-local transport failed") from error

    @staticmethod
    def _check_deadline(options: RequestOptions) -> None:
        if options.deadline_micros is not None and time.time_ns() // 1000 >= options.deadline_micros:
            raise product_error("deadline_exceeded", options.checked_request_id())


def _windows_pipe_namespace(endpoint: str) -> str:
    if endpoint.lower().startswith(_WINDOWS_PIPE_PREFIX.lower()):
        endpoint = endpoint[len(_WINDOWS_PIPE_PREFIX):]
    if not endpoint or endpoint.startswith("\\\\"):
        raise ClientError("Windows local endpoint must be a local named-pipe namespace")
    return endpoint


def _write_all(stream: BinaryIO, encoded: bytes) -> None:
    remaining = memoryview(encoded)
    while remaining:
        written = stream.write(remaining)
        if written is None or written <= 0:
            raise OSError("native-local stream accepted no bytes")
        remaining = remaining[written:]
    stream.flush()


__all__ = ["LocalTransport"]
