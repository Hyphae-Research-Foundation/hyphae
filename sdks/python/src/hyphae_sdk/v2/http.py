# SPDX-License-Identifier: AGPL-3.0-only
"""Binary product-envelope HTTP /v2 transport."""

from __future__ import annotations

import http.client
import threading
import time
import urllib.parse

from .models import ClientError, ProductError, RequestOptions, Response, product_error
from .protocol import decode_product_error, decode_product_response, encode_product_request


PRODUCT_MEDIA_TYPE = "application/vnd.hyphae.product-v1"
ERROR_MEDIA_TYPE = "application/vnd.hyphae.error-v1"


class HttpTransport:
    """Bounded HTTP `/v2/execute` transport carrying canonical binary envelopes."""

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
        if bearer_token is not None and (not bearer_token or "\r" in bearer_token or "\n" in bearer_token):
            raise ClientError("invalid bearer token")
        self._parsed = parsed
        self._bearer_token = bearer_token
        self._timeout_seconds = timeout_seconds
        self._response_bytes = response_bytes
        self._session_id: str | None = None

    def execute(self, operation: str, arguments: dict[str, object], options: RequestOptions) -> Response:
        request_id = options.checked_request_id()
        if options.cancellation.cancelled:
            raise product_error("cancelled", request_id)
        timeout_seconds = self._timeout_seconds
        if options.deadline_micros is not None:
            remaining = options.deadline_micros / 1_000_000 - time.time()
            if remaining <= 0:
                raise product_error("deadline_exceeded", request_id)
            timeout_seconds = min(timeout_seconds, remaining)
        body = encode_product_request(operation, arguments, options)
        headers = {
            "Accept": f"{PRODUCT_MEDIA_TYPE}, {ERROR_MEDIA_TYPE}",
            "Content-Type": PRODUCT_MEDIA_TYPE,
            "Content-Length": str(len(body)),
            "X-Hyphae-Request-Id": str(request_id),
        }
        if options.deadline_micros is not None:
            headers["X-Hyphae-Deadline-Micros"] = str(options.deadline_micros)
        if self._bearer_token is not None:
            headers["Authorization"] = f"Bearer {self._bearer_token}"
        if self._session_id is not None:
            headers["X-Hyphae-Session-Id"] = self._session_id
        connection_type = http.client.HTTPSConnection if self._parsed.scheme == "https" else http.client.HTTPConnection
        connection = connection_type(self._parsed.hostname, self._parsed.port, timeout=timeout_seconds)
        try:
            connection.request("POST", "/v2/execute", body=body, headers=headers)
            response = connection.getresponse()
            session_id = response.getheader("X-Hyphae-Session-Id")
            if session_id is not None:
                self._session_id = session_id
            if response.getheader("X-Hyphae-Request-Id") != str(request_id):
                raise ClientError("HTTP v2 response request ID mismatch")
            declared = response.getheader("Content-Length")
            maximum = min(self._response_bytes, options.limits["max_response_bytes"])
            if declared is not None and (not declared.isascii() or not declared.isdigit() or int(declared) > maximum):
                raise ClientError("HTTP v2 response exceeds the configured maximum")
            encoded = response.read(maximum + 1)
            if len(encoded) > maximum:
                raise ClientError("HTTP v2 response exceeds the configured maximum")
            if declared is not None and len(encoded) != int(declared):
                raise ClientError("HTTP v2 response length differs from Content-Length")
            media_type = (response.getheader("Content-Type") or "").split(";", 1)[0].strip().lower()
            if 200 <= response.status < 300:
                if response.status != 200 or media_type != PRODUCT_MEDIA_TYPE:
                    raise ClientError("HTTP v2 returned an unexpected status or media type")
                return decode_product_response(encoded, request_id)
            if media_type == ERROR_MEDIA_TYPE:
                raise ProductError(decode_product_error(encoded), status=response.status)
            if media_type == "application/json":
                raise ProductError(_decode_json_error(encoded), status=response.status)
            raise ClientError("HTTP v2 failure did not contain a typed product error")
        except (OSError, http.client.HTTPException) as error:
            if options.deadline_micros is not None and time.time_ns() // 1000 >= options.deadline_micros:
                raise product_error("deadline_exceeded", request_id) from error
            raise ClientError("Hyphae HTTP v2 transport failed") from error
        finally:
            connection.close()


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
            request_id=int(value["request_id"]) if value.get("request_id") is not None else None,
            trace_id=int(value["trace_id"]) if value.get("trace_id") is not None else None,
            object_id=int(value["object_id"]) if value.get("object_id") is not None else None,
            transaction_state=value["transaction_state"],
            transaction_id=int(value["transaction_id"]) if value.get("transaction_id") is not None else None,
            limit=value.get("limit"),
            source_span=value.get("source_span"),
            details=details,
        )
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise ClientError("HTTP v2 product error JSON is invalid") from error


__all__ = ["ERROR_MEDIA_TYPE", "HttpTransport", "PRODUCT_MEDIA_TYPE"]
