# SPDX-License-Identifier: Apache-2.0
"""Hosted Windows named-pipe lifecycle evidence for the installed v2 wheel."""

from __future__ import annotations

import asyncio
import ctypes
import importlib.metadata
import json
import os
import struct
import sys
import threading
import time
import unittest
import uuid
from pathlib import Path
from typing import Any

import hyphae_sdk
from hyphae_sdk.v2 import (
    AsyncHyphaeClient,
    ClientError,
    ProductError,
    RequestOptions,
)
from hyphae_sdk.v2.protocol import (
    FRAME_HEADER_SIZE,
    FRAME_KINDS,
    G6_CAPABILITIES,
    blake3,
    decode_frame,
    encode_frame,
)


_PIPE_ACCESS_DUPLEX = 0x00000003
_PIPE_TYPE_BYTE = 0x00000000
_PIPE_READMODE_BYTE = 0x00000000
_PIPE_WAIT = 0x00000000
_ERROR_BROKEN_PIPE = 109
_ERROR_NO_DATA = 232
_ERROR_PIPE_CONNECTED = 535
_ERROR_OPERATION_ABORTED = 995
_INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
_TRANSCRIPT_ENV = "HYPHAE_WINDOWS_ASYNC_TRANSCRIPT"
_EXPECTED_WHEEL_ENV = "HYPHAE_WINDOWS_ASYNC_WHEEL"
_EXPECTED_VERSION_ENV = "HYPHAE_WINDOWS_ASYNC_VERSION"
_COORDINATION_TIMEOUT_SECONDS = 5.0
_TERMINATION_TIMEOUT_SECONDS = 0.95


class _GateFailure(AssertionError):
    pass


def _kernel32() -> Any:
    if os.name != "nt":
        raise _GateFailure("Windows named-pipe gate requires Windows")
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateNamedPipeW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
    ]
    kernel32.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel32.ConnectNamedPipe.argtypes = [wintypes.HANDLE, wintypes.LPVOID]
    kernel32.ConnectNamedPipe.restype = wintypes.BOOL
    kernel32.DisconnectNamedPipe.argtypes = [wintypes.HANDLE]
    kernel32.DisconnectNamedPipe.restype = wintypes.BOOL
    kernel32.ReadFile.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    ]
    kernel32.ReadFile.restype = wintypes.BOOL
    kernel32.WriteFile.argtypes = [
        wintypes.HANDLE,
        wintypes.LPCVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    ]
    kernel32.WriteFile.restype = wintypes.BOOL
    kernel32.CancelIoEx.argtypes = [wintypes.HANDLE, wintypes.LPVOID]
    kernel32.CancelIoEx.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    return kernel32


def _welcome() -> bytes:
    return struct.pack(
        "<8sIHHQ16sIIIHHHHQQQQH",
        b"HYPWEL01",
        94,
        1,
        0,
        G6_CAPABILITIES,
        (7).to_bytes(16, "little"),
        16 * 1024 * 1024,
        64,
        64 * 1024,
        1,
        1,
        2,
        6,
        11,
        1024,
        4096,
        4096,
        0,
    )


def _capabilities_response() -> bytes:
    encoded = bytearray(72)
    encoded[:8] = b"HYPRSP01"
    struct.pack_into("<IHHHHHH", encoded, 8, len(encoded), 1, 0, 1, 1, 2, 6)
    return bytes(encoded)


def _end(payload: bytes) -> bytes:
    return struct.pack("<8sIB3xQ32s", b"HYPEND01", 56, 1, len(payload), blake3(payload))


class _NamedPipePeer:
    def __init__(self, *, stall: str, reconnect: bool) -> None:
        if stall not in {"welcome", "response"}:
            raise ValueError("invalid named-pipe stall point")
        self.endpoint = f"hyphae-async-{uuid.uuid4().hex}"
        self._pipe_name = rf"\\.\pipe\{self.endpoint}"
        self._stall = stall
        self._reconnect = reconnect
        self.ready = threading.Event()
        self.stalled = threading.Event()
        self.reconnect_ready = threading.Event()
        self.recovered = threading.Event()
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._handle: int | None = None
        self.error: BaseException | None = None
        self._thread = threading.Thread(target=self._serve, name="hyphae-win-pipe-peer")
        self._thread.start()

    def close(self) -> None:
        self._stop.set()
        deadline = time.monotonic() + _COORDINATION_TIMEOUT_SECONDS
        while self._thread.is_alive() and time.monotonic() < deadline:
            with self._lock:
                handle = self._handle
                if handle is not None:
                    kernel32 = _kernel32()
                    kernel32.CancelIoEx(ctypes.c_void_p(handle), None)
                    kernel32.DisconnectNamedPipe(ctypes.c_void_p(handle))
            self._thread.join(0.05)
        if self._thread.is_alive():
            raise _GateFailure("named-pipe peer did not stop after cancellation")
        if self.error is not None:
            raise _GateFailure("named-pipe peer failed") from self.error

    def _serve(self) -> None:
        handle: int | None = None
        try:
            handle = self._create_pipe()
            with self._lock:
                self._handle = handle
            self.ready.set()
            connections = 2 if self._reconnect else 1
            for index in range(connections):
                if self._stop.is_set():
                    break
                if index:
                    self.reconnect_ready.set()
                self._connect(handle)
                if index == 0:
                    self._serve_stalled(handle)
                else:
                    self._serve_success(handle)
                _kernel32().DisconnectNamedPipe(ctypes.c_void_p(handle))
        except BaseException as error:
            if not self._stop.is_set():
                self.error = error
        finally:
            with self._lock:
                if self._handle == handle:
                    self._handle = None
            if handle is not None:
                _kernel32().CloseHandle(ctypes.c_void_p(handle))

    def _create_pipe(self) -> int:
        handle = _kernel32().CreateNamedPipeW(
            self._pipe_name,
            _PIPE_ACCESS_DUPLEX,
            _PIPE_TYPE_BYTE | _PIPE_READMODE_BYTE | _PIPE_WAIT,
            1,
            64 * 1024,
            64 * 1024,
            0,
            None,
        )
        raw = int(handle) if handle is not None else 0
        if not raw or raw == _INVALID_HANDLE_VALUE:
            raise ctypes.WinError(ctypes.get_last_error())
        return raw

    @staticmethod
    def _connect(handle: int) -> None:
        if _kernel32().ConnectNamedPipe(ctypes.c_void_p(handle), None):
            return
        error = ctypes.get_last_error()
        if error != _ERROR_PIPE_CONNECTED:
            raise ctypes.WinError(error)

    def _serve_stalled(self, handle: int) -> None:
        hello = self._read_frame(handle)
        if hello.kind != FRAME_KINDS["hello"] or hello.stream_id != 0:
            raise _GateFailure("client did not begin with HELLO")
        if self._stall == "welcome":
            self.stalled.set()
            self._read_until_disconnect(handle)
            return
        self._write(
            handle,
            encode_frame(FRAME_KINDS["welcome"], 0, hello.request_id, _welcome()),
        )
        request = self._read_frame(handle)
        if request.kind != FRAME_KINDS["execute"] or request.stream_id == 0:
            raise _GateFailure("client did not send an EXECUTE frame")
        self.stalled.set()
        self._read_until_disconnect(handle)

    def _serve_success(self, handle: int) -> None:
        hello = self._read_frame(handle)
        if hello.kind != FRAME_KINDS["hello"] or hello.stream_id != 0:
            raise _GateFailure("reconnect contained stale bytes before HELLO")
        self._write(
            handle,
            encode_frame(FRAME_KINDS["welcome"], 0, hello.request_id, _welcome()),
        )
        request = self._read_frame(handle)
        if request.kind != FRAME_KINDS["execute"] or request.stream_id == 0:
            raise _GateFailure("reconnect contained stale bytes before EXECUTE")
        payload = _capabilities_response()
        self._write(
            handle,
            encode_frame(
                FRAME_KINDS["data"], request.stream_id, request.request_id, payload
            ),
        )
        self._write(
            handle,
            encode_frame(
                FRAME_KINDS["end"], request.stream_id, request.request_id, _end(payload)
            ),
        )
        self.recovered.set()
        self._read_until_disconnect(handle)

    def _read_frame(self, handle: int) -> Any:
        header = self._read_exact(handle, FRAME_HEADER_SIZE)
        length = struct.unpack_from("<I", header, 24)[0]
        return decode_frame(header + self._read_exact(handle, length))

    def _read_exact(self, handle: int, length: int) -> bytes:
        output = bytearray()
        while len(output) < length:
            chunk = self._read(handle, length - len(output), allow_disconnect=False)
            if not chunk:
                raise _GateFailure("named-pipe peer disconnected during a frame")
            output.extend(chunk)
        return bytes(output)

    @staticmethod
    def _read(handle: int, length: int, *, allow_disconnect: bool) -> bytes:
        from ctypes import wintypes

        buffer = ctypes.create_string_buffer(length)
        read = wintypes.DWORD()
        if _kernel32().ReadFile(
            ctypes.c_void_p(handle),
            buffer,
            length,
            ctypes.byref(read),
            None,
        ):
            return buffer.raw[: read.value]
        error = ctypes.get_last_error()
        if allow_disconnect and error in {
            _ERROR_BROKEN_PIPE,
            _ERROR_NO_DATA,
            _ERROR_OPERATION_ABORTED,
        }:
            return b""
        raise ctypes.WinError(error)

    def _read_until_disconnect(self, handle: int) -> None:
        while not self._stop.is_set() and self._read(
            handle, 256, allow_disconnect=True
        ):
            pass

    @staticmethod
    def _write(handle: int, payload: bytes) -> None:
        from ctypes import wintypes

        buffer = ctypes.create_string_buffer(payload)
        written = wintypes.DWORD()
        if not _kernel32().WriteFile(
            ctypes.c_void_p(handle),
            buffer,
            len(payload),
            ctypes.byref(written),
            None,
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        if written.value != len(payload):
            raise _GateFailure("named-pipe peer emitted a partial frame")


async def _wait_event(event: threading.Event, message: str) -> None:
    deadline = time.monotonic() + _COORDINATION_TIMEOUT_SECONDS
    while not event.is_set():
        if time.monotonic() >= deadline:
            raise _GateFailure(message)
        await asyncio.sleep(0.01)


async def _exercise(stall: str, action: str, request_id: int) -> dict[str, object]:
    reconnect = action != "aclose"
    peer = _NamedPipePeer(stall=stall, reconnect=reconnect)
    client = AsyncHyphaeClient.local(peer.endpoint)
    await _wait_event(peer.ready, "named-pipe peer did not become ready")
    operation = asyncio.create_task(
        client.execute(
            "capabilities",
            {},
            options=RequestOptions(
                request_id=request_id,
                deadline_micros=(
                    int((time.time() + 0.30) * 1_000_000)
                    if action == "deadline"
                    else None
                ),
            ),
        )
    )
    try:
        await _wait_event(peer.stalled, f"client did not reach stalled {stall}")
        started = time.monotonic()
        if action == "task_cancel":
            operation.cancel()
        elif action == "aclose":
            await asyncio.wait_for(
                client.aclose(), timeout=_TERMINATION_TIMEOUT_SECONDS
            )
        try:
            await asyncio.wait_for(
                asyncio.shield(operation), timeout=_TERMINATION_TIMEOUT_SECONDS
            )
        except asyncio.CancelledError:
            if action != "task_cancel":
                raise _GateFailure("operation returned task cancellation unexpectedly")
            error = "cancelled_task"
        except ProductError as product_error:
            expected = "deadline_exceeded" if action == "deadline" else "cancelled"
            if product_error.code != expected or product_error.request_id != request_id:
                raise _GateFailure("operation returned the wrong typed product error")
            error = product_error.code
        else:
            raise _GateFailure("interrupted operation unexpectedly succeeded")
        elapsed_millis = int((time.monotonic() - started) * 1000)
        if elapsed_millis >= 1000:
            raise _GateFailure("interrupted operation exceeded one second")
        if reconnect:
            await _wait_event(
                peer.reconnect_ready, "peer did not expose a clean reconnect"
            )
            response = await asyncio.wait_for(
                client.execute(
                    "capabilities",
                    {},
                    options=RequestOptions(request_id=request_id + 100),
                ),
                timeout=_COORDINATION_TIMEOUT_SECONDS,
            )
            if response.kind != "capabilities":
                raise _GateFailure("reconnected stream returned the wrong response")
            await _wait_event(peer.recovered, "peer did not observe the clean stream")
            await client.aclose()
            recovery = "reconnected"
        else:
            try:
                await client.execute("capabilities", {})
            except ClientError as client_error:
                if "closed" not in str(client_error):
                    raise _GateFailure(
                        "closed client returned the wrong terminal error"
                    ) from client_error
            else:
                raise _GateFailure("closed client accepted another operation")
            recovery = "terminal"
        return {
            "elapsed_millis": elapsed_millis,
            "error": error,
            "recovery": recovery,
        }
    finally:
        if not operation.done():
            operation.cancel()
        try:
            await client.aclose()
        finally:
            peer.close()


async def run_windows_async_cases() -> dict[str, dict[str, object]]:
    observations: dict[str, dict[str, object]] = {}
    request_id = 1000
    for stall in ("welcome", "response"):
        for action, suffix in (
            ("task_cancel", "task_cancel_reconnect"),
            ("deadline", "deadline_reconnect"),
            ("aclose", "aclose_terminal"),
        ):
            observations[f"{stall}_{suffix}"] = await _exercise(
                stall, action, request_id
            )
            request_id += 1000
    return observations


def _assert_installed_wheel() -> tuple[str, str]:
    expected_wheel = os.environ.get(_EXPECTED_WHEEL_ENV)
    expected_version = os.environ.get(_EXPECTED_VERSION_ENV)
    if not expected_wheel or not expected_version:
        raise _GateFailure("hosted gate did not declare its exact wheel")
    version = importlib.metadata.version("hyphae-sdk")
    if version != expected_version:
        raise _GateFailure("installed SDK version differs from exact wheel")
    origin = Path(hyphae_sdk.__file__).resolve()
    source_root = Path(__file__).resolve().parents[1] / "src"
    if origin == source_root or source_root in origin.parents:
        raise _GateFailure("hosted gate imported the source tree instead of the wheel")
    return expected_wheel, version


@unittest.skipUnless(os.name == "nt", "hosted Windows named-pipe gate")
class WindowsAsyncNamedPipeTests(unittest.TestCase):
    def test_installed_wheel_interrupts_and_reconnects_without_contamination(
        self,
    ) -> None:
        wheel, version = _assert_installed_wheel()
        observations = asyncio.run(run_windows_async_cases())
        transcript_path = os.environ.get(_TRANSCRIPT_ENV)
        if transcript_path:
            transcript = {
                "schema": "hyphae-python-windows-async-transcript-v1",
                "status": "passed",
                "platform": "windows",
                "python_version": ".".join(map(str, sys.version_info[:3])),
                "distribution": {"filename": wheel, "version": version},
                "transport": "named-pipe",
                "cases": observations,
            }
            Path(transcript_path).write_text(
                json.dumps(transcript, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
