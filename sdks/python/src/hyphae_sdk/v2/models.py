# SPDX-License-Identifier: GPL-3.0-only
"""Runtime models shared by Hyphae v2 local and HTTP transports."""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field
from typing import Any


DEFAULT_LIMITS = {
    "max_count": 4096,
    "max_request_bytes": 16 * 1024 * 1024,
    "max_response_bytes": 16 * 1024 * 1024,
    "max_work_units": 1_000_000,
    "max_memory_bytes": 64 * 1024 * 1024,
}


@dataclass(frozen=True)
class ProductErrorFields:
    """Typed transport-independent fields decoded from JSON or HYPERR01."""

    code: str
    category: str
    retry: str
    message: str
    request_id: int | None = None
    trace_id: int | None = None
    object_id: int | None = None
    transaction_state: str = "none"
    transaction_id: int | None = None
    limit: dict[str, int | str] | None = None
    source_span: dict[str, int] | None = None
    details: dict[str, Any] = field(default_factory=dict)


class ProductError(Exception):
    """Stable product rejection with fields equal across transports."""

    def __init__(self, fields: ProductErrorFields, *, status: int | None = None) -> None:
        self.fields = fields
        self.status = status
        self.code = fields.code
        self.category = fields.category
        self.retry = fields.retry
        self.request_id = fields.request_id
        self.transaction_state = fields.transaction_state
        self.transaction_id = fields.transaction_id
        super().__init__(fields.message)


_PRODUCT_ERROR_DEFAULTS = {
    "deadline_exceeded": ("deadline", "same-request", "native product request deadline exceeded"),
    "cancelled": ("cancelled", "same-request", "native product request was cancelled"),
}


def product_error(code: str, request_id: int) -> ProductError:
    """Builds one registered terminal client state as a typed product error."""

    category, retry, message = _PRODUCT_ERROR_DEFAULTS[code]
    return ProductError(ProductErrorFields(
        code=code,
        category=category,
        retry=retry,
        message=message,
        request_id=request_id,
    ))


class ClientError(Exception):
    """Configuration, transport, bound, cancellation, or protocol failure."""


class CancellationToken:
    """Thread-safe cooperative cancellation token."""

    def __init__(self) -> None:
        self._event = threading.Event()

    def cancel(self) -> None:
        self._event.set()

    @property
    def cancelled(self) -> bool:
        return self._event.is_set()

    def wait(self, timeout: float | None = None) -> bool:
        return self._event.wait(timeout)


@dataclass(frozen=True)
class RequestOptions:
    """Complete execution context applied identically by both transports."""

    request_id: int | None = None
    logical_time_micros: int = 0
    deadline_micros: int | None = None
    idempotency_token: int | None = None
    limits: dict[str, int] = field(default_factory=lambda: dict(DEFAULT_LIMITS))
    durability: str = "strict"
    cancellation: CancellationToken = field(default_factory=CancellationToken)

    def checked_request_id(self) -> int:
        request_id = self.request_id
        if request_id is None:
            request_id = time.time_ns() & ((1 << 64) - 1)
        if isinstance(request_id, bool) or not isinstance(request_id, int) or not 0 < request_id < 1 << 64:
            raise ClientError("request_id must be an unsigned nonzero 64-bit integer")
        return request_id


@dataclass(frozen=True)
class Response:
    """Definitive high-level response after completion validation."""

    kind: str
    value: Any
    request_id: int


__all__ = [
    "CancellationToken",
    "ClientError",
    "DEFAULT_LIMITS",
    "ProductError",
    "ProductErrorFields",
    "RequestOptions",
    "Response",
]
