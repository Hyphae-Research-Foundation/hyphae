# SPDX-License-Identifier: Apache-2.0
"""Bounded synchronous client for the public Hyphae v1 HTTP API."""

from __future__ import annotations

import http.client
import json
import math
import socket
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from email.message import Message
from typing import Generic, TypeVar, cast

from .generated import (
    CapabilitiesV1,
    CommitReceiptV1,
    DefineLexicalIndexRequestV1,
    DefineVectorSpaceRequestV1,
    DeleteRequestV1,
    DeleteVectorsRequestV1,
    ErrorV1,
    ExactRetrievalRequestV1,
    ExactRetrievalResponseV1,
    GetRequestV1,
    GetResponseV1,
    HealthV1,
    HybridRetrievalRequestV1,
    HybridRetrievalResponseV1,
    LexicalRetrievalRequestV1,
    LexicalRetrievalResponseV1,
    ProofV1,
    PutRequestV1,
    PutVectorsRequestV1,
    QueryRequestV1,
    QueryResponseV1,
    RetrievalProofV1,
)

DEFAULT_RESPONSE_BYTES = 32 * 1024 * 1024
DEFAULT_WITNESS_BYTES = 512 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 60.0
_CHUNK_BYTES = 64 * 1024
T = TypeVar("T")


@dataclass(frozen=True)
class ApiResponse(Generic[T]):
    """A typed API value and its response correlation identifier."""

    value: T
    request_id: str


class HyphaeClientError(Exception):
    """A local configuration, transport, bound, or contract failure."""


class HyphaeApiError(HyphaeClientError):
    """A stable error declared by the Hyphae v1 API."""

    def __init__(self, status: int, envelope: ErrorV1) -> None:
        self.status = status
        self.code = envelope["code"]
        self.request_id = envelope["request_id"]
        self.server_message = envelope["message"]
        super().__init__(
            f"Hyphae API returned HTTP {status} {self.code} "
            f"(request {self.request_id})"
        )


@dataclass(frozen=True)
class _Deadline:
    expires_at: float

    @classmethod
    def start(cls, timeout_seconds: float) -> _Deadline:
        return cls(time.monotonic() + timeout_seconds)

    def remaining(self) -> float:
        remaining = self.expires_at - time.monotonic()
        if remaining <= 0:
            raise _deadline_error()
        return remaining

    def elapsed(self) -> bool:
        return time.monotonic() >= self.expires_at


def _abort_connection(connection: http.client.HTTPConnection) -> None:
    current_socket = connection.sock
    if current_socket is None:
        return
    _abort_socket(current_socket)


class _DeadlineGuard:
    """Interrupt blocking urllib I/O when one absolute deadline expires."""

    def __init__(self, deadline: _Deadline) -> None:
        self._deadline = deadline
        self._cancelled = threading.Event()
        self._expired = threading.Event()
        self._lock = threading.Lock()
        self._connection: http.client.HTTPConnection | None = None
        self._socket: socket.socket | None = None
        self._thread = threading.Thread(
            target=self._watch,
            name="hyphae-http-deadline",
            daemon=True,
        )
        self._thread.start()

    def attach(self, connection: http.client.HTTPConnection) -> None:
        with self._lock:
            self._connection = connection
            self._socket = None
        if self.expired():
            _abort_connection(connection)

    def attach_response(self, response) -> None:  # type: ignore[no-untyped-def]
        response_socket = _response_socket(response)
        with self._lock:
            self._socket = response_socket
        if self.expired() and response_socket is not None:
            _abort_socket(response_socket)

    def expired(self) -> bool:
        return self._expired.is_set() or self._deadline.elapsed()

    def ensure_open(self, connection: http.client.HTTPConnection) -> None:
        if self.expired():
            _abort_connection(connection)
            raise TimeoutError("Hyphae HTTP deadline elapsed")

    def close(self) -> None:
        self._cancelled.set()
        self._thread.join()

    def _watch(self) -> None:
        while True:
            remaining = self._deadline.expires_at - time.monotonic()
            if remaining <= 0:
                break
            if self._cancelled.wait(min(remaining, 60.0)):
                return
        self._expired.set()
        with self._lock:
            connection = self._connection
            response_socket = self._socket
        if response_socket is not None:
            _abort_socket(response_socket)
        if connection is not None:
            _abort_connection(connection)


def _abort_socket(current_socket: socket.socket) -> None:
    try:
        current_socket.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    try:
        current_socket.close()
    except OSError:
        pass


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


class _GuardedHTTPConnection(http.client.HTTPConnection):
    def __init__(
        self, host: str, *, guard: _DeadlineGuard, **kwargs: object
    ) -> None:
        super().__init__(host, **kwargs)
        self._hyphae_guard = guard
        guard.attach(self)

    def connect(self) -> None:
        super().connect()
        self._hyphae_guard.ensure_open(self)


class _GuardedHTTPSConnection(http.client.HTTPSConnection):
    def __init__(
        self, host: str, *, guard: _DeadlineGuard, **kwargs: object
    ) -> None:
        super().__init__(host, **kwargs)
        self._hyphae_guard = guard
        guard.attach(self)

    def connect(self) -> None:
        super().connect()
        self._hyphae_guard.ensure_open(self)


class _DeadlineHTTPHandler(urllib.request.HTTPHandler):
    def __init__(self, guard: _DeadlineGuard) -> None:
        super().__init__()
        self._guard = guard

    def http_open(self, request):  # type: ignore[no-untyped-def]
        guard = self._guard

        def connection(host: str, **kwargs: object) -> _GuardedHTTPConnection:
            return _GuardedHTTPConnection(host, guard=guard, **kwargs)

        return self.do_open(connection, request)


class _DeadlineHTTPSHandler(urllib.request.HTTPSHandler):
    def __init__(self, guard: _DeadlineGuard) -> None:
        super().__init__()
        self._guard = guard

    def https_open(self, request):  # type: ignore[no-untyped-def]
        guard = self._guard

        def connection(host: str, **kwargs: object) -> _GuardedHTTPSConnection:
            return _GuardedHTTPSConnection(host, guard=guard, **kwargs)

        return self.do_open(
            connection,
            request,
            context=self._context,
            check_hostname=self._check_hostname,
        )


class _RejectRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(  # type: ignore[no-untyped-def]
        self, request, file_pointer, code, message, headers, new_url
    ):
        del request, file_pointer, code, message, headers, new_url
        raise HyphaeClientError("Hyphae HTTP redirects are not allowed")


class _GuardedResponse:
    def __init__(self, response, guard: _DeadlineGuard) -> None:  # type: ignore[no-untyped-def]
        self._response = response
        self._guard = guard
        self._closed = False

    def __getattr__(self, name: str):  # type: ignore[no-untyped-def]
        return getattr(self._response, name)

    def read(self, size: int = -1) -> bytes:
        return self._response.read(size)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self._response.close()
        finally:
            self._guard.close()


def _guarded_response(response, guard: _DeadlineGuard) -> _GuardedResponse:  # type: ignore[no-untyped-def]
    try:
        guard.attach_response(response)
    except BaseException:
        try:
            response.close()
        finally:
            guard.close()
        raise
    return _GuardedResponse(response, guard)


class HyphaeClient:
    """Dependency-free, bounded client for one root Hyphae HTTP origin."""

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        response_bytes: int = DEFAULT_RESPONSE_BYTES,
        witness_bytes: int = DEFAULT_WITNESS_BYTES,
    ) -> None:
        parsed = urllib.parse.urlsplit(base_url)
        if (
            parsed.scheme not in {"http", "https"}
            or not parsed.netloc
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
            or parsed.path not in {"", "/"}
        ):
            raise HyphaeClientError(
                "Hyphae base URL must be a root HTTP(S) origin"
            )
        if any(
            isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0
            for value in (timeout_seconds, response_bytes, witness_bytes)
        ) or not math.isfinite(float(timeout_seconds)):
            raise HyphaeClientError(
                "client timeout and response limits must be positive numbers"
            )
        if not isinstance(response_bytes, int) or not isinstance(witness_bytes, int):
            raise HyphaeClientError("client byte limits must be integers")
        if bearer_token is not None and (
            not bearer_token or "\r" in bearer_token or "\n" in bearer_token
        ):
            raise HyphaeClientError(
                "invalid bearer token for an HTTP authorization header"
            )
        self._base_url = urllib.parse.urlunsplit(
            (parsed.scheme, parsed.netloc, "/", "", "")
        )
        self._bearer_token = bearer_token
        self._timeout_seconds = float(timeout_seconds)
        self._response_bytes = response_bytes
        self._witness_bytes = witness_bytes

    def capabilities(self) -> ApiResponse[CapabilitiesV1]:
        return cast(ApiResponse[CapabilitiesV1], self._json("v1/capabilities", False))

    def liveness(self) -> ApiResponse[HealthV1]:
        return cast(ApiResponse[HealthV1], self._json("v1/health/live", False))

    def readiness(self) -> ApiResponse[HealthV1]:
        return cast(ApiResponse[HealthV1], self._json("v1/health/ready", False))

    def put(self, request: PutRequestV1) -> ApiResponse[CommitReceiptV1]:
        return cast(ApiResponse[CommitReceiptV1], self._json("v1/kv/put", True, request))

    def delete(self, request: DeleteRequestV1) -> ApiResponse[CommitReceiptV1]:
        return cast(
            ApiResponse[CommitReceiptV1], self._json("v1/kv/delete", True, request)
        )

    def get(self, request: GetRequestV1) -> ApiResponse[GetResponseV1]:
        return cast(ApiResponse[GetResponseV1], self._json("v1/kv/get", True, request))

    def query(self, request: QueryRequestV1) -> ApiResponse[QueryResponseV1]:
        return cast(ApiResponse[QueryResponseV1], self._json("v1/query", True, request))

    def define_vector_space(
        self, request: DefineVectorSpaceRequestV1
    ) -> ApiResponse[CommitReceiptV1]:
        return cast(
            ApiResponse[CommitReceiptV1],
            self._json("v1/vector-spaces/define", True, request),
        )

    def put_vectors(
        self, request: PutVectorsRequestV1
    ) -> ApiResponse[CommitReceiptV1]:
        return cast(
            ApiResponse[CommitReceiptV1],
            self._json("v1/vectors/put", True, request),
        )

    def delete_vectors(
        self, request: DeleteVectorsRequestV1
    ) -> ApiResponse[CommitReceiptV1]:
        return cast(
            ApiResponse[CommitReceiptV1],
            self._json("v1/vectors/delete", True, request),
        )

    def retrieve_exact(
        self, request: ExactRetrievalRequestV1
    ) -> ApiResponse[ExactRetrievalResponseV1]:
        return cast(
            ApiResponse[ExactRetrievalResponseV1],
            self._json("v1/retrieve/exact", True, request),
        )

    def define_lexical_index(
        self, request: DefineLexicalIndexRequestV1
    ) -> ApiResponse[CommitReceiptV1]:
        return cast(
            ApiResponse[CommitReceiptV1],
            self._json("v1/lexical-indexes/define", True, request),
        )

    def retrieve_lexical(
        self, request: LexicalRetrievalRequestV1
    ) -> ApiResponse[LexicalRetrievalResponseV1]:
        return cast(
            ApiResponse[LexicalRetrievalResponseV1],
            self._json("v1/retrieve/lexical", True, request),
        )

    def retrieve_hybrid(
        self, request: HybridRetrievalRequestV1
    ) -> ApiResponse[HybridRetrievalResponseV1]:
        return cast(
            ApiResponse[HybridRetrievalResponseV1],
            self._json("v1/retrieve/hybrid", True, request),
        )

    def download_witness(self, proof: ProofV1) -> ApiResponse[bytes]:
        return self._download_witness(proof)

    def download_retrieval_witness(
        self, proof: RetrievalProofV1
    ) -> ApiResponse[bytes]:
        return self._download_witness(proof)

    def _download_witness(
        self, proof: ProofV1 | RetrievalProofV1
    ) -> ApiResponse[bytes]:
        deadline = _Deadline.start(self._timeout_seconds)
        expected_path = (
            f"/v1/witnesses/{proof['checkpoint_sequence']}/"
            f"{proof['snapshot_digest']}"
        )
        if proof["witness"]["path"] != expected_path:
            raise HyphaeClientError(
                "proof contains a noncanonical witness reference"
            )
        expected_bytes = proof["witness"]["file_bytes"]
        if (
            isinstance(expected_bytes, bool)
            or not isinstance(expected_bytes, int)
            or expected_bytes < 0
            or expected_bytes > self._witness_bytes
        ):
            raise HyphaeClientError(
                f"Hyphae response exceeded local limit {self._witness_bytes} bytes"
            )
        deadline.remaining()
        response = self._open(
            expected_path[1:], authenticated=True, deadline=deadline
        )
        try:
            deadline.remaining()
            if response.status < 200 or response.status >= 300:
                raise self._decode_api_error(response, deadline)
            if response.status != 200:
                raise HyphaeClientError(
                    f"Hyphae returned unexpected success status {response.status}"
                )
            request_id = _request_id(response.headers)
            if _single_header(response.headers, "digest") != (
                f"blake3={proof['snapshot_digest']}"
            ):
                raise HyphaeClientError(
                    "downloaded witness digest header differs from the proof"
                )
            value = _read_bounded(response, self._witness_bytes, deadline)
            if len(value) != expected_bytes:
                raise HyphaeClientError(
                    "downloaded witness length differs from the proof"
                )
            deadline.remaining()
            return ApiResponse(value, request_id)
        finally:
            response.close()

    def _json(
        self, path: str, authenticated: bool, body: object | None = None
    ) -> ApiResponse[object]:
        deadline = _Deadline.start(self._timeout_seconds)
        response = self._open(
            path, authenticated=authenticated, body=body, deadline=deadline
        )
        try:
            deadline.remaining()
            if response.status < 200 or response.status >= 300:
                raise self._decode_api_error(response, deadline)
            if response.status != 200:
                raise HyphaeClientError(
                    f"Hyphae returned unexpected success status {response.status}"
                )
            _require_json(response.headers)
            request_id = _request_id(response.headers)
            encoded = _read_bounded(response, self._response_bytes, deadline)
            try:
                value = _loads_integer_json(encoded)
            except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
                if deadline.elapsed():
                    raise _deadline_error() from error
                raise HyphaeClientError(
                    "Hyphae response violated the version 1 JSON contract"
                ) from error
            deadline.remaining()
            return ApiResponse(value, request_id)
        finally:
            response.close()

    def _open(
        self,
        path: str,
        *,
        authenticated: bool,
        deadline: _Deadline,
        body: object | None = None,
    ):  # type: ignore[no-untyped-def]
        headers: dict[str, str] = {}
        data: bytes | None = None
        method = "GET"
        if authenticated and self._bearer_token is not None:
            headers["Authorization"] = f"Bearer {self._bearer_token}"
        if body is not None:
            method = "POST"
            headers["Content-Type"] = "application/json"
            data = json.dumps(
                body, ensure_ascii=False, separators=(",", ":"), allow_nan=False
            ).encode("utf-8")
        request = urllib.request.Request(
            urllib.parse.urljoin(self._base_url, path),
            data=data,
            headers=headers,
            method=method,
        )
        guard = _DeadlineGuard(deadline)
        try:
            opener = urllib.request.build_opener(
                _DeadlineHTTPHandler(guard),
                _DeadlineHTTPSHandler(guard),
                _RejectRedirectHandler(),
            )
            response = opener.open(request, timeout=deadline.remaining())
        except urllib.error.HTTPError as response:
            return _guarded_response(response, guard)
        except (
            OSError,
            urllib.error.URLError,
            TimeoutError,
            http.client.HTTPException,
        ) as error:
            expired = guard.expired()
            guard.close()
            if expired or _is_timeout_error(error):
                raise _deadline_error() from error
            raise HyphaeClientError("Hyphae HTTP transport failed") from error
        except BaseException:
            guard.close()
            raise
        return _guarded_response(response, guard)

    def _decode_api_error(
        self, response, deadline: _Deadline
    ) -> HyphaeApiError:  # type: ignore[no-untyped-def]
        _require_json(response.headers)
        request_id = _request_id(response.headers)
        encoded = _read_bounded(response, self._response_bytes, deadline)
        try:
            value = _loads_integer_json(encoded)
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
            if deadline.elapsed():
                raise _deadline_error() from error
            raise HyphaeClientError(
                "Hyphae error response violated the version 1 JSON contract"
            ) from error
        if (
            not isinstance(value, dict)
            or not isinstance(value.get("code"), str)
            or not isinstance(value.get("message"), str)
            or not isinstance(value.get("request_id"), str)
        ):
            raise HyphaeClientError(
                "Hyphae error response violated the version 1 JSON contract"
            )
        envelope = cast(ErrorV1, value)
        if envelope["request_id"] != request_id:
            raise HyphaeClientError(
                "Hyphae error envelope request ID differs from its response header"
            )
        deadline.remaining()
        return HyphaeApiError(response.status, envelope)


def _single_header(headers: Message, name: str) -> str | None:
    values = headers.get_all(name, failobj=[])
    if len(values) != 1 or not values[0] or "," in values[0]:
        return None
    return values[0]


def _loads_integer_json(encoded: bytes) -> object:
    def reject_non_integer(token: str) -> object:
        raise ValueError(f"Hyphae JSON number is not an integer: {token}")

    return json.loads(
        encoded.decode("utf-8", errors="strict"),
        parse_float=reject_non_integer,
        parse_constant=reject_non_integer,
    )


def _request_id(headers: Message) -> str:
    value = _single_header(headers, "x-request-id")
    if value is None:
        raise HyphaeClientError(
            "Hyphae response has no single valid X-Request-Id header"
        )
    return value


def _require_json(headers: Message) -> None:
    content_type = _single_header(headers, "content-type")
    media_type = content_type.split(";", 1)[0].strip().lower() if content_type else ""
    if media_type != "application/json" and not (
        media_type.startswith("application/") and media_type.endswith("+json")
    ):
        raise HyphaeClientError(
            "Hyphae response did not use a JSON content type"
        )


def _deadline_error() -> HyphaeClientError:
    return HyphaeClientError("Hyphae HTTP request/response deadline elapsed")


def _is_timeout_error(error: BaseException) -> bool:
    if isinstance(error, (TimeoutError, socket.timeout)):
        return True
    if isinstance(error, urllib.error.URLError) and isinstance(
        error.reason, BaseException
    ):
        return _is_timeout_error(error.reason)
    return False


def _set_response_timeout(response, timeout_seconds: float) -> None:  # type: ignore[no-untyped-def]
    # CPython exposes no public per-read timeout setter. Its urllib
    # HTTPResponse and HTTPError wrappers reach the owned socket through a
    # short fp/raw/_sock chain, so traverse that chain defensively.
    current = response
    visited: set[int] = set()
    for _ in range(8):
        identity = id(current)
        if identity in visited:
            return
        visited.add(identity)
        setter = getattr(current, "settimeout", None)
        if callable(setter):
            setter(timeout_seconds)
            return
        current = next(
            (
                candidate
                for attribute in ("fp", "raw", "_sock")
                if (candidate := getattr(current, attribute, None)) is not None
            ),
            None,
        )
        if current is None:
            return


def _read_bounded(
    response, maximum: int, deadline: _Deadline
) -> bytes:  # type: ignore[no-untyped-def]
    declared = _single_header(response.headers, "content-length")
    if declared is not None:
        if not declared.isascii() or not declared.isdigit() or int(declared) > maximum:
            raise HyphaeClientError(
                f"Hyphae response exceeded local limit {maximum} bytes"
            )
    chunks: list[bytes] = []
    length = 0
    while True:
        remaining = deadline.remaining()
        try:
            _set_response_timeout(response, remaining)
            chunk = response.read(_CHUNK_BYTES)
        except (
            OSError,
            urllib.error.URLError,
            TimeoutError,
            http.client.HTTPException,
            ValueError,
        ) as error:
            guard_expired = isinstance(response, _GuardedResponse) and (
                response._guard.expired()
            )
            if guard_expired or deadline.elapsed() or _is_timeout_error(error):
                raise _deadline_error() from error
            raise HyphaeClientError("Hyphae HTTP transport failed") from error
        if not chunk:
            break
        length += len(chunk)
        if length > maximum:
            raise HyphaeClientError(
                f"Hyphae response exceeded local limit {maximum} bytes"
            )
        chunks.append(chunk)
    deadline.remaining()
    return b"".join(chunks)


__all__ = [
    "ApiResponse",
    "HyphaeApiError",
    "HyphaeClient",
    "HyphaeClientError",
]
