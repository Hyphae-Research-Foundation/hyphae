# SPDX-License-Identifier: AGPL-3.0-only
"""Equivalent high-level Hyphae v2 API over local and HTTP transports."""

from __future__ import annotations

from typing import Any, Protocol

from .http import HttpTransport
from .local import LocalTransport
from .models import CancellationToken, ClientError, RequestOptions, Response


class Transport(Protocol):
    def execute(self, operation: str, arguments: dict[str, object], options: RequestOptions) -> Response: ...


class AbortableTransport(Transport, Protocol):
    """Transport whose active operation can be interrupted from another thread."""

    def abort(self, cancellation: CancellationToken | None = None) -> None: ...

    def close(self) -> None: ...


class HyphaeClient:
    """One high-level API whose operation names and results do not depend on transport."""

    def __init__(self, transport: Transport) -> None:
        self._transport = transport
        self._closed = False

    @classmethod
    def local(
        cls,
        endpoint: str,
        *,
        client_identity: str = "hyphae-python-sdk-v2",
        api_key: str | None = None,
    ) -> HyphaeClient:
        return cls(LocalTransport(endpoint, client_identity=client_identity, api_key=api_key))

    @classmethod
    def local_authenticated(
        cls,
        endpoint: str,
        api_key: str,
        *,
        client_identity: str = "hyphae-python-sdk-v2",
    ) -> HyphaeClient:
        """Open a managed local session using the Native HELLO credential trailer."""

        return cls.local(endpoint, client_identity=client_identity, api_key=api_key)

    @classmethod
    def http(cls, base_url: str, **kwargs: Any) -> HyphaeClient:
        return cls(HttpTransport(base_url, **kwargs))

    def execute(self, operation: str, arguments: dict[str, object] | None = None, *, options: RequestOptions | None = None) -> Response:
        if self._closed:
            raise ClientError("Hyphae client is closed")
        return self._transport.execute(operation, arguments or {}, options or RequestOptions())

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        close = getattr(self._transport, "close", None)
        if close is not None:
            close()

    def __enter__(self) -> HyphaeClient:
        if self._closed:
            raise ClientError("Hyphae client is closed")
        return self

    def __exit__(self, *exc_info: object) -> None:
        del exc_info
        self.close()

    def _execute_expected(
        self,
        operation: str,
        expected_kind: str,
        arguments: dict[str, object] | None = None,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        response = self.execute(operation, arguments, options=options)
        if response.kind != expected_kind:
            raise ClientError(
                f"{operation} returned unexpected response kind {response.kind}"
            )
        return response

    def capabilities(self, *, options: RequestOptions | None = None) -> Response:
        return self.execute("capabilities", options=options)

    def catalog(self, action: str, arguments: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute(f"catalog_{action}", arguments, options=options)

    def catalog_object(self, object_id: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("catalog_object", {"id": object_id}, options=options)

    def catalog_list(self, request: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute("catalog_list", request, options=options)

    def sql(self, statement: str, parameters: list[object] | None = None, *, options: RequestOptions | None = None) -> Response:
        return self.execute("sql_execute", {"statement": statement, "parameters": parameters or []}, options=options)

    def prepare_sql(self, statement: str, *, options: RequestOptions | None = None) -> Response:
        return self.execute("sql_prepare", {"statement": statement}, options=options)

    def execute_prepared(self, handle: int, parameters: list[object] | None = None, *, options: RequestOptions | None = None) -> Response:
        return self.execute("sql_execute_prepared", {"handle": handle, "parameters": parameters or []}, options=options)

    def deallocate_prepared(self, handle: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("sql_deallocate", {"handle": handle}, options=options)

    def structure_get(self, key: bytes, *, options: RequestOptions | None = None) -> Response:
        return self.execute("structure_get", {"key": key}, options=options)

    def structure_set(self, key: bytes, value: bytes, *, expires_at_micros: int | None = None, options: RequestOptions | None = None) -> Response:
        return self.execute("structure_set", {"key": key, "value": value, "expires_at_micros": expires_at_micros}, options=options)

    def structure_ttl(self, key: bytes, *, options: RequestOptions | None = None) -> Response:
        return self.execute("structure_ttl", {"key": key}, options=options)

    def structure_mutate(self, mutations: list[dict[str, object]], *, options: RequestOptions | None = None) -> Response:
        return self.execute("structure_mutate", {"mutations": mutations}, options=options)

    def structure_read(self, request: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute("structure_read", request, options=options)

    def search(self, index: int, query: dict[str, object], limit: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("search", {"index": index, "query": query, "limit": limit}, options=options)

    def search_collection(self, collection: int, request: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute("search_collection", {"collection": collection, "request": request}, options=options)

    def search_ingest(self, collection: int, batch: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute("search_ingest", {"collection": collection, "batch": batch}, options=options)

    def search_document_update(self, collection: int, idempotency_id: int, document: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute("search_document_update", {"collection": collection, "idempotency_id": idempotency_id, "document": document}, options=options)

    def search_document_delete(self, collection: int, idempotency_id: int, object_id: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("search_document_delete", {"collection": collection, "idempotency_id": idempotency_id, "object_id": object_id}, options=options)

    def admin(self, action: str, arguments: dict[str, object] | None = None, *, options: RequestOptions | None = None) -> Response:
        return self.execute(f"admin_{action}", arguments, options=options)

    def telemetry(self, *, options: RequestOptions | None = None) -> Response:
        return self.execute("telemetry", options=options)

    def doctor(self, path: str, logical_time_micros: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("doctor", {"path": path, "logical_time_micros": logical_time_micros}, options=options)

    def backup(self, destination: str, limits: dict[str, int], *, options: RequestOptions | None = None) -> Response:
        return self.execute("backup", {"destination": destination, "limits": limits}, options=options)

    def restore(self, backup: str, destination: str, limits: dict[str, int], *, doctor_logical_time_micros: int = 0, options: RequestOptions | None = None) -> Response:
        return self.execute("restore", {"backup": backup, "destination": destination, "limits": limits, "doctor_logical_time_micros": doctor_logical_time_micros}, options=options)

    def verify_proof(self, proof: bytes, witness: bytes, trusted_anchor: bytes, *, options: RequestOptions | None = None) -> Response:
        return self.execute("proof_verify", {"proof": proof, "witness": witness, "trusted_anchor": trusted_anchor}, options=options)

    def prove(self, operation: str, arguments: dict[str, object], *, limits: dict[str, int] | None = None, options: RequestOptions | None = None) -> Response:
        return self.execute("proof_generate", {"operation": operation, "arguments": arguments, "limits": limits or {}}, options=options)

    def prove_sql(self, statement: str, parameters: list[object] | None = None, *, limits: dict[str, int] | None = None, options: RequestOptions | None = None) -> Response:
        return self.prove("sql_execute", {"statement": statement, "parameters": parameters or []}, limits=limits, options=options)

    def transaction_status(self, transaction_id: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("transaction_status", {"transaction_id": transaction_id}, options=options)

    def transaction_begin(self, *, options: RequestOptions | None = None) -> Response:
        return self.execute("transaction_begin", options=options)

    def transaction_stage_sql(self, handle: int, statement: str, parameters: list[object] | None = None, *, options: RequestOptions | None = None) -> Response:
        return self.execute(
            "transaction_stage_sql",
            {"handle": handle, "statement": statement, "parameters": parameters or []},
            options=options,
        )

    def transaction_stage_structure(self, handle: int, mutation: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute(
            "transaction_stage_structure",
            {"handle": handle, "mutation": mutation},
            options=options,
        )

    def transaction_stage_search(self, handle: int, mutation: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute(
            "transaction_stage_search",
            {"handle": handle, "mutation": mutation},
            options=options,
        )

    def transaction_stage_vector(self, handle: int, mutation: dict[str, object], *, options: RequestOptions | None = None) -> Response:
        return self.execute(
            "transaction_stage_vector",
            {"handle": handle, "mutation": mutation},
            options=options,
        )

    def transaction_commit(self, handle: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("transaction_commit", {"handle": handle}, options=options)

    def transaction_rollback(self, handle: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("transaction_rollback", {"handle": handle}, options=options)

    def explicit_transaction_status(self, handle: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute("explicit_transaction_status", {"handle": handle}, options=options)

    def transaction_status_by_idempotency(self, idempotency_token: int, *, options: RequestOptions | None = None) -> Response:
        return self.execute(
            "transaction_status_by_idempotency",
            {"idempotency_token": idempotency_token},
            options=options,
        )

    def security_status(self, *, options: RequestOptions | None = None) -> Response:
        return self._execute_expected(
            "security_status", "security_status", options=options
        )

    def security_principal_list(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> Response:
        return self._execute_expected(
            "security_principal_list",
            "security_principal_page",
            {"cursor": cursor, "limit": limit},
            options=options,
        )

    def security_role_list(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> Response:
        return self._execute_expected(
            "security_role_list",
            "security_role_page",
            {"cursor": cursor, "limit": limit},
            options=options,
        )

    def security_assignment_list(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> Response:
        return self._execute_expected(
            "security_assignment_list",
            "security_assignment_page",
            {"cursor": cursor, "limit": limit},
            options=options,
        )

    def security_key_list(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> Response:
        """Return redacted API-key metadata; secrets and verifiers are absent."""

        return self._execute_expected(
            "security_key_list",
            "security_key_page",
            {"cursor": cursor, "limit": limit},
            options=options,
        )

    def security_audit_read(
        self,
        *,
        cursor: int | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> Response:
        return self._execute_expected(
            "security_audit_read",
            "security_audit_page",
            {"cursor": cursor, "limit": limit},
            options=options,
        )

    def security_principal_create(
        self,
        display_name: str,
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_principal_create",
            "security_principal_mutated",
            {"display_name": display_name},
            options=_security_mutation_options(options),
        )

    def security_principal_set_enabled(
        self,
        principal_id: int,
        enabled: bool,
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_principal_set_enabled",
            "security_mutated",
            {"principal_id": principal_id, "enabled": enabled},
            options=_security_mutation_options(options),
        )

    def security_custom_role_create(
        self,
        display_name: str,
        grants: list[dict[str, object]],
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_custom_role_create",
            "security_custom_role_mutated",
            {"display_name": display_name, "grants": grants},
            options=_security_mutation_options(options),
        )

    def security_built_in_assignment_create(
        self,
        principal_id: int,
        role: str,
        scope: dict[str, object],
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_built_in_assignment_create",
            "security_assignment_mutated",
            {"principal_id": principal_id, "role": role, "scope": scope},
            options=_security_mutation_options(options),
        )

    def security_custom_assignment_create(
        self,
        principal_id: int,
        role_id: int,
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_custom_assignment_create",
            "security_assignment_mutated",
            {"principal_id": principal_id, "role_id": role_id},
            options=_security_mutation_options(options),
        )

    def security_assignment_revoke(
        self,
        assignment_id: int,
        *,
        options: RequestOptions,
    ) -> Response:
        return self._execute_expected(
            "security_assignment_revoke",
            "security_mutated",
            {"assignment_id": assignment_id},
            options=_security_mutation_options(options),
        )


def _security_mutation_options(options: RequestOptions) -> RequestOptions:
    token = options.idempotency_token
    if isinstance(token, bool) or not isinstance(token, int) or not 0 < token < 1 << 128:
        raise ClientError("security mutation requires a nonzero idempotency_token")
    return options


__all__ = ["AbortableTransport", "HyphaeClient", "Transport"]
