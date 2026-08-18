# SPDX-License-Identifier: Apache-2.0
"""Binary product-envelope HTTP /v2 transport."""

from __future__ import annotations

import errno
import http.client
import ipaddress
import select
import socket
import ssl
import threading
import time
import urllib.parse

from .models import (
    CancellationToken,
    ClientError,
    ProductError,
    ProductErrorFields,
    RequestOptions,
    Response,
    product_error,
)
from .protocol import (
    decode_product_error,
    decode_product_response,
    encode_product_request,
)

PRODUCT_MEDIA_TYPE = "application/vnd.hyphae.product-v1"
ERROR_MEDIA_TYPE = "application/vnd.hyphae.error-v1"
PROTOCOL_MINOR = "3"
_STANDARD_HTTP_CONNECTION = http.client.HTTPConnection
_STANDARD_HTTPS_CONNECTION = http.client.HTTPSConnection
_CONNECT_PENDING = {
    errno.EINPROGRESS,
    errno.EALREADY,
    errno.EWOULDBLOCK,
    errno.EAGAIN,
    errno.EINTR,
}


def _noop() -> None:
    pass


class _HttpRequestContext:
    def __init__(self, cancellation: CancellationToken) -> None:
        self.cancellation = cancellation
        self.aborted = False
        self.deadline_expired = False


class HttpTransport:
    """Bounded HTTP `/v2/execute` transport carrying canonical binary envelopes."""

    abort_invalidates_session = False

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout_seconds: float = 60.0,
        response_bytes: int = 16 * 1024 * 1024,
    ) -> None:
        parsed = urllib.parse.urlsplit(base_url)
        if (
            parsed.scheme not in {"http", "https"}
            or not parsed.netloc
            or parsed.username is not None
            or parsed.password is not None
            or parsed.path not in {"", "/"}
            or parsed.query
            or parsed.fragment
        ):
            raise ClientError("base URL must be a root HTTP(S) origin")
        if timeout_seconds <= 0 or not 0 < response_bytes <= 16 * 1024 * 1024:
            raise ClientError("HTTP timeout and response bound must be positive")
        if bearer_token is not None:
            if parsed.scheme == "http" and not _is_loopback_host(parsed.hostname):
                raise ClientError("durable API keys require HTTPS outside loopback")
            if not bearer_token or "\r" in bearer_token or "\n" in bearer_token:
                raise ClientError("invalid bearer token")
        self._parsed = parsed
        self._bearer_token = (
            bytearray(bearer_token, "utf-8") if bearer_token is not None else None
        )
        self._managed = bearer_token is not None
        self._timeout_seconds = timeout_seconds
        self._response_bytes = response_bytes
        self._session_id: str | None = None
        self._closed = False
        self._state_lock = threading.Lock()
        self._active_connections: dict[
            http.client.HTTPConnection, _HttpRequestContext
        ] = {}
        self._active_sockets: dict[socket.socket, _HttpRequestContext] = {}

    def __repr__(self) -> str:
        authentication = "bearer" if self._managed else "none"
        origin = f"{self._parsed.scheme}://{self._parsed.netloc}"
        return f"HttpTransport(base_url={origin!r}, authentication={authentication!r})"

    def __enter__(self) -> HttpTransport:
        with self._state_lock:
            if self._closed:
                raise ClientError("HTTP transport is closed")
        return self

    def __exit__(self, *exc_info: object) -> None:
        del exc_info
        self.close()

    def close(self) -> None:
        with self._state_lock:
            if self._closed:
                return
            self._closed = True
            self._session_id = None
            credential, self._bearer_token = self._bearer_token, None
            contexts = set(self._active_connections.values()) | set(
                self._active_sockets.values()
            )
            for context in contexts:
                context.aborted = True
            connections = tuple(self._active_connections)
            sockets = tuple(self._active_sockets)
        if credential is not None:
            credential[:] = b"\0" * len(credential)
        _abort_http_handles(connections, sockets)

    def abort(self, cancellation: CancellationToken | None = None) -> None:
        """Interrupt matching requests while preserving managed session identity."""

        with self._state_lock:
            contexts = {
                context
                for context in (
                    *self._active_connections.values(),
                    *self._active_sockets.values(),
                )
                if cancellation is None or context.cancellation is cancellation
            }
            for context in contexts:
                context.aborted = True
            connections = tuple(
                connection
                for connection, context in self._active_connections.items()
                if context in contexts
            )
            sockets = tuple(
                current_socket
                for current_socket, context in self._active_sockets.items()
                if context in contexts
            )
        _abort_http_handles(connections, sockets)

    def __del__(self) -> None:
        try:
            self.close()
        except (AttributeError, OSError):
            pass

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        with self._state_lock:
            if self._closed:
                raise ClientError("HTTP transport is closed")
        request_id = options.checked_request_id()
        if options.cancellation.cancelled:
            raise product_error("cancelled", request_id)
        deadline_at = (
            options.deadline_micros / 1_000_000
            if options.deadline_micros is not None
            else None
        )
        body = encode_product_request(
            operation, arguments, options, negotiated_minor=int(PROTOCOL_MINOR)
        )
        key_lifecycle = operation.startswith("security_api_key_") or operation == (
            "security_legacy_bearer_revoke"
        )
        one_time_secret = operation.startswith("security_api_key_") and operation.endswith(
            "_start"
        )
        timeout_seconds = self._timeout_seconds
        if deadline_at is not None:
            remaining = deadline_at - time.time()
            if remaining <= 0:
                raise product_error("deadline_exceeded", request_id)
            timeout_seconds = min(timeout_seconds, remaining)
        context = _HttpRequestContext(options.cancellation)
        headers = {
            "Accept": f"{PRODUCT_MEDIA_TYPE}, {ERROR_MEDIA_TYPE}",
            "Content-Type": PRODUCT_MEDIA_TYPE,
            "Content-Length": str(len(body)),
            "X-Hyphae-Protocol-Minor": PROTOCOL_MINOR,
            "X-Hyphae-Request-Id": str(request_id),
        }
        if options.deadline_micros is not None:
            headers["X-Hyphae-Deadline-Micros"] = str(options.deadline_micros)
        connection_type = (
            http.client.HTTPSConnection
            if self._parsed.scheme == "https"
            else http.client.HTTPConnection
        )
        connection = connection_type(
            self._parsed.hostname,
            self._parsed.port,
            timeout=timeout_seconds,
        )
        with self._state_lock:
            if self._closed:
                connection.close()
                raise ClientError("HTTP transport is closed")
            if options.cancellation.cancelled:
                connection.close()
                raise product_error("cancelled", request_id)
            if self._bearer_token is not None:
                headers["Authorization"] = "Bearer " + self._bearer_token.decode(
                    "utf-8"
                )
            if self._session_id is not None:
                headers["X-Hyphae-Session-Id"] = self._session_id
            self._active_connections[connection] = context
        unsubscribe_cancellation = _noop
        deadline_timer: threading.Timer | None = None
        response: http.client.HTTPResponse | None = None
        try:
            unsubscribe_cancellation = options.cancellation._subscribe(
                lambda: self.abort(options.cancellation)
            )
            if deadline_at is not None:
                deadline_timer = threading.Timer(
                    max(0.0, deadline_at - time.time()),
                    self._expire_context,
                    args=(context,),
                )
                deadline_timer.daemon = True
                deadline_timer.start()
            if isinstance(
                connection,
                (_STANDARD_HTTP_CONNECTION, _STANDARD_HTTPS_CONNECTION),
            ):
                self._connect_socket(
                    connection,
                    context,
                    request_id,
                    deadline_at,
                    timeout_seconds,
                )
            else:
                connection.connect()
            with self._state_lock:
                interrupted = self._closed or context.aborted
                connected_socket = connection.sock
                if connected_socket is not None and not interrupted:
                    self._active_sockets[connected_socket] = context
            if interrupted:
                _abort_connection(connection)
                raise self._interrupted_error(context, request_id)
            connection.auto_open = 0
            connection.request(
                "POST",
                "/v2/security/keys" if key_lifecycle else "/v2/execute",
                body=body,
                headers=headers,
            )
            with self._state_lock:
                interrupted = self._closed or context.aborted
            if interrupted:
                _abort_connection(connection)
                raise self._interrupted_error(context, request_id)
            response = connection.getresponse()
            selected_minor = response.getheader("X-Hyphae-Protocol-Minor")
            response_request_id = response.getheader("X-Hyphae-Request-Id")
            session_id = response.getheader("X-Hyphae-Session-Id")
            if selected_minor != PROTOCOL_MINOR:
                raise ClientError("HTTP v2 protocol minor is missing or unsupported")
            if response_request_id != str(request_id):
                raise ClientError("HTTP v2 response request ID mismatch")
            if session_id is not None and (
                len(session_id) != 32
                or any(character not in "0123456789abcdef" for character in session_id)
                or session_id == "0" * 32
            ):
                raise ClientError("HTTP v2 response session ID is invalid")
            if one_time_secret and 200 <= response.status < 300 and (
                response.getheader("Cache-Control")
                != "no-store, private, max-age=0"
                or response.getheader("Pragma") != "no-cache"
                or response.getheader("Content-Encoding") is not None
            ):
                raise ClientError("HTTP API-key secret response is not cache-safe")
            response_socket = _response_socket(response)
            with self._state_lock:
                interrupted = self._closed or context.aborted
                if response_socket is not None and not interrupted:
                    self._active_sockets[response_socket] = context
            if interrupted:
                if response_socket is not None:
                    _abort_socket(response_socket)
                raise self._interrupted_error(context, request_id)
            if session_id is not None:
                with self._state_lock:
                    if not self._closed:
                        self._session_id = session_id
            declared = response.getheader("Content-Length")
            maximum = min(self._response_bytes, options.limits["max_response_bytes"])
            if declared is not None and (
                not declared.isascii()
                or not declared.isdigit()
                or int(declared) > maximum
            ):
                raise ClientError("HTTP v2 response exceeds the configured maximum")
            encoded = response.read(maximum + 1)
            with self._state_lock:
                interrupted = self._closed or context.aborted
            if interrupted:
                raise self._interrupted_error(context, request_id)
            if len(encoded) > maximum:
                raise ClientError("HTTP v2 response exceeds the configured maximum")
            if declared is not None and len(encoded) != int(declared):
                raise ClientError("HTTP v2 response length differs from Content-Length")
            media_type = (
                (response.getheader("Content-Type") or "")
                .split(";", 1)[0]
                .strip()
                .lower()
            )
            if 200 <= response.status < 300:
                if response.status != 200 or media_type != PRODUCT_MEDIA_TYPE:
                    raise ClientError(
                        "HTTP v2 returned an unexpected status or media type"
                    )
                return decode_product_response(
                    encoded, request_id, negotiated_minor=int(PROTOCOL_MINOR)
                )
            if media_type == ERROR_MEDIA_TYPE:
                raise ProductError(
                    decode_product_error(encoded), status=response.status
                )
            if media_type == "application/json":
                raise ProductError(_decode_json_error(encoded), status=response.status)
            raise ClientError("HTTP v2 failure did not contain a typed product error")
        except (OSError, http.client.HTTPException) as error:
            if context.deadline_expired or (
                options.deadline_micros is not None
                and time.time_ns() // 1000 >= options.deadline_micros
            ):
                raise product_error("deadline_exceeded", request_id) from error
            if options.cancellation.cancelled:
                raise product_error("cancelled", request_id) from error
            with self._state_lock:
                closed = self._closed
            if closed:
                raise ClientError("HTTP transport is closed") from error
            raise ClientError("Hyphae HTTP v2 transport failed") from error
        finally:
            if deadline_timer is not None:
                deadline_timer.cancel()
            unsubscribe_cancellation()
            with self._state_lock:
                self._active_connections.pop(connection, None)
                for current_socket, active in tuple(self._active_sockets.items()):
                    if active is context:
                        self._active_sockets.pop(current_socket, None)
            if response is not None:
                close_response = getattr(response, "close", None)
                if close_response is not None:
                    try:
                        close_response()
                    except (OSError, ValueError):
                        pass
            connection.close()

    def _connect_socket(
        self,
        connection: http.client.HTTPConnection,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
        timeout_seconds: float,
    ) -> None:
        host = self._parsed.hostname
        assert host is not None
        port = self._parsed.port or (443 if self._parsed.scheme == "https" else 80)
        try:
            addresses = socket.getaddrinfo(
                host,
                port,
                type=socket.SOCK_STREAM,
            )
        except OSError as error:
            self._check_connect_state(context, request_id, deadline_at)
            raise ClientError("Hyphae HTTP endpoint resolution failed") from error
        self._check_connect_state(context, request_id, deadline_at)
        last_error: OSError | None = None
        connect_expires = time.monotonic() + timeout_seconds
        for family, socket_type, protocol, _, address in addresses:
            current_socket = socket.socket(family, socket_type, protocol)
            try:
                self._register_socket(current_socket, context, request_id, deadline_at)
                self._connect_one(
                    current_socket,
                    address,
                    context,
                    request_id,
                    deadline_at,
                    connect_expires,
                )
                if isinstance(connection, _STANDARD_HTTPS_CONNECTION):
                    wrapped = _wrap_tls_socket(connection, current_socket, host)
                    self._replace_socket(
                        current_socket,
                        wrapped,
                        context,
                        request_id,
                        deadline_at,
                    )
                    current_socket = wrapped
                    self._handshake_tls(
                        current_socket,
                        context,
                        request_id,
                        deadline_at,
                        connect_expires,
                    )
                current_socket.settimeout(
                    self._remaining_timeout(deadline_at, timeout_seconds)
                )
                connection.sock = current_socket
                return
            except (ClientError, ProductError):
                self._drop_socket(current_socket, context)
                _abort_socket(current_socket)
                raise
            except (OSError, ValueError) as error:
                self._drop_socket(current_socket, context)
                _abort_socket(current_socket)
                self._check_connect_state(context, request_id, deadline_at)
                last_error = (
                    error if isinstance(error, OSError) else OSError(str(error))
                )
        raise ClientError("Hyphae HTTP endpoint connection failed") from last_error

    def _connect_one(
        self,
        current_socket: socket.socket,
        address: object,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
        connect_expires: float,
    ) -> None:
        current_socket.setblocking(False)
        result = current_socket.connect_ex(address)  # type: ignore[arg-type]
        if result not in {0, errno.EISCONN, *_CONNECT_PENDING}:
            raise OSError(result, "HTTP endpoint connection failed")
        while result not in {0, errno.EISCONN}:
            ready = self._wait_socket(
                current_socket,
                read=False,
                context=context,
                request_id=request_id,
                deadline_at=deadline_at,
                connect_expires=connect_expires,
            )
            if not ready:
                continue
            result = current_socket.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
            if result not in {0, errno.EISCONN}:
                raise OSError(result, "HTTP endpoint connection failed")

    def _handshake_tls(
        self,
        current_socket: ssl.SSLSocket,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
        connect_expires: float,
    ) -> None:
        while True:
            self._check_connect_state(context, request_id, deadline_at)
            try:
                current_socket.do_handshake()
                return
            except ssl.SSLWantReadError:
                read = True
            except ssl.SSLWantWriteError:
                read = False
            self._wait_socket(
                current_socket,
                read=read,
                context=context,
                request_id=request_id,
                deadline_at=deadline_at,
                connect_expires=connect_expires,
            )

    def _wait_socket(
        self,
        current_socket: socket.socket,
        *,
        read: bool,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
        connect_expires: float,
    ) -> bool:
        self._check_connect_state(context, request_id, deadline_at)
        remaining = connect_expires - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("HTTP endpoint connection timed out")
        timeout = min(0.05, remaining)
        readers = (current_socket,) if read else ()
        writers = () if read else (current_socket,)
        selected_readers, selected_writers, exceptional = select.select(
            readers,
            writers,
            (current_socket,),
            timeout,
        )
        self._check_connect_state(context, request_id, deadline_at)
        return bool(selected_readers or selected_writers or exceptional)

    def _register_socket(
        self,
        current_socket: socket.socket,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
    ) -> None:
        with self._state_lock:
            interrupted = self._closed or context.aborted
            if not interrupted:
                self._active_sockets[current_socket] = context
        if interrupted:
            _abort_socket(current_socket)
            raise self._interrupted_error(context, request_id)
        self._check_connect_state(context, request_id, deadline_at)

    def _replace_socket(
        self,
        previous: socket.socket,
        current: socket.socket,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
    ) -> None:
        with self._state_lock:
            active = self._active_sockets.pop(previous, None)
            interrupted = self._closed or context.aborted or active is not context
            if not interrupted:
                self._active_sockets[current] = context
        if interrupted:
            _abort_socket(current)
            raise self._interrupted_error(context, request_id)
        self._check_connect_state(context, request_id, deadline_at)

    def _drop_socket(
        self,
        current_socket: socket.socket,
        context: _HttpRequestContext,
    ) -> None:
        with self._state_lock:
            if self._active_sockets.get(current_socket) is context:
                self._active_sockets.pop(current_socket, None)

    def _check_connect_state(
        self,
        context: _HttpRequestContext,
        request_id: int,
        deadline_at: float | None,
    ) -> None:
        if deadline_at is not None and time.time() >= deadline_at:
            context.deadline_expired = True
            context.aborted = True
        with self._state_lock:
            interrupted = self._closed or context.aborted
        if interrupted:
            raise self._interrupted_error(context, request_id)

    @staticmethod
    def _remaining_timeout(
        deadline_at: float | None,
        timeout_seconds: float,
    ) -> float:
        if deadline_at is None:
            return timeout_seconds
        return max(1e-6, min(timeout_seconds, deadline_at - time.time()))

    def _expire_context(self, context: _HttpRequestContext) -> None:
        with self._state_lock:
            context.deadline_expired = True
            context.aborted = True
            connections = tuple(
                connection
                for connection, active in self._active_connections.items()
                if active is context
            )
            sockets = tuple(
                current_socket
                for current_socket, active in self._active_sockets.items()
                if active is context
            )
        _abort_http_handles(connections, sockets)

    def _interrupted_error(
        self,
        context: _HttpRequestContext,
        request_id: int,
    ) -> ClientError | ProductError:
        if context.deadline_expired:
            return product_error("deadline_exceeded", request_id)
        if context.cancellation.cancelled:
            return product_error("cancelled", request_id)
        with self._state_lock:
            closed = self._closed
        return ClientError(
            "HTTP transport is closed" if closed else "HTTP transport was aborted"
        )


def _abort_http_handles(
    connections: tuple[http.client.HTTPConnection, ...],
    sockets: tuple[socket.socket, ...],
) -> None:
    for current_socket in sockets:
        _abort_socket(current_socket)
    for connection in connections:
        _abort_connection(connection)


def _abort_connection(connection: http.client.HTTPConnection) -> None:
    connection.auto_open = 0
    current_socket = getattr(connection, "sock", None)
    if current_socket is not None:
        _abort_socket(current_socket)
    try:
        connection.close()
    except (OSError, ValueError):
        pass


def _abort_socket(current_socket: socket.socket) -> None:
    try:
        current_socket.shutdown(socket.SHUT_RDWR)
    except (OSError, ValueError):
        pass
    try:
        current_socket.close()
    except (OSError, ValueError):
        pass


def _wrap_tls_socket(
    connection: http.client.HTTPSConnection,
    current_socket: socket.socket,
    server_hostname: str,
) -> ssl.SSLSocket:
    return connection._context.wrap_socket(  # type: ignore[attr-defined]
        current_socket,
        server_hostname=server_hostname,
        do_handshake_on_connect=False,
    )


def _response_socket(response) -> socket.socket | None:  # type: ignore[no-untyped-def]
    current = response
    visited: set[int] = set()
    for _ in range(8):
        identity = id(current)
        if identity in visited:
            return None
        visited.add(identity)
        if isinstance(current, socket.socket):
            return current
        current = next(
            (
                candidate
                for attribute in ("fp", "raw", "_sock")
                if (candidate := getattr(current, attribute, None)) is not None
            ),
            None,
        )
        if current is None:
            return None
    return None


def _is_loopback_host(hostname: str | None) -> bool:
    if hostname is None:
        return False
    if hostname.casefold() == "localhost":
        return True
    try:
        address = ipaddress.ip_address(hostname)
    except ValueError:
        return False
    return (
        isinstance(address, ipaddress.IPv4Address) and address.is_loopback
    ) or address == ipaddress.IPv6Address("::1")


def _decode_json_error(encoded: bytes) -> ProductErrorFields:
    import json

    try:
        value = json.loads(encoded)
        details = value.get("details", {})
        return ProductErrorFields(
            code=value["code"],
            category=value["category"],
            retry=value["retry"],
            message=value["message"],
            request_id=int(value["request_id"])
            if value.get("request_id") is not None
            else None,
            trace_id=int(value["trace_id"])
            if value.get("trace_id") is not None
            else None,
            object_id=int(value["object_id"])
            if value.get("object_id") is not None
            else None,
            transaction_state=value["transaction_state"],
            transaction_id=int(value["transaction_id"])
            if value.get("transaction_id") is not None
            else None,
            limit=value.get("limit"),
            source_span=value.get("source_span"),
            details=details,
        )
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise ClientError("HTTP v2 product error JSON is invalid") from error


__all__ = ["ERROR_MEDIA_TYPE", "HttpTransport", "PRODUCT_MEDIA_TYPE"]
