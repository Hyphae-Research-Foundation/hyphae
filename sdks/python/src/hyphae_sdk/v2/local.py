# SPDX-License-Identifier: Apache-2.0
"""Serial AF_UNIX and Windows named-pipe HYPHLCL1 transport."""

from __future__ import annotations

import errno
import os
import select
import socket
import struct
import threading
import time
from typing import BinaryIO

from .models import (
    CancellationToken,
    ClientError,
    ProductError,
    RequestOptions,
    Response,
    product_error,
)
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
    encode_authenticated_hello,
    encode_cancel,
    encode_frame,
    encode_hello,
    encode_product_request,
    encode_window_update,
)

_WINDOWS_PIPE_PREFIX = "\\\\.\\pipe\\"
_MAXIMUM_FRAME_PAYLOAD = 16 * 1024 * 1024
_WINDOWS_THREAD_TERMINATE = 0x0001
_WINDOWS_ERROR_NOT_FOUND = 1168


def _noop() -> None:
    pass


class _WindowsSynchronousIo:
    """Cancelable handle for synchronous named-pipe I/O owned by one worker."""

    def __init__(self, handle: int | None) -> None:
        self._handle = handle
        self._lock = threading.Lock()

    @classmethod
    def current(cls) -> _WindowsSynchronousIo:
        if os.name != "nt":
            return cls(None)
        import ctypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.OpenThread.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        kernel32.OpenThread.restype = ctypes.c_void_p
        handle = kernel32.OpenThread(
            _WINDOWS_THREAD_TERMINATE,
            False,
            threading.get_native_id(),
        )
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        return cls(int(handle))

    def cancel(self) -> None:
        if os.name != "nt":
            return
        import ctypes

        with self._lock:
            handle = self._handle
            if handle is None:
                return
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CancelSynchronousIo.argtypes = [ctypes.c_void_p]
            kernel32.CancelSynchronousIo.restype = ctypes.c_int
            if kernel32.CancelSynchronousIo(ctypes.c_void_p(handle)):
                return
            error = ctypes.get_last_error()
            if error != _WINDOWS_ERROR_NOT_FOUND:
                raise ctypes.WinError(error)

    def close(self) -> None:
        if os.name != "nt":
            return
        import ctypes

        with self._lock:
            handle, self._handle = self._handle, None
        if handle is not None:
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
            kernel32.CloseHandle.restype = ctypes.c_int
            kernel32.CloseHandle(ctypes.c_void_p(handle))


class _LocalRequestContext:
    def __init__(
        self,
        generation: int,
        cancellation: CancellationToken,
        windows_io: _WindowsSynchronousIo,
    ) -> None:
        self.generation = generation
        self.cancellation = cancellation
        self.windows_io = windows_io
        self.aborted = False
        self.deadline_expired = False


class LocalTransport:
    """Exact local byte-stream transport with no wrapper protocol."""

    abort_invalidates_session = True

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
        self._endpoint = (
            _windows_pipe_namespace(endpoint) if os.name == "nt" else endpoint
        )
        self._client_identity = client_identity
        self._managed = credential is not None
        self._api_key = credential
        self._closed = False
        self._stream: socket.socket | BinaryIO | None = None
        self._lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._next_generation = 1
        self._active_context: _LocalRequestContext | None = None
        self._next_stream_id = 1
        self._initial_window = 64 * 1024
        self._maximum_frame_payload = _MAXIMUM_FRAME_PAYLOAD
        self._negotiated_minor: int | None = None

    def __repr__(self) -> str:
        authentication = "managed" if self._managed else "unmanaged"
        return (
            f"LocalTransport(endpoint={self._endpoint!r}, "
            f"authentication={authentication!r})"
        )

    def __enter__(self) -> LocalTransport:
        with self._state_lock:
            if self._closed:
                raise ClientError("local transport is closed")
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

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        with self._lock:
            with self._state_lock:
                if self._closed:
                    raise ClientError("local transport is closed")
            windows_io = _WindowsSynchronousIo.current()
            with self._state_lock:
                if self._closed:
                    windows_io.close()
                    raise ClientError("local transport is closed")
                context = _LocalRequestContext(
                    self._next_generation,
                    options.cancellation,
                    windows_io,
                )
                self._next_generation += 1
                self._active_context = context
            unsubscribe_cancellation = _noop
            deadline_timer: threading.Timer | None = None
            try:
                unsubscribe_cancellation = options.cancellation._subscribe(
                    lambda: self._abort_context(context)
                )
                if options.deadline_micros is not None:
                    deadline_timer = threading.Timer(
                        max(
                            0.0,
                            options.deadline_micros / 1_000_000 - time.time(),
                        ),
                        self._expire_context,
                        args=(context,),
                    )
                    deadline_timer.daemon = True
                    deadline_timer.start()
                return self._execute_locked(operation, arguments, options)
            finally:
                if deadline_timer is not None:
                    deadline_timer.cancel()
                unsubscribe_cancellation()
                with self._state_lock:
                    if self._active_context is context:
                        self._active_context = None
                windows_io.close()

    def _execute_locked(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        request_id = options.checked_request_id()
        terminal_replay = operation in {
            "security_api_key_revoke_self",
            "security_api_key_rotate_self_activate",
        }
        fresh_terminal_replay = terminal_replay and self._current_stream() is None
        if options.cancellation.cancelled:
            raise product_error("cancelled", request_id)
        self._check_deadline(options)
        if self._current_stream() is None:
            with self._state_lock:
                context = self._active_context
                interrupted = context is not None and context.aborted
            if interrupted:
                raise self._interrupted_error(options)
            terminal_payload = (
                encode_product_request(
                    operation,
                    arguments,
                    options,
                    negotiated_minor=PROTOCOL_MINOR,
                )
                if fresh_terminal_replay
                else None
            )
            self._connect(
                ((request_id + 1) & ((1 << 64) - 1)) or 1,
                options,
                terminal_request=(1, request_id, terminal_payload)
                if terminal_payload is not None
                else None,
            )
            if terminal_payload is not None:
                self._next_stream_id = 2
        stream_id = self._next_stream_id
        self._next_stream_id = self._next_stream_id % 0xFFFFFFFF + 1
        if self._negotiated_minor is None:
            raise self._interrupted_error(options)
        payload = encode_product_request(
            operation,
            arguments,
            options,
            negotiated_minor=self._negotiated_minor,
        )
        if options.cancellation.cancelled:
            self._abort_context_for_options(options)
            raise product_error("cancelled", request_id)
        self._check_deadline(options)
        if operation == "sql_prepare":
            frame_kind = FRAME_KINDS["prepare"]
        elif operation == "sql_deallocate":
            frame_kind = FRAME_KINDS["deallocate"]
        else:
            frame_kind = FRAME_KINDS["execute"]
        if not fresh_terminal_replay:
            self._write(encode_frame(frame_kind, stream_id, request_id, payload), options)
        provisional = bytearray()
        credited = 0
        maximum = min(options.limits["max_response_bytes"], 16 * 1024 * 1024)
        while True:
            if options.cancellation.cancelled:
                self._best_effort_cancel(stream_id, request_id, options)
                raise product_error("cancelled", request_id)
            try:
                self._check_deadline(options)
            except ProductError:
                self._best_effort_cancel(stream_id, request_id, options, reason=2)
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
                    self._write(
                        encode_frame(
                            FRAME_KINDS["window_update"],
                            stream_id,
                            request_id,
                            encode_window_update(credited),
                        ),
                        options,
                    )
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
        with self._state_lock:
            if self._closed:
                return
            self._closed = True
            credential, self._api_key = self._api_key, None
            context = self._active_context
            stream, self._stream = self._stream, None
            self._negotiated_minor = None
            if context is not None:
                context.aborted = True
        if credential is not None:
            credential[:] = b"\0" * len(credential)
        self._interrupt_detached(context, stream)

    def abort(self, cancellation: CancellationToken | None = None) -> None:
        """Interrupts the active stream without making the transport terminal."""

        with self._state_lock:
            context = self._active_context
            if cancellation is not None and (
                context is None or context.cancellation is not cancellation
            ):
                return
            if context is None:
                stream, self._stream = self._stream, None
                self._negotiated_minor = None
            else:
                stream = None
        if context is None:
            self._close_stream(stream)
        else:
            self._abort_context(context)

    def _abort_context(self, context: _LocalRequestContext) -> None:
        with self._state_lock:
            active = self._active_context
            if active is not context or active.generation != context.generation:
                return
            context.aborted = True
            stream, self._stream = self._stream, None
            self._negotiated_minor = None
        self._interrupt_detached(context, stream)

    def _expire_context(self, context: _LocalRequestContext) -> None:
        with self._state_lock:
            active = self._active_context
            if active is not context or active.generation != context.generation:
                return
            context.deadline_expired = True
        try:
            self._abort_context(context)
        except ClientError:
            pass

    def _interrupt_detached(
        self,
        context: _LocalRequestContext | None,
        stream: socket.socket | BinaryIO | None,
    ) -> None:
        cancellation_error: OSError | None = None
        try:
            if context is not None:
                context.windows_io.cancel()
        except OSError as error:
            cancellation_error = error
        self._close_stream(stream)
        if cancellation_error is not None:
            raise ClientError(
                "could not interrupt Windows named-pipe I/O"
            ) from cancellation_error

    @staticmethod
    def _close_stream(stream: socket.socket | BinaryIO | None) -> None:
        if stream is not None:
            if isinstance(stream, socket.socket):
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except (OSError, ValueError):
                    pass
            try:
                stream.close()
            except (OSError, ValueError):
                pass

    def _disconnect(self) -> None:
        with self._state_lock:
            stream, self._stream = self._stream, None
            self._negotiated_minor = None
        self._close_stream(stream)

    def _connect(
        self,
        request_id: int,
        options: RequestOptions | None = None,
        terminal_request: tuple[int, int, bytes] | None = None,
    ) -> None:
        request_options = options or RequestOptions(request_id=request_id)
        self._check_deadline(request_options)
        try:
            if os.name == "nt":
                stream: socket.socket | BinaryIO = open(
                    _WINDOWS_PIPE_PREFIX + self._endpoint,
                    "r+b",
                    buffering=0,
                )
            else:
                stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self._publish_stream(stream, request_options)
                self._connect_unix(stream, request_options)
            self._publish_stream(stream, request_options)
            hello = (
                encode_hello(self._client_identity, maximum_minor=PROTOCOL_MINOR)
                if not self._managed
                else encode_authenticated_hello(
                    self._api_key or b"",
                    self._client_identity,
                    maximum_minor=PROTOCOL_MINOR,
                )
            )
            self._write(
                encode_frame(FRAME_KINDS["hello"], 0, request_id, hello),
                request_options,
            )
            if terminal_request is not None:
                stream_id, terminal_request_id, payload = terminal_request
                self._write(
                    encode_frame(
                        FRAME_KINDS["execute"],
                        stream_id,
                        terminal_request_id,
                        payload,
                    ),
                    request_options,
                )
            frame = self._read_frame(request_options)
            if frame.kind == FRAME_KINDS["failure"]:
                raise ProductError(decode_product_error(frame.payload))
            if (
                frame.kind != FRAME_KINDS["welcome"]
                or frame.stream_id != 0
                or frame.request_id != request_id
            ):
                raise ClientError("local handshake response mismatch")
            welcome = decode_welcome(frame.payload)
            if welcome["capabilities"] & G6_CAPABILITIES != G6_CAPABILITIES:
                raise ClientError("local server omitted required Native capabilities")
            if self._managed and not welcome["capabilities"] & API_KEY_AUTH_CAPABILITY:
                raise ClientError(
                    "local server downgraded managed API-key authentication"
                )
            self._negotiated_minor = welcome["minor"]
            self._initial_window = welcome["initial_window"]
            maximum_frame_payload = welcome["maximum_frame_payload"]
            if not 0 < maximum_frame_payload <= _MAXIMUM_FRAME_PAYLOAD:
                raise ClientError("local handshake frame limit is invalid")
            self._maximum_frame_payload = maximum_frame_payload
        except (ClientError, ProductError):
            self._disconnect()
            raise
        except (OSError, ValueError) as error:
            self._disconnect()
            self._check_deadline(request_options)
            with self._state_lock:
                context = self._active_context
                interrupted = self._closed or (context is not None and context.aborted)
            if interrupted or request_options.cancellation.cancelled:
                raise self._interrupted_error(request_options) from error
            raise ClientError("native-local endpoint connection failed") from error
        except BaseException:
            self._disconnect()
            raise

    def _connect_unix(
        self,
        stream: socket.socket,
        options: RequestOptions,
    ) -> None:
        stream.setblocking(False)
        result = stream.connect_ex(self._endpoint)
        if result not in {
            0,
            errno.EISCONN,
            errno.EINPROGRESS,
            errno.EALREADY,
            errno.EWOULDBLOCK,
            errno.EAGAIN,
            errno.EINTR,
        }:
            raise OSError(result, os.strerror(result))
        while result not in {0, errno.EISCONN}:
            self._check_connect_state(stream, options)
            try:
                _, writable, exceptional = select.select(
                    (),
                    (stream,),
                    (stream,),
                    0.05,
                )
            except (OSError, ValueError) as error:
                self._check_connect_state(stream, options)
                raise ClientError("native-local endpoint connection failed") from error
            self._check_connect_state(stream, options)
            if writable or exceptional:
                result = stream.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                if result not in {0, errno.EISCONN}:
                    raise OSError(result, os.strerror(result))
        stream.settimeout(0.05)

    def _check_connect_state(
        self,
        stream: socket.socket,
        options: RequestOptions,
    ) -> None:
        self._check_deadline(options)
        with self._state_lock:
            context = self._active_context
            interrupted = (
                self._closed
                or self._stream is not stream
                or (context is not None and context.aborted)
            )
        if interrupted or options.cancellation.cancelled:
            raise self._interrupted_error(options)

    def _publish_stream(
        self,
        stream: socket.socket | BinaryIO,
        options: RequestOptions,
    ) -> None:
        with self._state_lock:
            context = self._active_context
            interrupted = (
                self._closed
                or (context is not None and context.aborted)
                or (self._stream is not None and self._stream is not stream)
            )
            if not interrupted:
                self._stream = stream
        if interrupted:
            self._close_stream(stream)
            raise self._interrupted_error(options)

    def _read_frame(self, options: RequestOptions):  # type: ignore[no-untyped-def]
        header = self._read_exact(FRAME_HEADER_SIZE, options)
        length = struct.unpack_from("<I", header, 24)[0]
        if length > self._maximum_frame_payload:
            raise ClientError("local frame exceeds the negotiated maximum")
        return decode_frame(header + self._read_exact(length, options))

    def _read_exact(self, length: int, options: RequestOptions) -> bytes:
        output = bytearray()
        while len(output) < length:
            if options.cancellation.cancelled:
                self._disconnect()
                raise product_error("cancelled", options.checked_request_id())
            try:
                self._check_deadline(options)
            except ProductError:
                self._disconnect()
                raise
            stream = self._current_stream()
            if stream is None:
                raise self._interrupted_error(options)
            try:
                if isinstance(stream, socket.socket):
                    chunk = stream.recv(length - len(output))
                else:
                    chunk = stream.read(length - len(output))
            except socket.timeout:
                continue
            except (OSError, ValueError) as error:
                self._disconnect()
                with self._state_lock:
                    context = self._active_context
                    interrupted = self._closed or (
                        context is not None and context.aborted
                    )
                if interrupted or options.cancellation.cancelled:
                    raise self._interrupted_error(options) from error
                raise ClientError("native-local transport failed") from error
            if not chunk:
                self._disconnect()
                with self._state_lock:
                    closed = self._closed
                    context = self._active_context
                    interrupted = closed or (context is not None and context.aborted)
                if interrupted or options.cancellation.cancelled:
                    raise self._interrupted_error(options)
                raise ClientError("native-local stream closed before completion")
            output.extend(chunk)
        return bytes(output)

    def _write(self, encoded: bytes, options: RequestOptions) -> None:
        stream = self._current_stream()
        if stream is None:
            raise self._interrupted_error(options)
        if len(encoded) < FRAME_HEADER_SIZE:
            raise ClientError("local frame is truncated")
        if struct.unpack_from("<I", encoded, 24)[0] > self._maximum_frame_payload:
            raise ClientError("local frame exceeds the negotiated maximum")
        try:
            if isinstance(stream, socket.socket):
                stream.sendall(encoded)
            else:
                _write_all(stream, encoded)
        except (OSError, ValueError) as error:
            self._disconnect()
            with self._state_lock:
                context = self._active_context
                interrupted = self._closed or (context is not None and context.aborted)
            if interrupted or options.cancellation.cancelled:
                raise self._interrupted_error(options) from error
            raise ClientError("native-local transport failed") from error

    def _best_effort_cancel(
        self,
        stream_id: int,
        request_id: int,
        options: RequestOptions,
        *,
        reason: int = 1,
    ) -> None:
        try:
            self._write(
                encode_frame(
                    FRAME_KINDS["cancel"],
                    stream_id,
                    request_id,
                    encode_cancel(reason),
                ),
                options,
            )
        except (ClientError, ProductError):
            pass
        self._disconnect()

    def _current_stream(self) -> socket.socket | BinaryIO | None:
        with self._state_lock:
            return self._stream

    def _abort_context_for_options(self, options: RequestOptions) -> None:
        with self._state_lock:
            context = self._active_context
        if context is not None and context.cancellation is options.cancellation:
            self._abort_context(context)

    def _interrupted_error(self, options: RequestOptions) -> ClientError | ProductError:
        with self._state_lock:
            closed = self._closed
            context = self._active_context
            deadline_expired = context is not None and context.deadline_expired
        if deadline_expired:
            return product_error("deadline_exceeded", options.checked_request_id())
        if options.cancellation.cancelled:
            return product_error("cancelled", options.checked_request_id())
        return ClientError(
            "local transport is closed" if closed else "local transport was aborted"
        )

    @staticmethod
    def _check_deadline(options: RequestOptions) -> None:
        if (
            options.deadline_micros is not None
            and time.time_ns() // 1000 >= options.deadline_micros
        ):
            raise product_error("deadline_exceeded", options.checked_request_id())


def _windows_pipe_namespace(endpoint: str) -> str:
    if endpoint.lower().startswith(_WINDOWS_PIPE_PREFIX.lower()):
        endpoint = endpoint[len(_WINDOWS_PIPE_PREFIX) :]
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
