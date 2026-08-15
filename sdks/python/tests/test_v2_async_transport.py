# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import asyncio
import errno
import http.client
import os
import socket
import ssl
import struct
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from hyphae_sdk.v2 import (
    AsyncHyphaeClient,
    ClientError,
    HttpTransport,
    LocalTransport,
    ProductError,
    RequestOptions,
)
from hyphae_sdk.v2.http import PRODUCT_MEDIA_TYPE
from hyphae_sdk.v2.protocol import (
    FRAME_HEADER_SIZE,
    FRAME_KINDS,
    G6_CAPABILITIES,
    blake3,
    decode_frame,
    encode_frame,
)


def _read_exact(connection: socket.socket, length: int) -> bytes:
    output = bytearray()
    while len(output) < length:
        chunk = connection.recv(length - len(output))
        if not chunk:
            raise ConnectionError("peer closed")
        output.extend(chunk)
    return bytes(output)


def _read_frame(connection: socket.socket):  # type: ignore[no-untyped-def]
    header = _read_exact(connection, FRAME_HEADER_SIZE)
    length = struct.unpack_from("<I", header, 24)[0]
    return decode_frame(header + _read_exact(connection, length))


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
    return struct.pack(
        "<8sIB3xQ32s",
        b"HYPEND01",
        56,
        1,
        len(payload),
        blake3(payload),
    )


class _RetainingHttpPeer:
    def __init__(self, *, response_headers: bool, slow_body: bool = False) -> None:
        self._response_headers = response_headers
        self._slow_body = slow_body
        self.started = threading.Event()
        self.peer_closed = threading.Event()
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(1)
        host, port = self._listener.getsockname()
        self.origin = f"http://{host}:{port}"
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def close(self) -> None:
        self._listener.close()
        self._thread.join(1)

    def _serve(self) -> None:
        connection, _ = self._listener.accept()
        try:
            request = self._read_request(connection)
            request_id = self._request_id(request)
            if self._response_headers:
                body = _capabilities_response()
                connection.sendall(
                    (
                        "HTTP/1.1 200 OK\r\n"
                        f"Content-Type: {PRODUCT_MEDIA_TYPE}\r\n"
                        f"Content-Length: {len(body)}\r\n"
                        f"X-Hyphae-Request-Id: {request_id}\r\n"
                        "Connection: close\r\n\r\n"
                    ).encode()
                )
                if self._slow_body:
                    for byte in body:
                        connection.sendall(bytes((byte,)))
                        self.started.set()
                        time.sleep(0.04)
                    return
                connection.sendall(body[:1])
            self.started.set()
            while connection.recv(1):
                pass
            self.peer_closed.set()
        except (BrokenPipeError, ConnectionError, OSError):
            self.peer_closed.set()
        finally:
            connection.close()

    @staticmethod
    def _read_request(connection: socket.socket) -> bytes:
        request = bytearray()
        while b"\r\n\r\n" not in request:
            request.extend(_read_exact(connection, 1))
        head, body = bytes(request).split(b"\r\n\r\n", 1)
        content_length = next(
            int(line.split(b":", 1)[1])
            for line in head.split(b"\r\n")
            if line.lower().startswith(b"content-length:")
        )
        return (
            head
            + b"\r\n\r\n"
            + body
            + _read_exact(connection, content_length - len(body))
        )

    @staticmethod
    def _request_id(request: bytes) -> str:
        return next(
            line.split(b":", 1)[1].strip().decode()
            for line in request.split(b"\r\n")
            if line.lower().startswith(b"x-hyphae-request-id:")
        )


class _BarrierHttpConnection:
    entered = threading.Event()
    release = threading.Event()
    dispatched = threading.Event()

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.sock = None
        self.auto_open = 1

    def connect(self) -> None:
        pass

    def request(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        type(self).entered.set()
        type(self).release.wait(1)
        if self.auto_open:
            type(self).dispatched.set()
            return
        raise http.client.NotConnected()

    def getresponse(self):  # type: ignore[no-untyped-def]
        raise AssertionError("cancelled request reached getresponse")

    def close(self) -> None:
        self.sock = None


class _ConnectBarrierHttpConnection:
    entered = threading.Event()
    release = threading.Event()
    dispatched = threading.Event()

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.sock = None
        self.auto_open = 1

    def connect(self) -> None:
        type(self).entered.set()
        type(self).release.wait(1)

    def request(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        type(self).dispatched.set()

    def getresponse(self):  # type: ignore[no-untyped-def]
        raise AssertionError("cancelled request reached getresponse")

    def close(self) -> None:
        self.sock = None


class _DialBarrierSocket:
    entered = threading.Event()
    release = threading.Event()
    request_bytes = threading.Event()
    block_connect = True

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.timeout: float | None = None

    def setblocking(self, blocking: bool) -> None:
        del blocking

    def connect_ex(self, address: object) -> int:
        del address
        if type(self).block_connect:
            type(self).entered.set()
            return errno.EINPROGRESS
        return 0

    def getsockopt(self, level: int, option: int) -> int:
        del level, option
        return 0

    def settimeout(self, timeout: float) -> None:
        self.timeout = timeout

    def sendall(self, encoded: bytes) -> None:
        del encoded
        type(self).request_bytes.set()

    def shutdown(self, how: int) -> None:
        del how
        type(self).release.set()

    def close(self) -> None:
        type(self).release.set()


class _TlsBarrierSocket(_DialBarrierSocket):
    entered = threading.Event()
    release = threading.Event()
    request_bytes = threading.Event()

    def do_handshake(self) -> None:
        type(self).entered.set()
        raise ssl.SSLWantReadError()


def _dial_barrier_select(read, write, exceptional, timeout):  # type: ignore[no-untyped-def]
    del exceptional
    selected = read or write
    if not selected:
        return (), (), ()
    current_socket = selected[0]
    current_socket.release.wait(timeout)
    if not current_socket.release.is_set():
        return (), (), ()
    return (read, (), ()) if read else ((), write, ())


def _loopback_address(*args, **kwargs):  # type: ignore[no-untyped-def]
    del args, kwargs
    return [(socket.AF_INET, socket.SOCK_STREAM, 0, "", ("127.0.0.1", 8787))]


def _tls_barrier_wrap(*args, **kwargs):  # type: ignore[no-untyped-def]
    del args, kwargs
    return _TlsBarrierSocket()


class HttpAbortTests(unittest.TestCase):
    def test_abort_interrupts_getresponse_before_headers(self) -> None:
        self._assert_abort_interrupts(response_headers=False)

    def test_abort_interrupts_body_after_connection_close_headers(self) -> None:
        self._assert_abort_interrupts(response_headers=True)

    def test_close_interrupts_active_request_and_is_terminal(self) -> None:
        peer = _RetainingHttpPeer(response_headers=False)
        self.addCleanup(peer.close)
        transport = HttpTransport(peer.origin)
        errors: list[BaseException] = []
        worker = threading.Thread(
            target=self._execute_http,
            args=(transport, RequestOptions(request_id=18), errors),
        )
        worker.start()
        self.assertTrue(peer.started.wait(1))
        transport.close()
        worker.join(0.5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertRegex(str(errors[0]), "closed")
        self.assertTrue(peer.peer_closed.wait(0.5))

    @patch("http.client.HTTPConnection", _BarrierHttpConnection)
    def test_cancel_before_connect_disables_automatic_reopen(self) -> None:
        _BarrierHttpConnection.entered.clear()
        _BarrierHttpConnection.release.clear()
        _BarrierHttpConnection.dispatched.clear()
        transport = HttpTransport("http://127.0.0.1:8787")
        options = RequestOptions(request_id=21)
        errors: list[BaseException] = []
        worker = threading.Thread(
            target=self._execute_http,
            args=(transport, options, errors),
        )
        worker.start()
        self.assertTrue(_BarrierHttpConnection.entered.wait(1))
        options.cancellation.cancel()
        _BarrierHttpConnection.release.set()
        worker.join(0.5)
        self.assertFalse(worker.is_alive())
        self.assertFalse(_BarrierHttpConnection.dispatched.is_set())
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], ProductError)
        transport.close()

    @patch("http.client.HTTPConnection", _ConnectBarrierHttpConnection)
    def test_cancel_during_connect_never_dispatches_request_bytes(self) -> None:
        _ConnectBarrierHttpConnection.entered.clear()
        _ConnectBarrierHttpConnection.release.clear()
        _ConnectBarrierHttpConnection.dispatched.clear()
        transport = HttpTransport("http://127.0.0.1:8787")
        options = RequestOptions(request_id=22)
        errors: list[BaseException] = []
        worker = threading.Thread(
            target=self._execute_http,
            args=(transport, options, errors),
        )
        worker.start()
        self.assertTrue(_ConnectBarrierHttpConnection.entered.wait(1))
        options.cancellation.cancel()
        _ConnectBarrierHttpConnection.release.set()
        worker.join(0.5)
        self.assertFalse(worker.is_alive())
        self.assertFalse(_ConnectBarrierHttpConnection.dispatched.is_set())
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], ProductError)
        self.assertEqual(errors[0].code, "cancelled")  # type: ignore[union-attr]
        transport.close()

    def _assert_abort_interrupts(self, *, response_headers: bool) -> None:
        peer = _RetainingHttpPeer(response_headers=response_headers)
        self.addCleanup(peer.close)
        transport = HttpTransport(peer.origin)
        options = RequestOptions(request_id=17)
        errors: list[BaseException] = []
        worker = threading.Thread(
            target=self._execute_http,
            args=(transport, options, errors),
        )
        worker.start()
        self.assertTrue(peer.started.wait(1))
        options.cancellation.cancel()
        worker.join(0.5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], ProductError)
        self.assertEqual(errors[0].code, "cancelled")  # type: ignore[union-attr]
        self.assertTrue(peer.peer_closed.wait(0.5))
        transport.close()

    def test_absolute_deadline_interrupts_slow_drip_body(self) -> None:
        peer = _RetainingHttpPeer(response_headers=True, slow_body=True)
        self.addCleanup(peer.close)
        transport = HttpTransport(peer.origin, timeout_seconds=2)
        options = RequestOptions(
            request_id=19,
            deadline_micros=time.time_ns() // 1000 + 150_000,
        )
        started_at = time.monotonic()
        with self.assertRaises(ProductError) as caught:
            transport.execute("capabilities", {}, options)
        self.assertEqual(caught.exception.code, "deadline_exceeded")
        self.assertLess(time.monotonic() - started_at, 0.6)
        transport.close()

    @staticmethod
    def _execute_http(
        transport: HttpTransport,
        options: RequestOptions,
        errors: list[BaseException],
    ) -> None:
        try:
            transport.execute("capabilities", {}, options)
        except BaseException as error:
            errors.append(error)


class HttpAsyncConnectAbortTests(unittest.IsolatedAsyncioTestCase):
    async def test_task_cancel_interrupts_tcp_connect_without_request_bytes(
        self,
    ) -> None:
        self._reset_dial(block_connect=True)
        with (
            patch("hyphae_sdk.v2.http.socket.getaddrinfo", _loopback_address),
            patch("hyphae_sdk.v2.http.socket.socket", _DialBarrierSocket),
            patch("hyphae_sdk.v2.http.select.select", _dial_barrier_select),
        ):
            async with AsyncHyphaeClient.http("http://127.0.0.1:8787") as client:
                operation = asyncio.create_task(client.execute("capabilities"))
                await self._wait_for(_DialBarrierSocket.entered)
                operation.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await asyncio.wait_for(operation, 0.5)
        self.assertFalse(_DialBarrierSocket.request_bytes.is_set())

    async def test_aclose_interrupts_tls_handshake_without_request_bytes(self) -> None:
        self._reset_dial(block_connect=False)
        with (
            patch("hyphae_sdk.v2.http.socket.getaddrinfo", _loopback_address),
            patch("hyphae_sdk.v2.http.socket.socket", _DialBarrierSocket),
            patch("hyphae_sdk.v2.http.select.select", _dial_barrier_select),
            patch("hyphae_sdk.v2.http._wrap_tls_socket", _tls_barrier_wrap),
        ):
            client = AsyncHyphaeClient.http("https://127.0.0.1:8787")
            operation = asyncio.create_task(client.execute("capabilities"))
            await self._wait_for(_TlsBarrierSocket.entered)
            await asyncio.wait_for(client.aclose(), 0.5)
            with self.assertRaises((ClientError, ProductError)) as caught:
                await operation
            if isinstance(caught.exception, ProductError):
                self.assertEqual(caught.exception.code, "cancelled")
        self.assertFalse(_TlsBarrierSocket.request_bytes.is_set())

    @staticmethod
    def _reset_dial(*, block_connect: bool) -> None:
        _DialBarrierSocket.block_connect = block_connect
        for current in (_DialBarrierSocket, _TlsBarrierSocket):
            current.entered.clear()
            current.release.clear()
            current.request_bytes.clear()

    @staticmethod
    async def _wait_for(event: threading.Event) -> None:
        for _ in range(100):
            if event.is_set():
                return
            await asyncio.sleep(0.01)
        raise AssertionError("HTTP connect did not reach the expected state")


class _DelayedAbortLocalTransport(LocalTransport):
    def __init__(
        self,
        endpoint: str,
        abort_captured: threading.Event,
        release_abort: threading.Event,
    ) -> None:
        super().__init__(endpoint)
        self._abort_captured = abort_captured
        self._release_abort = release_abort

    def _interrupt_detached(self, context, stream) -> None:  # type: ignore[no-untyped-def]
        if context is not None and context.generation == 1:
            self._abort_captured.set()
            self._release_abort.wait(1)
        super()._interrupt_detached(context, stream)


class _NoConnectLocalTransport(LocalTransport):
    def __init__(self) -> None:
        super().__init__("unused")
        self.connect_attempted = False

    def _connect(self, request_id, options=None) -> None:  # type: ignore[no-untyped-def]
        del request_id, options
        self.connect_attempted = True
        raise AssertionError("expired request attempted to connect")


class _ConnectBarrierSocket:
    entered = threading.Event()
    release = threading.Event()
    hello_written = threading.Event()

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs

    def setblocking(self, blocking: bool) -> None:
        del blocking

    def connect_ex(self, endpoint: str) -> int:
        del endpoint
        type(self).entered.set()
        return errno.EINPROGRESS

    def getsockopt(self, level: int, option: int) -> int:
        del level, option
        return 0

    def settimeout(self, timeout: float) -> None:
        del timeout

    def sendall(self, encoded: bytes) -> None:
        del encoded
        type(self).hello_written.set()

    def shutdown(self, how: int) -> None:
        del how
        type(self).release.set()

    def close(self) -> None:
        type(self).release.set()


def _connect_barrier_select(read, write, exceptional, timeout):  # type: ignore[no-untyped-def]
    del read, exceptional
    _ConnectBarrierSocket.release.wait(timeout)
    return (), write if _ConnectBarrierSocket.release.is_set() else (), ()


@unittest.skipIf(os.name == "nt", "AF_UNIX generation race runs on POSIX")
class LocalGenerationTests(unittest.TestCase):
    def test_missing_endpoint_is_a_typed_client_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            transport = LocalTransport(os.path.join(directory, "missing.sock"))
            with self.assertRaisesRegex(ClientError, "endpoint connection failed"):
                transport.execute(
                    "capabilities",
                    {},
                    RequestOptions(request_id=34),
                )
            transport.close()

    def test_preexpired_deadline_never_connects_or_writes(self) -> None:
        transport = _NoConnectLocalTransport()
        with self.assertRaises(ProductError) as caught:
            transport.execute(
                "capabilities",
                {},
                RequestOptions(request_id=30, deadline_micros=1),
            )
        self.assertEqual(caught.exception.code, "deadline_exceeded")
        self.assertFalse(transport.connect_attempted)
        transport.close()

    @patch("hyphae_sdk.v2.local.select.select", _connect_barrier_select)
    @patch("hyphae_sdk.v2.local.socket.socket", _ConnectBarrierSocket)
    def test_cancel_during_connect_interrupts_without_sending_hello(self) -> None:
        self._assert_connect_interrupted(cancel=True)

    @patch("hyphae_sdk.v2.local.select.select", _connect_barrier_select)
    @patch("hyphae_sdk.v2.local.socket.socket", _ConnectBarrierSocket)
    def test_deadline_during_connect_interrupts_without_sending_hello(self) -> None:
        self._assert_connect_interrupted(cancel=False)

    def _assert_connect_interrupted(self, *, cancel: bool) -> None:
        _ConnectBarrierSocket.entered.clear()
        _ConnectBarrierSocket.release.clear()
        _ConnectBarrierSocket.hello_written.clear()
        transport = LocalTransport("unused")
        options = RequestOptions(
            request_id=33,
            deadline_micros=(None if cancel else time.time_ns() // 1000 + 100_000),
        )
        errors: list[BaseException] = []
        worker = threading.Thread(
            target=self._execute_local,
            args=(transport, options, errors, None),
        )
        worker.start()
        self.assertTrue(_ConnectBarrierSocket.entered.wait(1))
        if cancel:
            options.cancellation.cancel()
        worker.join(0.5)
        self.assertFalse(worker.is_alive())
        self.assertFalse(_ConnectBarrierSocket.hello_written.is_set())
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], ProductError)
        expected = "cancelled" if cancel else "deadline_exceeded"
        self.assertEqual(errors[0].code, expected)  # type: ignore[union-attr]
        transport.close()

    def test_late_abort_of_previous_generation_cannot_close_next_stream(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            ready = threading.Event()
            first_request = threading.Event()
            allow_first_close = threading.Event()
            second_request = threading.Event()
            allow_second_response = threading.Event()
            server = threading.Thread(
                target=self._serve_generation_race,
                args=(
                    endpoint,
                    ready,
                    first_request,
                    allow_first_close,
                    second_request,
                    allow_second_response,
                ),
            )
            server.start()
            self.assertTrue(ready.wait(1))
            abort_captured = threading.Event()
            release_abort = threading.Event()
            transport = _DelayedAbortLocalTransport(
                endpoint,
                abort_captured,
                release_abort,
            )
            first_options = RequestOptions(request_id=31)
            first_errors: list[BaseException] = []
            first = threading.Thread(
                target=self._execute_local,
                args=(transport, first_options, first_errors, None),
            )
            first.start()
            self.assertTrue(first_request.wait(1))
            cancellation = threading.Thread(target=first_options.cancellation.cancel)
            cancellation.start()
            self.assertTrue(abort_captured.wait(1))
            allow_first_close.set()
            first.join(0.5)
            self.assertFalse(first.is_alive())

            second_responses: list[object] = []
            second = threading.Thread(
                target=self._execute_local,
                args=(
                    transport,
                    RequestOptions(request_id=32),
                    [],
                    second_responses,
                ),
            )
            second.start()
            self.assertTrue(second_request.wait(1))
            release_abort.set()
            cancellation.join(0.5)
            allow_second_response.set()
            second.join(0.5)
            self.assertFalse(second.is_alive())
            self.assertEqual(len(second_responses), 1)
            self.assertEqual(second_responses[0].kind, "capabilities")
            self.assertEqual(len(first_errors), 1)
            self.assertIsInstance(first_errors[0], ProductError)
            transport.close()
            server.join(1)
            self.assertFalse(server.is_alive())

    @staticmethod
    def _execute_local(
        transport: LocalTransport,
        options: RequestOptions,
        errors: list[BaseException],
        responses: list[object] | None,
    ) -> None:
        try:
            response = transport.execute("capabilities", {}, options)
            if responses is not None:
                responses.append(response)
        except BaseException as error:
            errors.append(error)

    @staticmethod
    def _serve_generation_race(
        endpoint: str,
        ready: threading.Event,
        first_request: threading.Event,
        allow_first_close: threading.Event,
        second_request: threading.Event,
        allow_second_response: threading.Event,
    ) -> None:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(endpoint)
        listener.listen(2)
        ready.set()
        try:
            first, _ = listener.accept()
            with first:
                LocalGenerationTests._welcome_and_read_request(first)
                first_request.set()
                allow_first_close.wait(1)
            second, _ = listener.accept()
            with second:
                request = LocalGenerationTests._welcome_and_read_request(second)
                second_request.set()
                allow_second_response.wait(1)
                payload = _capabilities_response()
                second.sendall(
                    encode_frame(
                        FRAME_KINDS["data"],
                        request.stream_id,
                        request.request_id,
                        payload,
                    )
                )
                second.sendall(
                    encode_frame(
                        FRAME_KINDS["end"],
                        request.stream_id,
                        request.request_id,
                        _end(payload),
                    )
                )
        finally:
            listener.close()

    @staticmethod
    def _welcome_and_read_request(connection: socket.socket):  # type: ignore[no-untyped-def]
        hello = _read_frame(connection)
        connection.sendall(
            encode_frame(
                FRAME_KINDS["welcome"],
                0,
                hello.request_id,
                _welcome(),
            )
        )
        return _read_frame(connection)


@unittest.skipIf(os.name == "nt", "AF_UNIX cancellation runs on POSIX")
class LocalAbortTests(unittest.IsolatedAsyncioTestCase):
    async def test_cancelled_operation_disconnects_and_next_operation_reconnects(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            first_request = threading.Event()
            ready = threading.Event()
            server = threading.Thread(
                target=self._serve_two_connections,
                args=(endpoint, ready, first_request),
            )
            server.start()
            self.assertTrue(ready.wait(1))
            async with AsyncHyphaeClient.local(endpoint) as client:
                cancelled = asyncio.create_task(client.execute("capabilities"))
                await self._wait_for(first_request)
                cancelled.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await cancelled
                response = await client.execute(
                    "capabilities",
                    options=RequestOptions(request_id=23),
                )
                self.assertEqual(response.kind, "capabilities")
            server.join(1)
            self.assertFalse(server.is_alive())

    def test_deadline_is_preserved_during_stalled_handshake(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            ready = threading.Event()
            observed = threading.Event()
            server = threading.Thread(
                target=self._serve_stalled_handshake,
                args=(endpoint, ready, observed),
            )
            server.start()
            self.assertTrue(ready.wait(1))
            transport = LocalTransport(endpoint)
            started_at = time.monotonic()
            with self.assertRaises(ProductError) as caught:
                transport.execute(
                    "capabilities",
                    {},
                    RequestOptions(
                        request_id=29,
                        deadline_micros=time.time_ns() // 1000 + 150_000,
                    ),
                )
            self.assertEqual(caught.exception.code, "deadline_exceeded")
            self.assertEqual(caught.exception.request_id, 29)
            self.assertLess(time.monotonic() - started_at, 0.5)
            transport.close()
            server.join(1)
            self.assertFalse(server.is_alive())
            self.assertTrue(observed.is_set())

    @staticmethod
    async def _wait_for(event: threading.Event) -> None:
        for _ in range(100):
            if event.is_set():
                return
            await asyncio.sleep(0.01)
        raise AssertionError("server did not observe the request")

    @staticmethod
    def _serve_two_connections(
        endpoint: str,
        ready: threading.Event,
        first_request: threading.Event,
    ) -> None:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(endpoint)
        listener.listen(2)
        ready.set()
        try:
            first, _ = listener.accept()
            with first:
                hello = _read_frame(first)
                first.sendall(
                    encode_frame(
                        FRAME_KINDS["welcome"],
                        0,
                        hello.request_id,
                        _welcome(),
                    )
                )
                _read_frame(first)
                first_request.set()
                while first.recv(1):
                    pass
            second, _ = listener.accept()
            with second:
                hello = _read_frame(second)
                second.sendall(
                    encode_frame(
                        FRAME_KINDS["welcome"],
                        0,
                        hello.request_id,
                        _welcome(),
                    )
                )
                request = _read_frame(second)
                payload = _capabilities_response()
                second.sendall(
                    encode_frame(
                        FRAME_KINDS["data"],
                        request.stream_id,
                        request.request_id,
                        payload,
                    )
                )
                second.sendall(
                    encode_frame(
                        FRAME_KINDS["end"],
                        request.stream_id,
                        request.request_id,
                        _end(payload),
                    )
                )
        finally:
            listener.close()

    @staticmethod
    def _serve_stalled_handshake(
        endpoint: str,
        ready: threading.Event,
        observed: threading.Event,
    ) -> None:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(endpoint)
        listener.listen(1)
        ready.set()
        try:
            connection, _ = listener.accept()
            with connection:
                _read_frame(connection)
                observed.set()
                while connection.recv(1):
                    pass
        finally:
            listener.close()


if __name__ == "__main__":
    unittest.main()
