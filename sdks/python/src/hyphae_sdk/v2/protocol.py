# SPDX-License-Identifier: AGPL-3.0-only
"""Exact dependency-free HYPHLCL1 and product-envelope codecs."""

from __future__ import annotations

import struct
import unicodedata
from dataclasses import dataclass
from typing import Any

from .models import ClientError, ProductErrorFields, RequestOptions, Response


MAX_PAYLOAD = 16 * 1024 * 1024
FRAME_HEADER_SIZE = 32
PROTOCOL_MAJOR = 1
PROTOCOL_MINOR = 2
G6_CAPABILITIES = 0x7F
API_KEY_AUTH_CAPABILITY = 1 << 7
API_KEY_BYTES = 102
MAX_SECURITY_TEXT_BYTES = 128
MAX_SECURITY_LIST_ROWS = 1_000
MAX_SECURITY_GRANTS = 256
MAX_SECURITY_ASSIGNMENTS = 128
FRAME_KINDS = {
    "hello": 1,
    "welcome": 2,
    "prepare": 4,
    "execute": 5,
    "failure": 13,
    "cancel": 14,
    "deallocate": 16,
    "data": 19,
    "end": 20,
    "window_update": 21,
}
REQUEST_KINDS = {
    "capabilities": 1,
    "sql_prepare": 2,
    "sql_execute_prepared": 3,
    "sql_execute": 4,
    "structure_get": 5,
    "structure_set": 6,
    "structure_ttl": 7,
    "transaction_status": 8,
    "search": 9,
    "admin_status": 10,
    "admin_checkpoint": 11,
    "sql_deallocate": 12,
    "catalog_object": 13,
    "catalog_object_named": 14,
    "catalog_list": 15,
    "catalog_dependencies": 16,
    "catalog_describe": 17,
    "catalog_resolve": 18,
    "catalog_create": 19,
    "admin_explain_sql": 20,
    "doctor": 21,
    "backup": 22,
    "telemetry": 23,
    "proof_verify": 24,
    "search_collection": 25,
    "search_ingest": 29,
    "search_document_update": 30,
    "search_document_delete": 31,
    "structure_mutate": 26,
    "structure_read": 27,
    "restore": 28,
    "transaction_begin": 32,
    "transaction_stage_sql": 33,
    "transaction_stage_structure": 34,
    "transaction_stage_search": 35,
    "transaction_stage_vector": 36,
    "transaction_commit": 37,
    "transaction_rollback": 38,
    "transaction_status_by_idempotency": 39,
    "explicit_transaction_status": 40,
    "proof_generate": 41,
    "security_status": 42,
    "security_principal_list": 43,
    "security_role_list": 44,
    "security_assignment_list": 45,
    "security_key_list": 46,
    "security_audit_read": 47,
    "security_principal_create": 48,
    "security_principal_set_enabled": 49,
    "security_custom_role_create": 50,
    "security_built_in_assignment_create": 51,
    "security_custom_assignment_create": 52,
    "security_assignment_revoke": 53,
}

SECURITY_READ_OPERATIONS = frozenset({
    "security_status",
    "security_principal_list",
    "security_role_list",
    "security_assignment_list",
    "security_key_list",
    "security_audit_read",
})
SECURITY_WRITE_OPERATIONS = frozenset({
    "security_principal_create",
    "security_principal_set_enabled",
    "security_custom_role_create",
    "security_built_in_assignment_create",
    "security_custom_assignment_create",
    "security_assignment_revoke",
})
SECURITY_READ_RESPONSE_KINDS = frozenset(range(32, 38))
SECURITY_WRITE_RESPONSE_KINDS = frozenset(range(38, 42))

BUILT_IN_ROLES = ("owner", "admin", "operator", "developer", "writer", "reader", "auditor")
PRODUCT_PERMISSIONS = (
    "audit.read",
    "backup.create",
    "backup.verify",
    "catalog.read",
    "catalog.write",
    "credential.self_manage",
    "data.read",
    "data.write",
    "discover",
    "maintain",
    "observe",
    "ownership.manage",
    "proof.generate",
    "proof.verify",
    "restore",
    "search.execute",
    "security.manage",
    "security.read",
)
SECURITY_AUDIT_ACTIONS = (
    "bootstrap_owner",
    "activate_key",
    "create_principal",
    "create_custom_role",
    "assign_built_in_role",
    "assign_custom_role",
    "issue_key",
    "rotate_key",
    "revoke_key",
    "recover_owner",
    "migrate_legacy_bearer",
    "abort_key_rotation",
    "abort_key_issue",
    "set_principal_enabled",
    "revoke_assignment",
)

DEFAULT_PROOF_LIMITS = {
    "result_items": 10_000,
    "candidate_items": 100_000,
    "evidence_bytes": 32 * 1024 * 1024,
    "max_proof_bytes": 64 * 1024 * 1024,
    "max_section_bytes": 32 * 1024 * 1024,
    "max_decoded_bytes": 48 * 1024 * 1024,
    "max_objects": 4_096,
    "max_hybrid_branches": 64,
    "max_witness_bytes": 4 * 1024 * 1024 * 1024,
    "max_entries": 65_536,
    "max_files": 32_768,
    "max_directories": 32_768,
    "max_path_bytes": 4_096,
    "max_file_bytes": 1024 * 1024 * 1024,
    "max_total_file_bytes": 3 * 1024 * 1024 * 1024,
    "max_witness_decoded_bytes": 3 * 1024 * 1024 * 1024,
}


@dataclass(frozen=True)
class Frame:
    kind: int
    stream_id: int
    request_id: int
    payload: bytes


def crc32c(data: bytes, crc: int = 0) -> int:
    """Portable Castagnoli CRC matching Rust crc32c."""

    crc ^= 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def encode_frame(kind: int, stream_id: int, request_id: int, payload: bytes) -> bytes:
    if len(payload) > MAX_PAYLOAD:
        raise ClientError("native frame payload exceeds the configured maximum")
    frame = bytearray(FRAME_HEADER_SIZE + len(payload))
    struct.pack_into("<8sHBBIQI", frame, 0, b"HYPHLCL1", 1, kind, 0, stream_id, request_id, len(payload))
    frame[FRAME_HEADER_SIZE:] = payload
    struct.pack_into("<I", frame, 28, crc32c(frame))
    return bytes(frame)


def decode_frame(encoded: bytes) -> Frame:
    if len(encoded) < FRAME_HEADER_SIZE:
        raise ClientError("native frame is truncated")
    magic, major, kind, flags, stream_id, request_id, length, checksum = struct.unpack_from(
        "<8sHBBIQII", encoded
    )
    if magic != b"HYPHLCL1" or major != 1 or flags != 0 or kind not in FRAME_KINDS.values():
        raise ClientError("native frame preamble is invalid")
    if length > MAX_PAYLOAD or len(encoded) != FRAME_HEADER_SIZE + length:
        raise ClientError("native frame length is invalid")
    checked = bytearray(encoded)
    checked[28:32] = b"\0" * 4
    if crc32c(checked) != checksum:
        raise ClientError("native frame CRC32C mismatch")
    return Frame(kind, stream_id, request_id, encoded[FRAME_HEADER_SIZE:])


def encode_hello(
    client_identity: str = "hyphae-python-sdk-v2",
    *,
    maximum_minor: int = 0,
) -> bytes:
    """Encode a legacy-compatible HELLO.

    The default remains the original 1.0 byte sequence. Current transports
    opt into minor 2 explicitly so old callers and published fixtures do not
    change beneath them.
    """

    if not 0 <= maximum_minor <= PROTOCOL_MINOR:
        raise ClientError("native protocol minor is invalid")
    names = [client_identity.encode(), b"main", b"public"]
    if not names[0] or sum(map(len, names)) > 4096:
        raise ClientError("native client identity is invalid")
    total = 58 + sum(map(len, names))
    return struct.pack(
        "<8sIHHHHQQIII B3x HHH",
        b"HYPHEL01",
        total,
        PROTOCOL_MAJOR,
        PROTOCOL_MAJOR,
        0,
        maximum_minor,
        G6_CAPABILITIES,
        G6_CAPABILITIES,
        MAX_PAYLOAD,
        64,
        64 * 1024,
        1,
        *(len(name) for name in names),
    ) + b"".join(names)


def encode_authenticated_hello(
    api_key: str | bytes | bytearray,
    client_identity: str = "hyphae-python-sdk-v2",
    *,
    maximum_minor: int = PROTOCOL_MINOR,
) -> bytes:
    """Encode one bounded API-key candidate for sole-owner authentication."""

    if isinstance(api_key, str):
        try:
            authentication = api_key.encode()
        except UnicodeEncodeError as error:
            raise ClientError("local API-key credential is invalid") from error
    elif isinstance(api_key, (bytes, bytearray)):
        authentication = bytes(api_key)
    else:
        raise ClientError("local API-key credential is invalid")
    try:
        authentication.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ClientError("local API-key credential is invalid") from error
    if len(authentication) != API_KEY_BYTES:
        raise ClientError("local API-key credential is invalid")
    if not 0 <= maximum_minor <= PROTOCOL_MINOR:
        raise ClientError("native protocol minor is invalid")
    names = [client_identity.encode(), b"main", b"public"]
    if not names[0] or sum(map(len, names)) > 4096:
        raise ClientError("native client identity is invalid")
    total = 58 + sum(map(len, names)) + API_KEY_BYTES
    capabilities = G6_CAPABILITIES | API_KEY_AUTH_CAPABILITY
    return struct.pack(
        "<8sIHHHHQQIII BBH HHH",
        b"HYPHEL01",
        total,
        PROTOCOL_MAJOR,
        PROTOCOL_MAJOR,
        0,
        maximum_minor,
        capabilities,
        capabilities,
        MAX_PAYLOAD,
        64,
        64 * 1024,
        1,
        1,
        API_KEY_BYTES,
        *(len(name) for name in names),
    ) + b"".join(names) + authentication


def decode_welcome(encoded: bytes) -> dict[str, int]:
    if (
        len(encoded) != 94
        or encoded[:8] != b"HYPWEL01"
        or struct.unpack_from("<I", encoded, 8)[0] != 94
        or encoded[92:94] != b"\0\0"
    ):
        raise ClientError("native welcome is malformed")
    major, minor, capabilities = struct.unpack_from("<HHQ", encoded, 12)
    session_id = int.from_bytes(encoded[24:40], "little")
    maximum_frame_payload, maximum_in_flight, initial_window = struct.unpack_from("<III", encoded, 40)
    if (
        major != PROTOCOL_MAJOR
        or minor > PROTOCOL_MINOR
        or capabilities & ~(G6_CAPABILITIES | API_KEY_AUTH_CAPABILITY)
        or session_id == 0
        or not all((maximum_frame_payload, maximum_in_flight, initial_window))
    ):
        raise ClientError("native welcome values are invalid")
    return {
        "major": major,
        "minor": minor,
        "capabilities": capabilities,
        "session_id": session_id,
        "maximum_frame_payload": maximum_frame_payload,
        "maximum_in_flight": maximum_in_flight,
        "initial_window": initial_window,
    }


def operation_required_minor(
    operation: str, arguments: dict[str, Any] | None = None
) -> int:
    if operation == "proof_generate" and arguments is not None:
        nested = arguments.get("operation")
        return operation_required_minor(nested) if isinstance(nested, str) else 0
    if operation in SECURITY_WRITE_OPERATIONS:
        return 2
    if operation in SECURITY_READ_OPERATIONS:
        return 1
    return 0


def response_required_minor(kind: int) -> int:
    if kind in SECURITY_WRITE_RESPONSE_KINDS:
        return 2
    if kind in SECURITY_READ_RESPONSE_KINDS:
        return 1
    return 0


def encode_product_request(
    operation: str,
    arguments: dict[str, Any],
    options: RequestOptions,
    *,
    negotiated_minor: int | None = None,
) -> bytes:
    kind = REQUEST_KINDS.get(operation)
    if kind is None:
        raise ClientError(f"unsupported native operation: {operation}")
    if negotiated_minor is not None and negotiated_minor < operation_required_minor(
        operation, arguments
    ):
        raise ClientError("native operation is unavailable at the negotiated protocol minor")
    limits = options.limits
    if options.deadline_micros is not None and options.deadline_micros <= 0:
        raise ClientError("deadline_micros must be positive")
    if any(
        isinstance(value, bool) or not isinstance(value, int) or value <= 0
        for value in limits.values()
    ):
        raise ClientError("product limits must be positive integers")
    try:
        token = options.idempotency_token
        if token is not None and (isinstance(token, bool) or not isinstance(token, int) or not 0 < token < 1 << 128):
            raise ClientError("idempotency_token must be an unsigned 128-bit integer")
        if operation in SECURITY_WRITE_OPERATIONS and token is None:
            raise ClientError("security mutation requires a nonzero idempotency_token")
        values = (
            options.logical_time_micros,
            options.deadline_micros or 0,
            limits["max_count"],
            limits["max_request_bytes"],
            limits["max_response_bytes"],
            limits["max_work_units"],
            limits["max_memory_bytes"],
            {"strict": 0, "group": 1, "memory": 2}[options.durability],
        )
        context = struct.pack("<qqQQQQQ B7x", *values) if token is None else struct.pack(
            "<qq16sQQQQQ B7s",
            values[0], values[1], token.to_bytes(16, "little"), *values[2:], b"\1\0\0\0\0\0\0",
        )
    except (KeyError, struct.error) as error:
        raise ClientError("invalid request options") from error
    body = _encode_operation(operation, arguments)
    total = 16 + len(context) + len(body)
    if total > MAX_PAYLOAD:
        raise ClientError("product request exceeds the protocol maximum")
    return struct.pack("<8sIHH", b"HYPREQ01", total, kind, 0) + context + body


def decode_product_request(encoded: bytes) -> tuple[str, dict[str, Any], RequestOptions]:
    kind, payload = _envelope(encoded, b"HYPREQ01")
    if len(payload) >= 80 and payload[73:80] == b"\1\0\0\0\0\0\0":
        values = struct.unpack_from("<qq16sQQQQQB", payload)
        token, limits, durability, offset = int.from_bytes(values[2], "little") or None, values[3:8], values[8], 80
    elif len(payload) >= 64 and payload[57:64] == b"\0" * 7:
        values = struct.unpack_from("<qqQQQQQB", payload)
        token, limits, durability, offset = None, values[2:7], values[7], 64
    else:
        raise ClientError("product request context is malformed")
    options = RequestOptions(
        logical_time_micros=values[0],
        deadline_micros=values[1] or None,
        idempotency_token=token,
        limits=dict(zip(("max_count", "max_request_bytes", "max_response_bytes", "max_work_units", "max_memory_bytes"), limits)),
        durability=("strict", "group", "memory")[durability],
    )
    operation = next((name for name, value in REQUEST_KINDS.items() if value == kind), None)
    if operation is None:
        raise ClientError("unsupported product request kind")
    if operation in SECURITY_WRITE_OPERATIONS and token is None:
        raise ClientError("security mutation requires a nonzero idempotency_token")
    return operation, _decode_operation(operation, payload[offset:]), options


def _encode_operation(operation: str, arguments: dict[str, Any]) -> bytes:
    if operation in {"capabilities", "admin_status", "admin_checkpoint", "telemetry", "transaction_begin"}:
        return b""
    if operation in {"structure_get", "structure_ttl"}:
        return _bytes(arguments["key"])
    if operation == "structure_set":
        expiry = arguments.get("expires_at_micros")
        return _bytes(arguments["key"]) + _bytes(arguments["value"]) + struct.pack("<B7x", expiry is not None) + (struct.pack("<q", expiry) if expiry is not None else b"")
    if operation in {"sql_prepare", "admin_explain_sql"}:
        return _text(arguments["statement"])
    if operation == "sql_deallocate":
        return struct.pack("<Q", arguments["handle"])
    if operation == "sql_execute_prepared":
        return struct.pack("<Q", arguments["handle"]) + _encode_values(arguments.get("parameters", []))
    if operation == "sql_execute":
        return _text(arguments["statement"]) + _encode_values(arguments.get("parameters", []))
    if operation == "transaction_status":
        return int(arguments["transaction_id"]).to_bytes(16, "little")
    if operation == "transaction_stage_sql":
        return struct.pack("<Q", arguments["handle"]) + _text(arguments["statement"]) + _encode_values(arguments.get("parameters", []))
    if operation == "transaction_stage_structure":
        return struct.pack("<Q", arguments["handle"]) + _encode_structure_mutation(arguments["mutation"])
    if operation == "transaction_stage_search":
        return struct.pack("<Q", arguments["handle"]) + _encode_transaction_search_mutation(arguments["mutation"])
    if operation == "transaction_stage_vector":
        return struct.pack("<Q", arguments["handle"]) + _encode_transaction_vector_mutation(arguments["mutation"])
    if operation in {"transaction_commit", "transaction_rollback", "explicit_transaction_status"}:
        return struct.pack("<Q", arguments["handle"])
    if operation == "transaction_status_by_idempotency":
        return int(arguments["idempotency_token"]).to_bytes(16, "little")
    if operation == "doctor":
        return b""
    if operation == "backup":
        limits = arguments["limits"]
        return _text(arguments["destination"]) + struct.pack(
            "<QQQQQ",
            limits["max_files"],
            limits["max_directories"],
            limits["max_total_bytes"],
            limits["max_path_bytes"],
            limits["max_manifest_bytes"],
        )
    if operation == "search":
        return int(arguments["index"]).to_bytes(16, "little") + struct.pack("<Q", arguments["limit"]) + _encode_query(arguments["query"])
    if operation in {"catalog_object", "catalog_describe"}:
        return int(arguments["id"]).to_bytes(16, "little")
    if operation in {"catalog_object_named", "catalog_resolve"}:
        return _encode_qualified_name(arguments["name"])
    if operation == "catalog_list":
        parent = arguments.get("parent")
        kind = arguments.get("kind")
        kind_tag = 0 if kind is None else _catalog_kind_tag(kind)
        return struct.pack("<BB6x", parent is not None, kind_tag) + (
            int(parent).to_bytes(16, "little") if parent is not None else b""
        ) + _encode_cursor(arguments.get("cursor")) + struct.pack(
            "<QQQ",
            arguments["item_limit"],
            arguments["visit_limit"],
            arguments["byte_limit"],
        )
    if operation == "catalog_dependencies":
        return int(arguments["object"]).to_bytes(16, "little") + struct.pack(
            "<B7x", 0 if arguments["direction"] == "outgoing" else 1
        ) + _encode_cursor(arguments.get("cursor")) + struct.pack(
            "<QQQ",
            arguments["item_limit"],
            arguments["visit_limit"],
            arguments["byte_limit"],
        )
    if operation == "catalog_create":
        return _bytes(arguments["definition"])
    if operation == "proof_verify":
        anchor = arguments["trusted_anchor"]
        if not isinstance(anchor, bytes) or len(anchor) != 32:
            raise ClientError("trusted_anchor must contain 32 bytes")
        return _bytes(arguments["proof"]) + _bytes(arguments["witness"]) + anchor
    if operation == "search_collection":
        return _encode_search_collection(arguments)
    if operation == "search_ingest":
        return int(arguments["collection"]).to_bytes(16, "little") + _encode_search_batch(arguments["batch"])
    if operation == "search_document_update":
        return int(arguments["collection"]).to_bytes(16, "little") + int(arguments["idempotency_id"]).to_bytes(16, "little") + _encode_search_document(arguments["document"])
    if operation == "search_document_delete":
        return int(arguments["collection"]).to_bytes(16, "little") + int(arguments["idempotency_id"]).to_bytes(16, "little") + int(arguments["object_id"]).to_bytes(16, "little")
    if operation == "structure_mutate":
        mutations = arguments.get("mutations")
        if not isinstance(mutations, list) or not 0 < len(mutations) <= 4096:
            raise ClientError("structure mutations must be a nonempty bounded list")
        return struct.pack("<I", len(mutations)) + b"".join(_encode_structure_mutation(value) for value in mutations)
    if operation == "structure_read":
        return _encode_structure_read(arguments)
    if operation == "restore":
        limits = arguments["limits"]
        return _text(arguments["backup"]) + _text(arguments["destination"]) + struct.pack(
            "<QQQQQq",
            limits["max_files"],
            limits["max_directories"],
            limits["max_total_bytes"],
            limits["max_path_bytes"],
            limits["max_manifest_bytes"],
            arguments.get("doctor_logical_time_micros", 0),
        )
    if operation == "proof_generate":
        nested_operation = arguments["operation"]
        nested_arguments = arguments.get("arguments", {})
        if nested_operation == "proof_generate" or nested_operation not in REQUEST_KINDS:
            raise ClientError("nested proof operation is invalid")
        nested_body = _encode_operation(nested_operation, nested_arguments)
        limits = {**DEFAULT_PROOF_LIMITS, **arguments.get("limits", {})}
        names = tuple(DEFAULT_PROOF_LIMITS)
        try:
            encoded_limits = struct.pack("<" + "Q" * len(names), *(limits[name] for name in names))
        except (KeyError, struct.error, TypeError) as error:
            raise ClientError("proof generation limits are invalid") from error
        return struct.pack("<HH", REQUEST_KINDS[nested_operation], 0) + _bytes(nested_body) + encoded_limits
    if operation == "security_status":
        return b""
    if operation in {
        "security_principal_list",
        "security_role_list",
        "security_assignment_list",
        "security_key_list",
    }:
        family = operation.removeprefix("security_").removesuffix("_list")
        return _encode_security_cursor(arguments.get("cursor"), family) + _security_limit(
            arguments.get("limit", MAX_SECURITY_LIST_ROWS)
        )
    if operation == "security_audit_read":
        return _encode_optional_security_id(arguments.get("cursor")) + _security_limit(
            arguments.get("limit", MAX_SECURITY_LIST_ROWS)
        )
    if operation == "security_principal_create":
        return _security_text(arguments["display_name"])
    if operation == "security_principal_set_enabled":
        enabled = arguments["enabled"]
        if not isinstance(enabled, bool):
            raise ClientError("security principal enabled state must be boolean")
        return _security_id(arguments["principal_id"]) + struct.pack("<B7x", enabled)
    if operation == "security_custom_role_create":
        return _security_text(arguments["display_name"]) + _encode_security_grants(
            arguments["grants"]
        )
    if operation == "security_built_in_assignment_create":
        return (
            _security_id(arguments["principal_id"])
            + _built_in_role(arguments["role"])
            + b"\0" * 7
            + _encode_product_scope(arguments["scope"])
        )
    if operation == "security_custom_assignment_create":
        return _security_id(arguments["principal_id"]) + _security_id(arguments["role_id"])
    if operation == "security_assignment_revoke":
        return _security_id(arguments["assignment_id"])
    raise ClientError(f"binary operation encoder is not implemented for {operation}")


def _security_limit(value: Any) -> bytes:
    if isinstance(value, bool) or not isinstance(value, int) or not 0 < value <= MAX_SECURITY_LIST_ROWS:
        raise ClientError("security list limit must be between 1 and 1000")
    return struct.pack("<Q", value)


def _security_id(value: Any) -> bytes:
    if isinstance(value, bool) or not isinstance(value, int) or not 0 < value < 1 << 128:
        raise ClientError("security identity must be a nonzero unsigned 128-bit integer")
    return value.to_bytes(16, "big")


def _security_text(value: Any) -> bytes:
    if not isinstance(value, str):
        raise ClientError("security text must be a string")
    encoded = value.encode("utf-8")
    if not encoded or len(encoded) > MAX_SECURITY_TEXT_BYTES or any(
        unicodedata.category(character) == "Cc" for character in value
    ):
        raise ClientError("security text is empty, oversized, or contains control characters")
    return struct.pack("<I", len(encoded)) + encoded


def _encode_security_cursor(cursor: Any, family: str) -> bytes:
    if cursor is None:
        return b"\0" * 40
    if not isinstance(cursor, dict):
        raise ClientError("security cursor is invalid")
    epoch = cursor.get("authorization_epoch")
    if isinstance(epoch, bool) or not isinstance(epoch, int) or not 0 < epoch < 1 << 64:
        raise ClientError("security cursor authorization_epoch is invalid")
    kind = cursor.get("kind")
    after = cursor.get("after")
    expected = {
        "principal": {"principal": 1},
        "role": {"built_in_role": 2, "custom_role": 3},
        "assignment": {"assignment": 4},
        "key": {"key": 5},
    }[family]
    tag = expected.get(kind)
    if tag is None:
        raise ClientError("security cursor family is invalid")
    if kind == "built_in_role":
        payload = _built_in_role(after) + b"\0" * 15
    elif kind == "key":
        payload = _api_key_id(after)
    else:
        payload = _security_id(after)
    return struct.pack("<B7xQB7x", 1, epoch, tag) + payload


def _encode_optional_security_id(value: Any) -> bytes:
    if value is None:
        return b"\0" * 24
    return struct.pack("<B7x", 1) + _security_id(value)


def _api_key_id(value: Any) -> bytes:
    if not isinstance(value, bytes) or len(value) != 16 or value == b"\0" * 16:
        raise ClientError("API key identity must contain 16 nonzero bytes")
    return value


def _built_in_role(value: Any) -> bytes:
    try:
        return bytes((BUILT_IN_ROLES.index(value),))
    except ValueError as error:
        raise ClientError("built-in security role is invalid") from error


def _encode_product_scope(scope: Any) -> bytes:
    if not isinstance(scope, dict):
        raise ClientError("security scope is invalid")
    kind = scope.get("kind")
    if kind == "instance":
        return b"\0" * 24
    if kind in {"catalog_subtree", "catalog_object"}:
        object_id = scope.get("object_id")
        if isinstance(object_id, bool) or not isinstance(object_id, int) or not 0 < object_id < 1 << 128:
            raise ClientError("security scope object_id is invalid")
        return bytes((1 if kind == "catalog_subtree" else 2,)) + b"\0" * 7 + object_id.to_bytes(16, "little")
    raise ClientError("security scope kind is invalid")


def _encode_security_grants(value: Any) -> bytes:
    if not isinstance(value, list) or not 0 < len(value) <= MAX_SECURITY_GRANTS:
        raise ClientError("custom-role grants must be a nonempty bounded list")
    encoded: list[tuple[int, tuple[int, int], bytes]] = []
    for grant in value:
        if not isinstance(grant, dict):
            raise ClientError("custom-role grant is invalid")
        try:
            permission = PRODUCT_PERMISSIONS.index(grant["permission"])
        except (KeyError, ValueError) as error:
            raise ClientError("custom-role permission is invalid") from error
        if permission == PRODUCT_PERMISSIONS.index("ownership.manage"):
            raise ClientError("ownership.manage cannot be assigned to a custom role")
        scope = _encode_product_scope(grant["scope"])
        if scope[0] != 0 and permission not in {3, 4, 6, 7, 12, 15}:
            raise ClientError("custom-role permission does not support object scope")
        scope_order = _scope_order(grant["scope"])
        encoded.append((permission, scope_order, scope))
    identities = [(permission, scope_order) for permission, scope_order, _ in encoded]
    if identities != sorted(identities) or len(set(identities)) != len(identities):
        raise ClientError("custom-role grants must use strict canonical order")
    return struct.pack("<I4x", len(encoded)) + b"".join(
        struct.pack("<B7x", permission) + scope for permission, _, scope in encoded
    )


def _catalog_kind_tag(kind: Any) -> int:
    kinds = (
        "database",
        "schema",
        "relation",
        "secondary_index",
        "keyspace",
        "structure",
        "search_collection",
        "analyzer",
        "cross_engine_link",
    )
    try:
        return kinds.index(kind) + 1
    except ValueError as error:
        raise ClientError("catalog object kind is invalid") from error


def _encode_qualified_name(name: Any) -> bytes:
    if not isinstance(name, dict):
        raise ClientError("qualified catalog name is invalid")
    output = bytearray()
    for component in ("database", "schema", "object"):
        value = name[component]
        if not isinstance(value, dict):
            raise ClientError("qualified catalog name component is invalid")
        output.extend(_text(value["display"]))
        output.extend(_text(value["lookup"]))
    return bytes(output)


def _encode_cursor(cursor: Any) -> bytes:
    if cursor is None:
        return struct.pack("<B7x", 0)
    if not isinstance(cursor, dict):
        raise ClientError("catalog cursor is invalid")
    return struct.pack("<B7x", 1) + _encode_snapshot(cursor["snapshot"]) + int(
        cursor["after"]
    ).to_bytes(16, "little")


def _encode_snapshot(snapshot: Any) -> bytes:
    if not isinstance(snapshot, dict):
        raise ClientError("snapshot identity is invalid")
    lineage, root = snapshot["directory_lineage"], snapshot["root_digest"]
    if not isinstance(lineage, bytes) or len(lineage) != 24 or not isinstance(root, bytes) or len(root) != 32:
        raise ClientError("snapshot digest widths are invalid")
    return lineage + struct.pack(
        "<QQ", snapshot.get("visible_csn") or 0, snapshot["catalog_version"]
    ) + root + struct.pack("<q", snapshot["logical_time_micros"])


def _encode_values(values: Any) -> bytes:
    if not isinstance(values, list) or len(values) > 4096:
        raise ClientError("SQL parameters must be a bounded list")
    return struct.pack("<I", len(values)) + b"".join(_encode_value(value, 0) for value in values)


def _encode_value(value: Any, depth: int) -> bytes:
    if depth > 8:
        raise ClientError("SQL parameter nesting is too deep")
    if value is None:
        return b"\0"
    if isinstance(value, bool):
        return bytes((1, int(value)))
    if isinstance(value, int):
        if -(1 << 63) <= value < 1 << 63:
            return b"\x02" + struct.pack("<q", value)
        if 0 <= value < 1 << 64:
            return b"\x03" + struct.pack("<Q", value)
        raise ClientError("SQL integer is outside the signed/unsigned 64-bit domain")
    if isinstance(value, str):
        return b"\x07" + _text(value)
    if isinstance(value, bytes):
        return b"\x08" + _bytes(value)
    if isinstance(value, list):
        return b"\x0e" + _encode_values(value)
    if isinstance(value, dict):
        entries = list(value.items())
        return b"\x0f" + struct.pack("<I", len(entries)) + b"".join(
            _encode_value(key, depth + 1) + _encode_value(child, depth + 1)
            for key, child in entries
        )
    raise ClientError("unsupported SQL parameter type")


def _encode_query(query: Any, depth: int = 0) -> bytes:
    if depth > 8 or not isinstance(query, dict):
        raise ClientError("search query is invalid")
    kind = query.get("kind")
    if kind in {"term", "phrase", "prefix"}:
        return bytes(({"term": 0, "phrase": 1, "prefix": 2}[kind],)) + _text(query["value"])
    if kind == "fuzzy":
        return b"\x03" + bytes((query["max_distance"],)) + _text(query["term"])
    if kind == "boolean":
        groups = [query.get("must", []), query.get("should", []), query.get("must_not", [])]
        return b"\x04" + struct.pack("<III", *(len(group) for group in groups)) + b"".join(
            _encode_query(child, depth + 1) for group in groups for child in group
        )
    raise ClientError("unsupported search query kind")


def _decode_operation(operation: str, encoded: bytes) -> dict[str, Any]:
    if operation in {
        "capabilities",
        "admin_status",
        "admin_checkpoint",
        "telemetry",
        "transaction_begin",
        "security_status",
    }:
        if encoded:
            raise ClientError("parameterless request has trailing bytes")
        return {}
    if operation in {"structure_get", "structure_ttl"}:
        value, offset = _take_bytes(encoded, 0)
        if offset != len(encoded):
            raise ClientError("structure request has trailing bytes")
        return {"key": value}
    reader = _Reader(encoded)
    if operation == "catalog_create":
        result = {"definition": reader.bytes()}
    elif operation == "transaction_stage_sql":
        result = {
            "handle": reader.u64(),
            "statement": reader.text(),
            "parameters": [_decode_value(reader, 0) for _ in range(reader.u32())],
        }
    elif operation == "transaction_stage_structure":
        result = {"handle": reader.u64(), "mutation": _decode_structure_mutation(reader)}
    elif operation == "transaction_stage_search":
        result = {"handle": reader.u64(), "mutation": _decode_transaction_search_mutation(reader)}
    elif operation == "transaction_stage_vector":
        result = {"handle": reader.u64(), "mutation": _decode_transaction_vector_mutation(reader)}
    elif operation in {"transaction_commit", "transaction_rollback", "explicit_transaction_status"}:
        result = {"handle": reader.u64()}
    elif operation == "transaction_status_by_idempotency":
        result = {"idempotency_token": reader.u128()}
    elif operation == "structure_read":
        result = _decode_structure_read_request(reader)
    elif operation in {
        "security_principal_list",
        "security_role_list",
        "security_assignment_list",
        "security_key_list",
    }:
        family = operation.removeprefix("security_").removesuffix("_list")
        result = {
            "cursor": _decode_security_cursor(reader, family),
            "limit": _decode_security_limit(reader),
        }
    elif operation == "security_audit_read":
        result = {
            "cursor": _decode_optional_security_id(reader),
            "limit": _decode_security_limit(reader),
        }
    elif operation == "security_principal_create":
        result = {"display_name": _decode_security_text(reader)}
    elif operation == "security_principal_set_enabled":
        result = {
            "principal_id": _decode_security_id(reader),
            "enabled": reader.boolean(),
        }
        reader.zeroes(7)
    elif operation == "security_custom_role_create":
        result = {
            "display_name": _decode_security_text(reader),
            "grants": _decode_security_grants(reader),
        }
    elif operation == "security_built_in_assignment_create":
        result = {
            "principal_id": _decode_security_id(reader),
            "role": _decode_built_in_role(reader),
        }
        reader.zeroes(7)
        result["scope"] = _decode_product_scope(reader)
    elif operation == "security_custom_assignment_create":
        result = {
            "principal_id": _decode_security_id(reader),
            "role_id": _decode_security_id(reader),
        }
    elif operation == "security_assignment_revoke":
        result = {"assignment_id": _decode_security_id(reader)}
    else:
        raise ClientError(f"binary operation decoder is not implemented for {operation}")
    reader.finish()
    return result


def _decode_security_limit(reader: _Reader) -> int:
    limit = reader.u64()
    if not 0 < limit <= MAX_SECURITY_LIST_ROWS:
        raise ClientError("security list limit is invalid")
    return limit


def _decode_security_id(reader: _Reader) -> int:
    value = int.from_bytes(reader.take(16), "big")
    if value == 0:
        raise ClientError("security identity is zero")
    return value


def _decode_api_key_id(reader: _Reader) -> bytes:
    value = reader.take(16)
    if value == b"\0" * 16:
        raise ClientError("API key identity is zero")
    return value


def _decode_security_text(reader: _Reader) -> str:
    length = reader.u32()
    if not 0 < length <= MAX_SECURITY_TEXT_BYTES:
        raise ClientError("security text length is invalid")
    try:
        value = reader.take(length).decode("utf-8")
    except UnicodeDecodeError as error:
        raise ClientError("security text is not valid UTF-8") from error
    if any(unicodedata.category(character) == "Cc" for character in value):
        raise ClientError("security text contains control characters")
    return value


def _decode_security_cursor(reader: _Reader, family: str) -> dict[str, Any] | None:
    present = reader.boolean()
    reader.zeroes(7)
    epoch = reader.u64()
    kind = reader.u8()
    reader.zeroes(7)
    payload = reader.take(16)
    if not present:
        if epoch != 0 or kind != 0 or payload != b"\0" * 16:
            raise ClientError("absent security cursor is noncanonical")
        return None
    if epoch == 0:
        raise ClientError("security cursor epoch is zero")
    expected = {
        "principal": {1: "principal"},
        "role": {2: "built_in_role", 3: "custom_role"},
        "assignment": {4: "assignment"},
        "key": {5: "key"},
    }[family]
    cursor_kind = expected.get(kind)
    if cursor_kind is None:
        raise ClientError("security cursor family is invalid")
    if cursor_kind == "built_in_role":
        if payload[1:] != b"\0" * 15 or payload[0] >= len(BUILT_IN_ROLES):
            raise ClientError("security role cursor is invalid")
        after: Any = BUILT_IN_ROLES[payload[0]]
    elif cursor_kind == "key":
        if payload == b"\0" * 16:
            raise ClientError("security key cursor is zero")
        after = payload
    else:
        after = int.from_bytes(payload, "big")
        if after == 0:
            raise ClientError("security cursor identity is zero")
    return {"authorization_epoch": epoch, "kind": cursor_kind, "after": after}


def _decode_optional_security_id(reader: _Reader) -> int | None:
    present = reader.boolean()
    reader.zeroes(7)
    payload = reader.take(16)
    value = int.from_bytes(payload, "big")
    if not present:
        if value != 0:
            raise ClientError("absent security identity is noncanonical")
        return None
    if value == 0:
        raise ClientError("security identity is zero")
    return value


def _decode_built_in_role(reader: _Reader) -> str:
    tag = reader.u8()
    if tag >= len(BUILT_IN_ROLES):
        raise ClientError("built-in security role is invalid")
    return BUILT_IN_ROLES[tag]


def _decode_product_scope(reader: _Reader) -> dict[str, object]:
    kind = reader.u8()
    reader.zeroes(7)
    payload = reader.take(16)
    if kind == 0:
        if payload != b"\0" * 16:
            raise ClientError("instance scope has a nonzero identity")
        return {"kind": "instance"}
    object_id = int.from_bytes(payload, "little")
    if kind not in (1, 2) or object_id == 0:
        raise ClientError("security object scope is invalid")
    return {
        "kind": "catalog_subtree" if kind == 1 else "catalog_object",
        "object_id": object_id,
    }


def _decode_security_grants(reader: _Reader) -> list[dict[str, object]]:
    count = reader.u32()
    reader.zeroes(4)
    if not 0 < count <= MAX_SECURITY_GRANTS:
        raise ClientError("custom-role grant count is invalid")
    grants: list[dict[str, object]] = []
    canonical: list[tuple[int, tuple[int, int]]] = []
    for _ in range(count):
        permission = reader.u8()
        reader.zeroes(7)
        if permission >= len(PRODUCT_PERMISSIONS):
            raise ClientError("custom-role permission is invalid")
        scope = _decode_product_scope(reader)
        if permission == 11 or (
            scope["kind"] != "instance" and permission not in {3, 4, 6, 7, 12, 15}
        ):
            raise ClientError("custom-role grant scope is invalid")
        canonical.append((permission, _scope_order(scope)))
        grants.append({"permission": PRODUCT_PERMISSIONS[permission], "scope": scope})
    if canonical != sorted(canonical) or len(set(canonical)) != len(canonical):
        raise ClientError("custom-role grants are noncanonical")
    return grants


def _decode_structure_key(reader: _Reader) -> dict[str, Any]:
    return {"keyspace": reader.u128(), "key": reader.bytes()}


def _decode_structure_mutation(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    kinds = (
        "string_set", "string_delete", "counter_add", "create", "delete", "expire",
        "hash_set", "hash_delete", "hash_counter_add", "hash_expire_field", "list_push",
        "list_pop", "set_add", "set_remove", "sorted_set_add", "sorted_set_remove", "stream_add",
    )
    if tag >= len(kinds):
        raise ClientError("structure mutation kind is invalid")
    kind = kinds[tag]
    result: dict[str, Any] = {"kind": kind, "key": _decode_structure_key(reader)}
    families = (None, "string", "counter", "hash", "list", "set", "sorted_set", "stream")
    if kind == "string_set":
        result["value"] = reader.bytes()
        result["expires_at_micros"] = reader.i64() if reader.boolean() else None
    elif kind == "counter_add":
        result["delta"] = reader.i64()
    elif kind in {"create", "delete"}:
        family = reader.u8()
        if family == 0 or family >= len(families):
            raise ClientError("structure family is invalid")
        result["family"] = families[family]
    elif kind == "expire":
        family = reader.u8()
        if family == 0 or family >= len(families):
            raise ClientError("structure family is invalid")
        result.update(family=families[family], expires_at_micros=reader.i64())
    elif kind in {"hash_set", "hash_delete", "hash_counter_add", "hash_expire_field"}:
        result["field"] = reader.bytes()
        if kind == "hash_set":
            result["value"] = reader.bytes()
        elif kind == "hash_counter_add":
            result["delta"] = reader.i64()
        elif kind == "hash_expire_field":
            result["expires_at_micros"] = reader.i64()
    elif kind in {"list_push", "list_pop"}:
        side = reader.u8()
        if side > 1:
            raise ClientError("list side is invalid")
        result["side"] = ("left", "right")[side]
        if kind == "list_push":
            result["value"] = reader.bytes()
    elif kind in {"set_add", "set_remove", "sorted_set_remove"}:
        result["member"] = reader.bytes()
    elif kind == "sorted_set_add":
        result.update(score=reader.f64(), member=reader.bytes())
    elif kind == "stream_add":
        result["fields"] = [(reader.bytes(), reader.bytes()) for _ in range(reader.u32())]
    return result


def _decode_transaction_search_mutation(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag > 2:
        raise ClientError("transaction search mutation kind is invalid")
    result = {"kind": ("index", "replace", "delete")[tag], "index": reader.u128(), "document_id": reader.bytes()}
    if tag != 2:
        result["text"] = reader.text()
    return result


def _decode_transaction_vector_mutation(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag > 1:
        raise ClientError("transaction vector mutation kind is invalid")
    result = {"kind": ("upsert", "delete")[tag], "index": reader.u128(), "object_id": reader.u128()}
    if tag == 0:
        result["vector"] = [reader.f32() for _ in range(reader.u32())]
    return result


def decode_product_response(
    encoded: bytes,
    request_id: int,
    *,
    negotiated_minor: int | None = None,
) -> Response:
    kind, payload = _envelope(encoded, b"HYPRSP01")
    if negotiated_minor is not None and negotiated_minor < response_required_minor(kind):
        raise ClientError("native response is unavailable at the negotiated protocol minor")
    reader = _Reader(payload)
    if kind == 1:
        value = {
            "product_api_version": reader.u16(),
            "native_directory_format": reader.u16(),
            "logical_catalog_codec_version": reader.u16(),
            "catalog_tree_format_version": reader.u16(),
            "max_catalog_items": reader.u64(),
            "max_catalog_visits": reader.u64(),
            "max_catalog_bytes": reader.u64(),
            "max_sql_statement_bytes": reader.u64(),
            "max_sql_parameters": reader.u64(),
            "max_sql_rows": reader.u64(),
        }
        reader.finish()
        return Response("capabilities", value, request_id)
    if kind == 2:
        value = {
            "handle": reader.u64(),
            "catalog_version": reader.u64(),
            "parameter_count": reader.u64(),
            "maximum_result_rows": reader.u64(),
        }
        reader.finish()
        return Response("prepared_sql", value, request_id)
    if kind == 3:
        flags = reader.u8()
        reader.zeroes(7)
        snapshot = _decode_snapshot(reader) if flags & 1 else None
        commit = _decode_commit_outcome(reader) if flags & 2 else None
        value = {"result": _decode_sql_result(reader), "snapshot": snapshot, "commit": commit}
        reader.finish()
        return Response("sql", value, request_id)
    if kind == 4:
        present = reader.u8()
        reader.zeroes(3)
        value = reader.bytes() if present == 1 else None
        if present not in (0, 1):
            raise ClientError("structure response is malformed")
        reader.finish()
        return Response("structure_value", value, request_id)
    if kind == 5:
        value = _decode_commit_outcome(reader)
        reader.finish()
        return Response("structure_set", value, request_id)
    if kind == 6:
        tag = reader.u8()
        value = {0: {"state": "missing"}, 1: {"state": "persistent"}}.get(tag)
        if tag == 2:
            value = {"state": "remaining", "remaining_micros": reader.i64()}
        if value is None:
            raise ClientError("structure TTL response is malformed")
        reader.finish()
        return Response("structure_ttl", value, request_id)
    if kind == 7:
        value = _decode_transaction_status(reader)
        reader.finish()
        return Response("transaction_status", value, request_id)
    if kind == 8:
        count = reader.u32()
        reader.zeroes(4)
        documents_examined = reader.u64()
        source_bytes = reader.u64()
        token_visits = reader.u64()
        token_comparisons = reader.u64()
        fuzzy_steps = reader.u64()
        value = {
            "documents_examined": documents_examined,
            "source_bytes": source_bytes,
            "token_visits": token_visits,
            "token_comparisons": token_comparisons,
            "fuzzy_steps": fuzzy_steps,
            "hits": [
                {"document_id": reader.bytes(), "score": reader.f64()}
                for _ in range(count)
            ],
        }
        reader.finish()
        return Response("search", value, request_id)
    if kind == 9:
        snapshot = _decode_snapshot(reader)
        fields = [reader.u64() for _ in range(10)]
        value = {
            "snapshot": snapshot,
            "snapshot_pin_count": fields[0],
            "physical": {
                "page_count": fields[1],
                "physical_page_reads": fields[2],
                "wal_bytes": fields[3],
                "process_full_state_loads": fields[4],
                "process_full_catalog_loads": fields[5],
            },
            "retained_wal_bytes": fields[6],
            "replayed_transactions": fields[7],
            "manifest_count": fields[8],
            "blob_count": fields[9],
        }
        reader.finish()
        return Response("admin_status", value, request_id)
    if kind == 10:
        value = {
            "transaction_id": reader.u128(),
            "visible_csn": reader.u64(),
            "manifest_generation": reader.u64(),
            "manifest_digest": reader.take(32),
            "checkpoint_lsn": reader.u64(),
            "parent_directory_sync_supported": reader.boolean(),
        }
        reader.finish()
        return Response("admin_checkpoint", value, request_id)
    if kind == 11:
        reader.finish()
        return Response("deallocated", None, request_id)
    if kind == 12:
        value = {"snapshot": _decode_snapshot(reader), "definition": reader.bytes()}
        reader.finish()
        return Response("catalog_object", value, request_id)
    if kind == 13:
        value = _decode_catalog_page(reader, dependencies=False)
        reader.finish()
        return Response("catalog_page", value, request_id)
    if kind == 14:
        value = _decode_catalog_page(reader, dependencies=True)
        reader.finish()
        return Response("catalog_dependency_page", value, request_id)
    if kind == 15:
        present = reader.u8()
        reader.zeroes(3)
        value = reader.bytes() if present == 1 else None
        if present not in (0, 1):
            raise ClientError("catalog definition response is malformed")
        reader.finish()
        return Response("catalog_definition", value, request_id)
    if kind == 16:
        value = _decode_commit_outcome(reader)
        reader.finish()
        return Response("catalog_created", value, request_id)
    if kind == 17:
        if reader.u8() != 0:
            raise ClientError("explain response is not an SQL plan")
        reader.zeroes(3)
        value = {"version": reader.u16()}
        if reader.u16() != 0:
            raise ClientError("explain response is malformed")
        value["visible_csn"] = reader.u64() or None
        value["catalog_version"] = reader.u64()
        value["executed"] = reader.boolean()
        reader.zeroes(7)
        value["text"] = reader.text()
        reader.finish()
        return Response("explain", value, request_id)
    if kind == 18:
        value = _decode_doctor(reader)
        reader.finish()
        return Response("doctor", value, request_id)
    if kind == 19:
        value = {
            "path": reader.text(),
            "visible_csn": reader.u64(),
            "checkpoint_digest": reader.take(32),
            "file_count": reader.u64(),
            "total_bytes": reader.u64(),
        }
        reader.finish()
        return Response("backup", value, request_id)
    if kind == 20:
        value = _decode_telemetry(reader)
        reader.finish()
        return Response("telemetry", value, request_id)
    if kind == 21:
        proof_kinds = ("point", "sql", "lexical", "exact_vector", "ann", "hybrid", "catalog")
        proof_kind = reader.u8()
        semantic = reader.boolean()
        reader.zeroes(6)
        if not 1 <= proof_kind <= len(proof_kinds):
            raise ClientError("proof verification kind is invalid")
        value = {
            "scope": "artifact_integrity",
            "kind": proof_kinds[proof_kind - 1],
            "semantic_reexecution_performed": semantic,
            "anchor_digest": reader.take(32),
            "proof_digest": reader.take(32),
            "witness_digest": reader.take(32),
            "request_digest": reader.take(32),
            "result_digest": reader.take(32),
            "evidence_digest": reader.take(32),
            "file_count": reader.u64(),
            "directory_count": reader.u64(),
            "total_file_bytes": reader.u64(),
        }
        reader.finish()
        return Response("proof_verification", value, request_id)
    if kind == 22:
        value = _decode_integrated_search(reader)
        reader.finish()
        return Response("integrated_search", value, request_id)
    if kind == 23:
        value = _decode_commit_outcome(reader)
        reader.finish()
        return Response("structure_mutated", value, request_id)
    if kind == 24:
        value = {"snapshot": _decode_snapshot(reader), "result": _decode_structure_read(reader)}
        reader.finish()
        return Response("structure_read", value, request_id)
    if kind == 25:
        value = {
            "data_path": reader.text(),
            "backup": {
                "path": reader.text(),
                "visible_csn": reader.u64(),
                "checkpoint_digest": reader.take(32),
                "file_count": reader.u64(),
                "total_bytes": reader.u64(),
            },
            "doctor": _decode_doctor(reader),
        }
        phase_count = reader.u32()
        value["phases"] = [reader.u8() for _ in range(phase_count)]
        reader.finish()
        return Response("restore", value, request_id)
    if kind == 27:
        value = _decode_explicit_transaction_status(reader)
        reader.finish()
        return Response("explicit_transaction_status", value, request_id)
    if kind == 28:
        value = {
            "handle": reader.u64(),
            "operation_ordinal": reader.u64(),
            "changed": reader.boolean(),
            "result": _decode_transaction_stage_result(reader),
        }
        reader.finish()
        return Response("transaction_staged", value, request_id)
    if kind == 29:
        value = {"handle": reader.u64(), "staged_operations": reader.u64(), "commit": _decode_commit_receipt(reader)}
        reader.finish()
        return Response("transaction_committed", value, request_id)
    if kind == 30:
        value = {"handle": reader.u64(), "discarded_operations": reader.u64()}
        reader.finish()
        return Response("transaction_rolled_back", value, request_id)
    if kind == 26:
        snapshot = _decode_snapshot(reader)
        has_commit, replay = reader.boolean(), reader.boolean()
        reader.zeroes(6)
        value = {
            "snapshot": snapshot,
            "documents": reader.u64(),
            "idempotent_replay": replay,
            "commit": _decode_commit_receipt(reader) if has_commit else None,
        }
        reader.finish()
        return Response("search_ingested", value, request_id)
    if kind == 31:
        nested = decode_product_response(
            reader.bytes(), request_id, negotiated_minor=negotiated_minor
        )
        value = {
            "response": nested,
            "proof": reader.bytes(),
            "witness": reader.bytes(),
            "trusted_anchor": reader.take(32),
        }
        reader.finish()
        return Response("proven", value, request_id)
    if kind == 32:
        value = _decode_security_status(reader)
        reader.finish()
        return Response("security_status", value, request_id)
    if kind in (33, 34, 35, 36):
        family = ("principal", "role", "assignment", "key")[kind - 33]
        value = _decode_security_page(reader, family)
        reader.finish()
        return Response(f"security_{family}_page", value, request_id)
    if kind == 37:
        value = _decode_security_audit_page(reader)
        reader.finish()
        return Response("security_audit_page", value, request_id)
    if kind in (38, 39, 40):
        identity_name = ("principal_id", "role_id", "assignment_id")[kind - 38]
        value = {
            identity_name: _decode_security_id(reader),
            "authorization_epoch": _decode_authorization_epoch(reader),
            "commit": _decode_commit_receipt(reader),
        }
        reader.finish()
        response_name = (
            "security_principal_mutated",
            "security_custom_role_mutated",
            "security_assignment_mutated",
        )[kind - 38]
        return Response(response_name, value, request_id)
    if kind == 41:
        value = {
            "authorization_epoch": _decode_authorization_epoch(reader),
            "commit": _decode_commit_receipt(reader),
        }
        reader.finish()
        return Response("security_mutated", value, request_id)
    raise ClientError(f"unsupported product response kind: {kind}")


def _decode_authorization_epoch(reader: _Reader) -> int:
    value = reader.u64()
    if value == 0:
        raise ClientError("authorization epoch is zero")
    return value


def _decode_security_status(reader: _Reader) -> dict[str, Any]:
    bootstrapped = reader.boolean()
    reader.zeroes(7)
    epoch = reader.u64()
    names = (
        "principals",
        "assignments",
        "custom_roles",
        "custom_assignments",
        "keys",
        "pending_keys",
        "audit_events",
    )
    counts = dict(zip(names, (reader.u64() for _ in names)))
    empty = all(count == 0 for count in counts.values())
    if bootstrapped:
        valid = epoch != 0 and counts["principals"] > 0 and counts["assignments"] > 0
    else:
        valid = epoch == 0 and empty
    maximum_assignments = counts["principals"] * MAX_SECURITY_ASSIGNMENTS
    if (
        not valid
        or counts["principals"] > 4096
        or counts["assignments"] + counts["custom_assignments"] > maximum_assignments
        or counts["custom_roles"] > 1024
        or counts["keys"] > counts["principals"] * 64
        or counts["pending_keys"] > counts["keys"]
        or counts["audit_events"] > 100_000
    ):
        raise ClientError("security status is invalid")
    return {"bootstrapped": bootstrapped, "authorization_epoch": epoch, **counts}


def _decode_security_page(reader: _Reader, family: str) -> dict[str, Any]:
    epoch = _decode_authorization_epoch(reader)
    count = reader.u32()
    reader.zeroes(4)
    if count > MAX_SECURITY_LIST_ROWS:
        raise ClientError("security page item count is invalid")
    cursor = _decode_security_cursor(reader, family)
    if cursor is not None and cursor["authorization_epoch"] != epoch:
        raise ClientError("security page cursor epoch differs from its page")
    decoders = {
        "principal": _decode_security_principal,
        "role": _decode_security_role,
        "assignment": _decode_security_assignment,
        "key": _decode_security_key,
    }
    items = [decoders[family](reader) for _ in range(count)]
    identities = [_security_item_order(item, family) for item in items]
    if identities != sorted(identities) or len(set(identities)) != len(identities):
        raise ClientError("security page items are noncanonical")
    if cursor is not None and (
        not items or _security_cursor_order(cursor, family) != identities[-1]
    ):
        raise ClientError("security page cursor does not identify its final item")
    return {"authorization_epoch": epoch, "items": items, "next_cursor": cursor}


def _decode_security_principal(reader: _Reader) -> dict[str, Any]:
    identity = _decode_security_id(reader)
    enabled = reader.boolean()
    reader.zeroes(7)
    return {
        "id": identity,
        "display_name": _decode_security_text(reader),
        "enabled": enabled,
    }


def _decode_security_role(reader: _Reader) -> dict[str, Any]:
    kind = reader.u8()
    if kind == 0:
        role = _decode_built_in_role(reader)
        reader.zeroes(6)
        return {"kind": "built_in", "role": role, "display_name": role, "grants": []}
    if kind == 1:
        reader.zeroes(7)
        return {
            "kind": "custom",
            "id": _decode_security_id(reader),
            "display_name": _decode_security_text(reader),
            "grants": _decode_security_grants(reader),
        }
    raise ClientError("security role kind is invalid")


def _decode_security_assignment(reader: _Reader) -> dict[str, Any]:
    identity = _decode_security_id(reader)
    principal_id = _decode_security_id(reader)
    kind = reader.u8()
    if kind == 0:
        role = _decode_built_in_role(reader)
        reader.zeroes(6)
        scope = _decode_product_scope(reader)
        if role == "owner" and scope != {"kind": "instance"}:
            raise ClientError("owner assignment is not instance-scoped")
        return {
            "id": identity,
            "principal_id": principal_id,
            "kind": "built_in",
            "role": role,
            "scope": scope,
        }
    if kind == 1:
        reader.zeroes(7)
        return {
            "id": identity,
            "principal_id": principal_id,
            "kind": "custom",
            "role_id": _decode_security_id(reader),
        }
    raise ClientError("security assignment kind is invalid")


def _decode_security_key(reader: _Reader) -> dict[str, Any]:
    identity = _decode_api_key_id(reader)
    principal_id = _decode_security_id(reader)
    flags = reader.u8()
    reader.zeroes(7)
    if flags & ~3:
        raise ClientError("security key flags are invalid")
    label = _decode_security_text(reader)
    role_count = reader.u32()
    reader.zeroes(4)
    if role_count > len(BUILT_IN_ROLES):
        raise ClientError("security key role count is invalid")
    roles = [_decode_built_in_role(reader) for _ in range(role_count)]
    if roles != sorted(roles, key=BUILT_IN_ROLES.index) or len(set(roles)) != len(roles):
        raise ClientError("security key roles are noncanonical")
    custom_count = reader.u32()
    reader.zeroes(4)
    if custom_count > MAX_SECURITY_ASSIGNMENTS:
        raise ClientError("security key custom-role count is invalid")
    custom_roles = [_decode_security_id(reader) for _ in range(custom_count)]
    if custom_roles != sorted(custom_roles) or len(set(custom_roles)) != len(custom_roles):
        raise ClientError("security key custom roles are noncanonical")
    if not roles and not custom_roles:
        raise ClientError("security key selects no roles")
    permission_ceiling = reader.u64()
    if permission_ceiling >> len(PRODUCT_PERMISSIONS):
        raise ClientError("security key permission ceiling has unknown bits")
    scope_count = reader.u32()
    reader.zeroes(4)
    if not 0 < scope_count <= MAX_SECURITY_ASSIGNMENTS:
        raise ClientError("security key scope count is invalid")
    scopes = [_decode_product_scope(reader) for _ in range(scope_count)]
    scope_order = [_scope_order(scope) for scope in scopes]
    if scope_order != sorted(scope_order) or len(set(scope_order)) != len(scope_order):
        raise ClientError("security key scopes are noncanonical")
    if {"kind": "instance"} in scopes and len(scopes) != 1:
        raise ClientError("instance security scope must be the only ceiling")
    created_at = reader.i64()
    expires_at = _decode_fixed_optional_i64(reader)
    published_epoch = _decode_authorization_epoch(reader)
    predecessor_id = _decode_optional_api_key_id(reader)
    successor_id = _decode_optional_api_key_id(reader)
    overlap_until = _decode_fixed_optional_i64(reader)
    rotation_overlap = _decode_optional_u64(reader)
    if expires_at is not None and expires_at <= created_at:
        raise ClientError("security key expiry is invalid")
    if overlap_until is not None and overlap_until <= created_at:
        raise ClientError("security key overlap deadline is invalid")
    if rotation_overlap is not None and rotation_overlap > 604_800_000_000:
        raise ClientError("security key rotation overlap is invalid")
    rotation_shape = (
        (predecessor_id is None and overlap_until is None and rotation_overlap is None)
        or (
            predecessor_id is not None
            and successor_id is None
            and overlap_until is None
            and rotation_overlap is not None
        )
        or (
            predecessor_id is None
            and successor_id is not None
            and overlap_until is not None
            and rotation_overlap is None
        )
    )
    if not rotation_shape or identity in {predecessor_id, successor_id}:
        raise ClientError("security key rotation links are invalid")
    return {
        "id": identity,
        "principal_id": principal_id,
        "label": label,
        "active": bool(flags & 1),
        "revoked": bool(flags & 2),
        "roles": roles,
        "custom_roles": custom_roles,
        "permission_ceiling": [
            permission
            for tag, permission in enumerate(PRODUCT_PERMISSIONS)
            if permission_ceiling & 1 << tag
        ],
        "scope_ceiling": scopes,
        "created_at_micros": created_at,
        "expires_at_micros": expires_at,
        "published_epoch": published_epoch,
        "predecessor_id": predecessor_id,
        "successor_id": successor_id,
        "overlap_until_micros": overlap_until,
        "rotation_overlap_micros": rotation_overlap,
    }


def _decode_security_audit_page(reader: _Reader) -> dict[str, Any]:
    count = reader.u32()
    reader.zeroes(4)
    if count > MAX_SECURITY_LIST_ROWS:
        raise ClientError("security audit page count is invalid")
    cursor = _decode_optional_security_id(reader)
    events = [_decode_security_audit_event(reader) for _ in range(count)]
    ids = [event["id"] for event in events]
    commit_csns = [event["commit_csn"] for event in events]
    if len(set(ids)) != len(ids) or commit_csns != sorted(commit_csns) or len(set(commit_csns)) != len(commit_csns):
        raise ClientError("security audit events are noncanonical")
    if cursor is not None and (not events or cursor != events[-1]["id"]):
        raise ClientError("security audit cursor does not identify its final event")
    return {"events": events, "next_cursor": cursor}


def _decode_security_audit_event(reader: _Reader) -> dict[str, Any]:
    identity = _decode_security_id(reader)
    commit_csn = reader.u64()
    if commit_csn == 0:
        raise ClientError("security audit commit CSN is zero")
    has_actor = reader.boolean()
    reader.zeroes(7)
    principal_wire, key_wire = reader.take(16), reader.take(16)
    if has_actor:
        actor_principal_id = int.from_bytes(principal_wire, "big")
        if actor_principal_id == 0 or key_wire == b"\0" * 16:
            raise ClientError("security audit actor is invalid")
        actor_key_id: bytes | None = key_wire
    elif principal_wire == b"\0" * 16 and key_wire == b"\0" * 16:
        actor_principal_id = None
        actor_key_id = None
    else:
        raise ClientError("absent security audit actor is noncanonical")
    action_tag = reader.u8()
    result_tag = reader.u8()
    reader.zeroes(6)
    if action_tag >= len(SECURITY_AUDIT_ACTIONS) or result_tag != 0:
        raise ClientError("security audit action or result is invalid")
    target_count = reader.u32()
    reader.zeroes(4)
    if not 0 < target_count <= MAX_SECURITY_ASSIGNMENTS:
        raise ClientError("security audit target count is invalid")
    targets = []
    for _ in range(target_count):
        target_kind = reader.u8()
        reader.zeroes(7)
        target_wire = reader.take(16)
        if target_kind > 3 or target_wire == b"\0" * 16:
            raise ClientError("security audit target is invalid")
        targets.append({
            "kind": ("principal", "role", "assignment", "key")[target_kind],
            "id": target_wire if target_kind == 3 else int.from_bytes(target_wire, "big"),
        })
    metadata_count = reader.u32()
    reader.zeroes(4)
    if metadata_count > MAX_SECURITY_ASSIGNMENTS:
        raise ClientError("security audit metadata count is invalid")
    metadata = []
    for _ in range(metadata_count):
        metadata_kind = reader.u8()
        reader.zeroes(7)
        if metadata_kind > 1:
            raise ClientError("security audit metadata is invalid")
        metadata.append({
            "kind": ("expires_at_micros", "rotation_overlap_until_micros")[metadata_kind],
            "value": reader.i64(),
        })
    return {
        "id": identity,
        "commit_csn": commit_csn,
        "actor_principal_id": actor_principal_id,
        "actor_key_id": actor_key_id,
        "action": SECURITY_AUDIT_ACTIONS[action_tag],
        "result": "succeeded",
        "targets": targets,
        "metadata": metadata,
    }


def _decode_optional_api_key_id(reader: _Reader) -> bytes | None:
    present = reader.boolean()
    reader.zeroes(7)
    value = reader.take(16)
    if not present:
        if value != b"\0" * 16:
            raise ClientError("absent API key identity is noncanonical")
        return None
    if value == b"\0" * 16:
        raise ClientError("API key identity is zero")
    return value


def _decode_fixed_optional_i64(reader: _Reader) -> int | None:
    present = reader.boolean()
    reader.zeroes(7)
    value = reader.i64()
    if not present:
        if value != 0:
            raise ClientError("absent optional instant is noncanonical")
        return None
    return value


def _decode_optional_u64(reader: _Reader) -> int | None:
    present = reader.boolean()
    reader.zeroes(7)
    value = reader.u64()
    if not present:
        if value != 0:
            raise ClientError("absent optional integer is noncanonical")
        return None
    return value


def _scope_order(scope: dict[str, object]) -> tuple[int, int]:
    return (
        ("instance", "catalog_subtree", "catalog_object").index(str(scope["kind"])),
        int(scope.get("object_id", 0)),
    )


def _security_item_order(item: dict[str, Any], family: str) -> tuple[int, int | bytes]:
    if family == "role" and item["kind"] == "built_in":
        return (0, BUILT_IN_ROLES.index(item["role"]))
    if family == "role":
        return (1, item["id"])
    return (0, item["id"])


def _security_cursor_order(cursor: dict[str, Any], family: str) -> tuple[int, int | bytes]:
    if family == "role" and cursor["kind"] == "built_in_role":
        return (0, BUILT_IN_ROLES.index(cursor["after"]))
    if family == "role":
        return (1, cursor["after"])
    return (0, cursor["after"])


def _decode_snapshot(reader: _Reader) -> dict[str, Any]:
    lineage = reader.take(24)
    visible = reader.u64()
    return {
        "directory_lineage": lineage,
        "visible_csn": visible or None,
        "catalog_version": reader.u64(),
        "root_digest": reader.take(32),
        "logical_time_micros": reader.i64(),
    }


def _encode_search_collection(arguments: dict[str, Any]) -> bytes:
    request = arguments["request"]
    output = bytearray()
    output.extend(int(arguments["collection"]).to_bytes(16, "little"))
    lexical = request.get("lexical")
    output.extend(struct.pack("<B7x", lexical is not None))
    if lexical is not None:
        output.extend(_text(lexical["query"]))
        output.extend(struct.pack("<QI", lexical["candidate_limit"], lexical["weight"]))
    vectors = request.get("vectors", [])
    output.extend(struct.pack("<I", len(vectors)))
    for vector in vectors:
        values = vector["query"]
        output.extend(_text(vector["target"]))
        output.extend(struct.pack("<I", len(values)))
        output.extend(struct.pack(f"<{len(values)}f", *values))
        output.extend(struct.pack("<QI", vector["candidate_limit"], vector["weight"]))
        execution = vector.get("execution")
        kind = execution["kind"] if execution is not None else "catalog"
        if kind == "catalog":
            output.extend(struct.pack("<B7x", 0))
        elif kind == "exact":
            output.extend(struct.pack("<B7x", 1))
        elif kind == "ann":
            rerank = execution.get("exact_rerank")
            output.extend(struct.pack("<BB6xQQ", 2, rerank is not None, execution["ef_search"], rerank or 0))
        elif kind == "adaptive":
            rerank = execution.get("exact_rerank")
            output.extend(struct.pack("<BB6xQQQ", 3, rerank is not None, execution["exact_candidate_threshold"], execution["ef_search"], rerank or 0))
        else:
            raise ClientError("integrated vector execution is invalid")
    output.extend(_encode_search_filter(request.get("filter", {"kind": "match_all"})))
    sorts = request.get("sort", [])
    output.extend(struct.pack("<I", len(sorts)))
    for sort in sorts:
        source = sort["source"]
        if source["kind"] == "score":
            output.append(0)
        elif source["kind"] == "field":
            output.append(1)
            output.extend(_text(source["field"]))
        else:
            raise ClientError("integrated sort source is invalid")
        output.extend(bytes((("ascending", "descending").index(sort["direction"]), ("first", "last").index(sort["missing"]))))
    facets = request.get("facets", [])
    output.extend(struct.pack("<I", len(facets)))
    for facet in facets:
        output.extend(_text(facet["field"]))
        output.extend(struct.pack("<Q", facet["limit"]))
    aggregations = request.get("aggregations", [])
    output.extend(struct.pack("<I", len(aggregations)))
    for aggregation in aggregations:
        output.extend(_text(aggregation["name"]))
        kind = aggregation["kind"]
        if kind == "count":
            output.append(0)
        elif kind in {"sum", "min", "max"}:
            output.append({"sum": 1, "min": 2, "max": 3}[kind])
            output.extend(_text(aggregation["field"]))
        else:
            raise ClientError("integrated aggregation is invalid")
    output.extend(struct.pack("<Q", request["limit"]))
    return bytes(output)


def _encode_search_batch(batch: Any) -> bytes:
    documents = batch["documents"]
    return int(batch["idempotency_id"]).to_bytes(16, "little") + struct.pack("<I", len(documents)) + b"".join(_encode_search_document(document) for document in documents)


def _encode_search_document(document: Any) -> bytes:
    output = bytearray(int(document["object_id"]).to_bytes(16, "little"))
    output.extend(_text(document["text"]))
    values = document.get("doc_values", {})
    output.extend(struct.pack("<I", len(values)))
    for name in sorted(values):
        output.extend(_text(name))
        output.extend(_encode_doc_value(values[name]))
    vectors = document.get("vectors", {})
    output.extend(struct.pack("<I", len(vectors)))
    for name in sorted(vectors):
        vector = vectors[name]
        output.extend(_text(name))
        output.extend(struct.pack("<I", len(vector)))
        output.extend(struct.pack(f"<{len(vector)}f", *vector))
    return bytes(output)


def _decode_integrated_search(reader: _Reader) -> dict[str, Any]:
    snapshot = _decode_snapshot(reader)
    hits = []
    for _ in range(reader.u32()):
        object_id, score = reader.u128(), reader.f64()
        values = {}
        for _ in range(reader.u32()):
            name, tag = reader.text(), reader.u8()
            if tag == 0:
                value = reader.boolean()
            elif tag == 1:
                value = reader.i64()
            elif tag == 2:
                value = reader.text()
            elif tag == 3:
                value = reader.bytes()
            else:
                raise ClientError("integrated doc value is invalid")
            values[name] = value
        hits.append({"object_id": object_id, "score": score, "doc_values": values})
    facets = []
    for _ in range(reader.u32()):
        field = reader.text()
        buckets = [
            {"value": _decode_doc_value(reader), "count": reader.u64()}
            for _ in range(reader.u32())
        ]
        facets.append({"field": field, "buckets": buckets})
    aggregations = []
    for _ in range(reader.u32()):
        aggregations.append({"name": reader.text(), "value": _decode_aggregation_value(reader)})
    strategies = ("exact_filtered", "adaptive_exact_filtered", "filter_aware_ann", "adaptive_filter_aware_ann")
    branches = []
    for _ in range(reader.u32()):
        target, strategy = reader.text(), reader.u8()
        if strategy >= len(strategies):
            raise ClientError("integrated vector strategy is invalid")
        approximate, reranked = reader.boolean(), reader.boolean()
        reader.zeroes(5)
        branches.append({
            "target": target,
            "strategy": strategies[strategy],
            "approximate": approximate,
            "exact_reranked": reranked,
            "eligible_documents": reader.u64(),
            "candidate_count": reader.u64(),
            "visited_nodes": reader.u64(),
        })
    approximate = reader.boolean()
    reader.zeroes(7)
    counts = [reader.u64() for _ in range(5)]
    return {
        "snapshot": snapshot,
        "hits": hits,
        "facets": facets,
        "aggregations": aggregations,
        "vector_branches": branches,
        "approximate": approximate,
        "total_documents": counts[0],
        "eligible_documents": counts[1],
        "lexical_candidates": counts[2],
        "retrieval_candidates": counts[3],
        "matched_candidates": counts[4],
    }


def _encode_search_filter(value: Any, depth: int = 0) -> bytes:
    if depth > 32 or not isinstance(value, dict):
        raise ClientError("integrated filter is invalid")
    kind = value.get("kind")
    if kind == "match_all":
        return b"\0"
    if kind == "exists":
        return b"\x01" + _text(value["field"])
    if kind == "compare":
        operators = ("equal", "not_equal", "less", "less_or_equal", "greater", "greater_or_equal")
        try:
            operator = operators.index(value["operator"])
        except ValueError as error:
            raise ClientError("integrated comparison operator is invalid") from error
        return b"\x02" + _text(value["field"]) + bytes((operator,)) + _encode_doc_value(value["value"])
    if kind in {"all", "any"}:
        children = value.get("filters", [])
        return bytes((3 if kind == "all" else 4,)) + struct.pack("<I", len(children)) + b"".join(
            _encode_search_filter(child, depth + 1) for child in children
        )
    if kind == "not":
        return b"\x05" + _encode_search_filter(value["filter"], depth + 1)
    raise ClientError("integrated filter kind is invalid")


def _encode_doc_value(value: Any) -> bytes:
    if isinstance(value, bool):
        return bytes((0, int(value)))
    if isinstance(value, int) and not isinstance(value, bool) and -(1 << 63) <= value < 1 << 63:
        return b"\x01" + struct.pack("<q", value)
    if isinstance(value, str):
        return b"\x02" + _text(value)
    if isinstance(value, bytes):
        return b"\x03" + _bytes(value)
    raise ClientError("integrated doc value is invalid")


def _decode_doc_value(reader: _Reader) -> Any:
    tag = reader.u8()
    if tag == 0:
        return reader.boolean()
    if tag == 1:
        return reader.i64()
    if tag == 2:
        return reader.text()
    if tag == 3:
        return reader.bytes()
    raise ClientError("integrated doc value is invalid")


def _decode_aggregation_value(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"kind": "count", "value": reader.u64()}
    if tag == 1:
        return {"kind": "integer", "value": reader.i128() if reader.boolean() else None}
    if tag == 2:
        return {"kind": "value", "value": _decode_doc_value(reader) if reader.boolean() else None}
    raise ClientError("integrated aggregation value is invalid")


def _encode_structure_mutation(value: Any) -> bytes:
    if not isinstance(value, dict):
        raise ClientError("structure mutation is invalid")
    aliases = {
        "create_hash": ("create", "hash"),
        "create_set": ("create", "set"),
        "create_list": ("create", "list"),
        "create_sorted_set": ("create", "sorted_set"),
        "create_stream": ("create", "stream"),
        "list_push_tail": ("list_push", "right"),
        "stream_append": ("stream_add", None),
    }
    kind = value.get("kind")
    alias = aliases.get(kind)
    if alias is not None:
        kind, implied = alias
    else:
        implied = None
    tags = {
        "string_set": 0,
        "string_delete": 1,
        "counter_add": 2,
        "create": 3,
        "delete": 4,
        "expire": 5,
        "hash_set": 6,
        "hash_delete": 7,
        "hash_counter_add": 8,
        "hash_expire_field": 9,
        "list_push": 10,
        "list_pop": 11,
        "set_add": 12,
        "set_remove": 13,
        "sorted_set_add": 14,
        "sorted_set_remove": 15,
        "stream_add": 16,
    }
    if kind not in tags:
        raise ClientError("structure mutation kind is invalid")
    output = bytearray((tags[kind],))
    output.extend(_encode_structure_key(value["key"]))
    if kind == "string_set":
        output.extend(_bytes(value["value"]))
        expiry = value.get("expires_at_micros")
        output.append(expiry is not None)
        if expiry is not None:
            output.extend(struct.pack("<q", expiry))
    elif kind == "counter_add":
        output.extend(struct.pack("<q", value["delta"]))
    elif kind in {"create", "delete"}:
        output.append(_structure_family_tag(implied or value["family"]))
    elif kind == "expire":
        output.append(_structure_family_tag(value["family"]))
        output.extend(struct.pack("<q", value["expires_at_micros"]))
    elif kind == "hash_set":
        output.extend(_bytes(value["field"]))
        output.extend(_bytes(value["value"]))
    elif kind == "hash_delete":
        output.extend(_bytes(value["field"]))
    elif kind == "hash_counter_add":
        output.extend(_bytes(value["field"]))
        output.extend(struct.pack("<q", value["delta"]))
    elif kind == "hash_expire_field":
        output.extend(_bytes(value["field"]))
        output.extend(struct.pack("<q", value["expires_at_micros"]))
    elif kind == "list_push":
        output.append(_list_side_tag(implied or value["side"]))
        output.extend(_bytes(value["value"]))
    elif kind == "list_pop":
        output.append(_list_side_tag(value["side"]))
    elif kind in {"set_add", "set_remove", "sorted_set_remove"}:
        output.extend(_bytes(value["member"]))
    elif kind == "sorted_set_add":
        output.extend(struct.pack("<d", value["score"]))
        output.extend(_bytes(value["member"]))
    elif kind == "stream_add":
        fields = value["fields"]
        if not isinstance(fields, list) or not 0 < len(fields) <= 4096:
            raise ClientError("stream fields must be a nonempty bounded list")
        output.extend(struct.pack("<I", len(fields)))
        for field, field_value in fields:
            output.extend(_bytes(field))
            output.extend(_bytes(field_value))
    return bytes(output)


def _structure_family_tag(value: Any) -> int:
    families = {
        "string": 1,
        "counter": 2,
        "hash": 3,
        "list": 4,
        "set": 5,
        "sorted_set": 6,
        "stream": 7,
    }
    try:
        return families[value]
    except (KeyError, TypeError) as error:
        raise ClientError("structure family is invalid") from error


def _list_side_tag(value: Any) -> int:
    if value == "left":
        return 0
    if value == "right":
        return 1
    raise ClientError("list side is invalid")


def _encode_transaction_search_mutation(value: Any) -> bytes:
    if not isinstance(value, dict):
        raise ClientError("transaction search mutation is invalid")
    kind = value.get("kind")
    tags = {"index": 0, "replace": 1, "delete": 2}
    if kind not in tags:
        raise ClientError("transaction search mutation kind is invalid")
    output = bytearray((tags[kind],))
    output.extend(int(value["index"]).to_bytes(16, "little"))
    output.extend(_bytes(value["document_id"]))
    if kind != "delete":
        output.extend(_text(value["text"]))
    return bytes(output)


def _encode_transaction_vector_mutation(value: Any) -> bytes:
    if not isinstance(value, dict):
        raise ClientError("transaction vector mutation is invalid")
    kind = value.get("kind")
    if kind not in {"upsert", "delete"}:
        raise ClientError("transaction vector mutation kind is invalid")
    output = bytearray((0 if kind == "upsert" else 1,))
    output.extend(int(value["index"]).to_bytes(16, "little"))
    output.extend(int(value["object_id"]).to_bytes(16, "little"))
    if kind == "upsert":
        vector = value.get("vector")
        if not isinstance(vector, list) or not vector:
            raise ClientError("transaction vector must be a nonempty list")
        output.extend(struct.pack("<I", len(vector)))
        output.extend(struct.pack("<" + "f" * len(vector), *vector))
    return bytes(output)


def _encode_structure_read(value: dict[str, Any]) -> bytes:
    kind = value.get("kind")
    tags = {
        "string_get": 0, "counter_get": 1, "ttl": 2, "hash_get": 3,
        "hash_field_ttl": 4, "hash_scan": 5, "hash_length": 6, "list_range": 7,
        "list_length": 8, "set_contains": 9, "set_members": 10, "set_cardinality": 11,
        "set_algebra": 12, "sorted_set_score": 13, "sorted_set_rank": 14,
        "sorted_set_range": 15, "sorted_set_cardinality": 16, "stream_range": 17,
    }
    if kind not in tags:
        raise ClientError("structure read kind is invalid")
    tag = tags[kind]
    output = bytearray((tag,))
    if kind == "set_algebra":
        operations = {"union": 0, "intersection": 1, "difference": 2}
        if value.get("operation") not in operations:
            raise ClientError("set algebra operation is invalid")
        keys = value.get("keys")
        if not isinstance(keys, list) or not keys:
            raise ClientError("set algebra keys must be a nonempty list")
        output.extend(int(value["keyspace"]).to_bytes(16, "little"))
        output.append(operations[value["operation"]])
        output.extend(struct.pack("<I", len(keys)))
        output.extend(b"".join(_bytes(key) for key in keys))
        output.extend(struct.pack("<QQ", value["output_member_limit"], value["visit_limit"]))
        return bytes(output)
    output.extend(_encode_structure_key(value["key"]))
    if kind == "ttl":
        output.append(_structure_family_tag(value["family"]))
    elif kind in {"hash_get", "hash_field_ttl"}:
        output.extend(_bytes(value["field"]))
    elif kind in {"hash_scan", "set_members"}:
        cursor = value.get("start_after")
        output.append(cursor is not None)
        if cursor is not None:
            output.extend(_bytes(cursor))
        output.extend(struct.pack("<Q", value["limit"]))
    elif kind in {"set_contains", "sorted_set_score"}:
        output.extend(_bytes(value["member"]))
    elif kind == "sorted_set_rank":
        output.extend(_bytes(value["member"]))
        output.append(_sorted_order_tag(value.get("order", "ascending")))
    elif kind in {"list_range", "sorted_set_range"}:
        output.extend(struct.pack("<qq", value["start"], value["stop"]))
        if kind == "sorted_set_range":
            output.append(_sorted_order_tag(value.get("order", "ascending")))
    elif kind == "stream_range":
        output.extend(struct.pack("<QQQ", value["start"], value["end"], value["limit"]))
    return bytes(output)


def _sorted_order_tag(value: Any) -> int:
    if value == "ascending":
        return 0
    if value == "descending":
        return 1
    raise ClientError("sorted-set order is invalid")


def _encode_structure_key(value: Any) -> bytes:
    if not isinstance(value, dict):
        raise ClientError("structure key is invalid")
    return int(value["keyspace"]).to_bytes(16, "little") + _bytes(value["key"])


def _decode_structure_read(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"kind": "value", "value": reader.bytes() if reader.boolean() else None}
    if tag == 1:
        values = [reader.bytes() for _ in range(reader.u32())]
        return {"kind": "values", "values": values}
    if tag == 2:
        return {"kind": "counter", "value": reader.i64() if reader.boolean() else None}
    if tag == 3:
        state = reader.u8()
        if state > 2:
            raise ClientError("structure TTL response is invalid")
        return {"kind": "ttl", "value": {"state": ("missing", "persistent", "remaining")[state], **({"remaining_micros": reader.i64()} if state == 2 else {})}}
    if tag == 4:
        return {"kind": "hash_entries", "entries": [
            {"field": reader.bytes(), "value": reader.bytes()} for _ in range(reader.u32())
        ]}
    if tag == 5:
        return {"kind": "count", "value": reader.u64()}
    if tag == 6:
        return {"kind": "boolean", "value": reader.boolean()}
    if tag == 7:
        return {"kind": "set_algebra", "members": [reader.bytes() for _ in range(reader.u32())], "visited": reader.u64()}
    if tag == 8:
        return {"kind": "sorted_set_score", "value": reader.f64() if reader.boolean() else None}
    if tag == 9:
        return {"kind": "sorted_set_rank", "value": reader.u64() if reader.boolean() else None}
    if tag == 10:
        return {"kind": "sorted_set_entries", "entries": [
            {"member": reader.bytes(), "score": reader.f64()} for _ in range(reader.u32())
        ]}
    if tag == 11:
        entries = []
        for _ in range(reader.u32()):
            entry_id = reader.u64()
            fields = [(reader.bytes(), reader.bytes()) for _ in range(reader.u32())]
            entries.append({"id": entry_id, "fields": fields})
        return {"kind": "stream_entries", "entries": entries}
    raise ClientError("structure read response is invalid")


def _decode_structure_read_request(reader: _Reader) -> dict[str, Any]:
    kinds = (
        "string_get", "counter_get", "ttl", "hash_get", "hash_field_ttl", "hash_scan",
        "hash_length", "list_range", "list_length", "set_contains", "set_members",
        "set_cardinality", "set_algebra", "sorted_set_score", "sorted_set_rank",
        "sorted_set_range", "sorted_set_cardinality", "stream_range",
    )
    tag = reader.u8()
    if tag >= len(kinds):
        raise ClientError("structure read kind is invalid")
    kind = kinds[tag]
    if kind == "set_algebra":
        operation_tag = reader.u8() if False else None
        keyspace = reader.u128()
        operation_tag = reader.u8()
        if operation_tag > 2:
            raise ClientError("set algebra operation is invalid")
        return {
            "kind": kind, "keyspace": keyspace,
            "operation": ("union", "intersection", "difference")[operation_tag],
            "keys": [reader.bytes() for _ in range(reader.u32())],
            "output_member_limit": reader.u64(), "visit_limit": reader.u64(),
        }
    result: dict[str, Any] = {"kind": kind, "key": _decode_structure_key(reader)}
    if kind == "ttl":
        families = (None, "string", "counter", "hash", "list", "set", "sorted_set", "stream")
        family = reader.u8()
        if family == 0 or family >= len(families):
            raise ClientError("structure family is invalid")
        result["family"] = families[family]
    elif kind in {"hash_get", "hash_field_ttl"}:
        result["field"] = reader.bytes()
    elif kind in {"hash_scan", "set_members"}:
        result["start_after"] = reader.bytes() if reader.boolean() else None
        result["limit"] = reader.u64()
    elif kind in {"set_contains", "sorted_set_score"}:
        result["member"] = reader.bytes()
    elif kind == "sorted_set_rank":
        result["member"] = reader.bytes()
        order = reader.u8()
        if order > 1:
            raise ClientError("sorted-set order is invalid")
        result["order"] = ("ascending", "descending")[order]
    elif kind in {"list_range", "sorted_set_range"}:
        result.update(start=reader.i64(), stop=reader.i64())
        if kind == "sorted_set_range":
            order = reader.u8()
            if order > 1:
                raise ClientError("sorted-set order is invalid")
            result["order"] = ("ascending", "descending")[order]
    elif kind == "stream_range":
        result.update(start=reader.u64(), end=reader.u64(), limit=reader.u64())
    return result


def _decode_commit_receipt(reader: _Reader) -> dict[str, Any]:
    value = {
        "transaction_id": reader.u128(),
        "commit_csn": reader.u64(),
        "catalog_version": reader.u64(),
        "commit_lsn": reader.u64(),
        "wal_block_digest": reader.take(32),
    }
    tag = reader.u8()
    reader.zeroes(7)
    if tag not in (0, 1, 2):
        raise ClientError("commit durability is invalid")
    value["durability"] = ("strict", "group", "memory")[tag]
    value["durability_cohort_size"] = reader.u64()
    value["durability_cohort_position"] = reader.u64()
    return value


def _decode_commit_outcome(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    reader.zeroes(7)
    if tag == 0:
        return {"state": "committed", "receipt": _decode_commit_receipt(reader)}
    if tag == 1:
        return {"state": "outcome_unknown", "transaction_id": reader.u128()}
    raise ClientError("commit outcome is malformed")


def _decode_transaction_status(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"state": "unknown"}
    if tag == 1:
        return {"state": "committed", "receipt": _decode_commit_receipt(reader)}
    if tag in (2, 3):
        return {
            "state": "rolled_back" if tag == 2 else "outcome_unknown",
            "transaction_id": reader.u128(),
        }
    raise ClientError("transaction status is malformed")


def _decode_explicit_transaction_status(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"state": "unknown"}
    if tag == 1:
        handle = reader.u64()
        read_csn = reader.u64() or None
        staged_operations = reader.u64()
        durability_tag = reader.u8()
        if durability_tag > 2:
            raise ClientError("explicit transaction durability is invalid")
        return {
            "state": "active",
            "handle": handle,
            "read_csn": read_csn,
            "staged_operations": staged_operations,
            "durability": ("strict", "group", "memory")[durability_tag],
        }
    if tag == 2:
        return {"state": "committed", "handle": reader.u64(), "staged_operations": reader.u64(), "receipt": _decode_commit_receipt(reader)}
    if tag == 3:
        return {
            "state": "rolled_back",
            "handle": reader.u64(),
            "discarded_operations": reader.u64(),
        }
    if tag == 4:
        return {
            "state": "outcome_unknown",
            "handle": reader.u64(),
            "transaction_id": reader.u128(),
            "staged_operations": reader.u64(),
        }
    raise ClientError("explicit transaction status is malformed")


def _decode_transaction_stage_result(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"kind": "sql", "result": _decode_sql_result(reader)}
    if tag == 1:
        return {"kind": "structure", "result": _decode_structure_mutation_result(reader)}
    if tag == 2:
        return {"kind": "search"}
    if tag == 3:
        return {"kind": "vector", "changed": reader.boolean()}
    raise ClientError("transaction stage result is malformed")


def _decode_structure_mutation_result(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        return {"kind": "unit"}
    if tag == 1:
        return {"kind": "integer", "value": reader.i64()}
    if tag == 2:
        return {"kind": "boolean", "value": reader.boolean()}
    if tag == 3:
        return {"kind": "count", "value": reader.u64()}
    if tag == 4:
        return {"kind": "value", "value": reader.bytes() if reader.boolean() else None}
    if tag == 5:
        return {"kind": "stream_id", "value": reader.u64()}
    raise ClientError("structure mutation result is malformed")


def _decode_sql_result(reader: _Reader) -> dict[str, Any]:
    tag = reader.u8()
    if tag == 0:
        has_object = reader.boolean()
        reader.zeroes(6)
        return {
            "kind": "command",
            "rows_affected": reader.u64(),
            "object_id": reader.u128() if has_object else None,
        }
    if tag == 1:
        reader.zeroes(7)
        column_count, row_count = reader.u32(), reader.u32()
        columns = [reader.text() for _ in range(column_count)]
        rows = [[_decode_value(reader, 0) for _ in columns] for _ in range(row_count)]
        return {"kind": "rows", "columns": columns, "rows": rows}
    raise ClientError("SQL result is malformed")


def _decode_value(reader: _Reader, depth: int) -> Any:
    if depth > 8:
        raise ClientError("SQL value nesting is too deep")
    tag = reader.u8()
    if tag == 0:
        return None
    if tag == 1:
        return reader.boolean()
    if tag == 2:
        return reader.i64()
    if tag == 3:
        return reader.u64()
    if tag == 4:
        return reader.i128()
    if tag == 5:
        return reader.f32()
    if tag == 6:
        return reader.f64()
    if tag == 7:
        return reader.text()
    if tag == 8:
        return reader.bytes()
    if tag == 9:
        return {"date_days": reader.i32()}
    if tag == 10:
        return {"time_nanos": reader.u64()}
    if tag == 11:
        return {"timestamp_micros": reader.i64()}
    if tag == 12:
        return {"interval": {"months": reader.i32(), "days": reader.i32(), "nanoseconds": reader.i64()}}
    if tag == 13:
        return {"uuid": reader.take(16)}
    if tag == 14:
        return [_decode_value(reader, depth + 1) for _ in range(reader.u32())]
    if tag == 15:
        return {"map": [[_decode_value(reader, depth + 1), _decode_value(reader, depth + 1)] for _ in range(reader.u32())]}
    if tag == 16:
        return {"vector": [reader.f32() for _ in range(reader.u32())]}
    if tag == 17:
        return {"json": reader.text()}
    raise ClientError("SQL value kind is unsupported")


def _decode_catalog_page(reader: _Reader, *, dependencies: bool) -> dict[str, Any]:
    snapshot = _decode_snapshot(reader)
    cursor = _decode_cursor(reader)
    stop_tag = reader.u8()
    reader.zeroes(7)
    stops = ("exhausted", "item_limit", "visit_limit", "byte_limit")
    if stop_tag >= len(stops):
        raise ClientError("catalog page stop is invalid")
    page = {
        "snapshot": snapshot,
        "cursor": cursor,
        "stop": stops[stop_tag],
        "visited": reader.u64(),
        "returned_bytes": reader.u64(),
        "items": [],
    }
    count = reader.u32()
    if dependencies:
        kinds = ("parent", "secondary_index_relation", "foreign_key", "analyzer", "link_endpoint", "relation_schema")
        for _ in range(count):
            dependent, prerequisite, tag = reader.u128(), reader.u128(), reader.u8()
            if not 1 <= tag <= len(kinds):
                raise ClientError("catalog dependency kind is invalid")
            page["items"].append({"dependent": dependent, "prerequisite": prerequisite, "kind": kinds[tag - 1]})
    else:
        kinds = ("database", "schema", "relation", "secondary_index", "keyspace", "structure", "search_collection", "analyzer", "cross_engine_link")
        for _ in range(count):
            object_id, tag, has_parent = reader.u128(), reader.u8(), reader.boolean()
            reader.zeroes(6)
            if not 1 <= tag <= len(kinds):
                raise ClientError("catalog object kind is invalid")
            page["items"].append({
                "id": object_id,
                "kind": kinds[tag - 1],
                "parent": reader.u128() if has_parent else None,
                "name": _decode_qualified_name(reader),
            })
    return page


def _decode_cursor(reader: _Reader) -> dict[str, Any] | None:
    present = reader.boolean()
    reader.zeroes(7)
    return {"snapshot": _decode_snapshot(reader), "after": reader.u128()} if present else None


def _decode_qualified_name(reader: _Reader) -> dict[str, dict[str, str]]:
    return {
        component: {"display": reader.text(), "lookup": reader.text()}
        for component in ("database", "schema", "object")
    }


def _decode_doctor(reader: _Reader) -> dict[str, Any]:
    status = reader.u8()
    statuses = ("healthy", "busy", "corrupt", "io")
    if status >= len(statuses):
        raise ClientError("doctor status is invalid")
    verified_open, snapshot_verified = reader.boolean(), reader.boolean()
    has_lineage, has_recovery = reader.boolean(), reader.boolean()
    reader.zeroes(3)
    telemetry_registry_version = reader.u16()
    reader.zeroes(2)
    process_start_identity = reader.u128()
    session_start_identity = reader.u128()
    value = {
        "status": statuses[status],
        "verified_open": verified_open,
        "snapshot_verified": snapshot_verified,
        "telemetry_registry_version": telemetry_registry_version,
        "process_start_identity": process_start_identity,
        "session_start_identity": session_start_identity,
        "directory_lineage": reader.take(24) if has_lineage else None,
        "recovery": None,
    }
    if has_recovery:
        value["recovery"] = {
            "visible_csn": reader.u64() or None,
            "replayed_transactions": reader.u64(),
            "page_tail_bytes_removed": reader.u64(),
            "wal_tail_bytes_removed": reader.u64(),
            "retained_wal_bytes": reader.u64(),
            "manifest_count": reader.u64(),
            "blob_count": reader.u64(),
            "open_time_micros": reader.u64(),
        }
    return value


def _decode_telemetry(reader: _Reader) -> dict[str, Any]:
    registry_version = reader.u16()
    if reader.u16() != 0:
        raise ClientError("telemetry response is malformed")
    value = {
        "registry_version": registry_version,
        "process_start_identity": reader.u128(),
        "session_start_identity": reader.u128(),
        "captured_at_micros": reader.i64(),
        "catalog_version": reader.u64() or None,
        "dropped_events": reader.u64(),
        "metrics": [],
        "events": [],
    }
    metric_count, event_count = reader.u32(), reader.u32()
    for _ in range(metric_count):
        name, kind = reader.text(), reader.u8()
        if kind in (0, 1):
            metric = {"name": name, "kind": "counter" if kind == 0 else "gauge", "value": reader.u64()}
        elif kind == 2:
            metric = {"name": name, "kind": "histogram", "count": reader.u64(), "sum_micros": reader.u64(), "buckets": [reader.u64() for _ in range(11)]}
        else:
            raise ClientError("telemetry metric kind is invalid")
        value["metrics"].append(metric)
    event_names = ("backup", "restore", "doctor", "cancelled", "deadline", "error")
    categories = ("invalid-request", "not-found", "conflict", "limit", "deadline", "cancelled", "authorization", "corruption", "unavailable", "io", "internal")
    for _ in range(event_count):
        captured, kind, category = reader.i64(), reader.u8(), reader.u8()
        reader.zeroes(6)
        if kind >= len(event_names) or (kind == 5 and category >= len(categories)):
            raise ClientError("telemetry event is invalid")
        value["events"].append({"captured_at_micros": captured, "kind": event_names[kind], "category": categories[category] if kind == 5 else None})
    return value


class _Reader:
    def __init__(self, encoded: bytes) -> None:
        self.encoded = encoded
        self.offset = 0

    def take(self, length: int) -> bytes:
        if length < 0 or self.offset + length > len(self.encoded):
            raise ClientError("product response is truncated")
        value = self.encoded[self.offset:self.offset + length]
        self.offset += length
        return value

    def zeroes(self, length: int) -> None:
        if self.take(length) != b"\0" * length:
            raise ClientError("product response reserved bytes are nonzero")

    def boolean(self) -> bool:
        value = self.u8()
        if value not in (0, 1):
            raise ClientError("product response boolean is invalid")
        return bool(value)

    def bytes(self) -> bytes:
        length = self.u32()
        if length > MAX_PAYLOAD:
            raise ClientError("product response bytes exceed the protocol maximum")
        return self.take(length)

    def text(self) -> str:
        try:
            return self.bytes().decode("utf-8")
        except UnicodeDecodeError as error:
            raise ClientError("product response text is not valid UTF-8") from error

    def u8(self) -> int:
        return self.take(1)[0]

    def u16(self) -> int:
        return struct.unpack("<H", self.take(2))[0]

    def u32(self) -> int:
        return struct.unpack("<I", self.take(4))[0]

    def i32(self) -> int:
        return struct.unpack("<i", self.take(4))[0]

    def u64(self) -> int:
        return struct.unpack("<Q", self.take(8))[0]

    def i64(self) -> int:
        return struct.unpack("<q", self.take(8))[0]

    def u128(self) -> int:
        return int.from_bytes(self.take(16), "little")

    def i128(self) -> int:
        return int.from_bytes(self.take(16), "little", signed=True)

    def f32(self) -> float:
        return struct.unpack("<f", self.take(4))[0]

    def f64(self) -> float:
        return struct.unpack("<d", self.take(8))[0]

    def finish(self) -> None:
        if self.offset != len(self.encoded):
            raise ClientError("product response has trailing bytes")


def decode_product_error(encoded: bytes) -> ProductErrorFields:
    if len(encoded) < 20 or encoded[:8] != b"HYPERR01" or struct.unpack_from("<I", encoded, 8)[0] != len(encoded):
        raise ClientError("HYPERR01 envelope is malformed")
    category, retry, transaction_state, flags, code_length, message_length, detail_count = struct.unpack_from("<BBBBBHB", encoded, 12)
    offset = 20
    code, offset = _take_text(encoded, offset, code_length)
    message, offset = _take_text(encoded, offset, message_length)
    identities: list[int | None] = []
    for bit in range(3):
        identities.append(int.from_bytes(encoded[offset:offset + 16], "little") if flags & 1 << bit else None)
        if flags & 1 << bit:
            offset += 16
    limit = None
    if flags & 8:
        size = encoded[offset]
        offset += 1
        name, offset = _take_text(encoded, offset, size)
        configured, observed = struct.unpack_from("<QQ", encoded, offset)
        offset += 16
        limit = {"kind": name, "configured": configured, "observed": observed}
    source_span = None
    if flags & 16:
        start, end = struct.unpack_from("<II", encoded, offset)
        offset += 8
        source_span = {"start": start, "end": end}
    details: dict[str, Any] = {}
    transaction_id = None
    previous = 0
    for _ in range(detail_count):
        tag, length = struct.unpack_from("<HH", encoded, offset)
        offset += 4
        if tag <= previous or offset + length > len(encoded):
            raise ClientError("HYPERR01 details are noncanonical")
        value = encoded[offset:offset + length]
        offset += length
        previous = tag
        if tag == 1:
            details["sql_subcode"] = value.decode("ascii")
        elif tag == 2:
            transaction_id = int.from_bytes(value, "little")
        else:
            details[f"unknown_{tag}"] = value.hex()
    if offset != len(encoded):
        raise ClientError("HYPERR01 envelope has trailing bytes")
    return ProductErrorFields(
        code=code,
        category=("invalid-request", "not-found", "conflict", "limit", "deadline", "cancelled", "authorization", "corruption", "unavailable", "io", "internal")[category],
        retry=("never", "same-request", "new-snapshot", "after-backoff", "after-recovery", "unknown-commit")[retry],
        message=message,
        request_id=identities[0],
        trace_id=identities[1],
        object_id=identities[2],
        transaction_state=("none", "active", "rolled-back", "committed", "outcome-unknown")[transaction_state],
        transaction_id=transaction_id,
        limit=limit,
        source_span=source_span,
        details=details,
    )


def decode_end(encoded: bytes) -> tuple[int, bytes]:
    if len(encoded) != 56 or encoded[:8] != b"HYPEND01" or encoded[12] != 1 or encoded[13:16] != b"\0" * 3:
        raise ClientError("native completion frame is malformed")
    return struct.unpack_from("<Q", encoded, 16)[0], encoded[24:56]


def blake3(data: bytes) -> bytes:
    """Dependency-free one-shot BLAKE3 needed by mandatory END validation."""

    iv = (0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19)
    permutation = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)

    def rotate(value: int, count: int) -> int:
        return ((value >> count) | (value << (32 - count))) & 0xFFFFFFFF

    def compress(cv: tuple[int, ...], words: tuple[int, ...], counter: int, length: int, flags: int) -> tuple[int, ...]:
        state = list(cv + iv[:4] + (counter & 0xFFFFFFFF, counter >> 32, length, flags))
        message = list(words)

        def mix(a: int, b: int, c: int, d: int, x: int, y: int) -> None:
            state[a] = (state[a] + state[b] + x) & 0xFFFFFFFF
            state[d] = rotate(state[d] ^ state[a], 16)
            state[c] = (state[c] + state[d]) & 0xFFFFFFFF
            state[b] = rotate(state[b] ^ state[c], 12)
            state[a] = (state[a] + state[b] + y) & 0xFFFFFFFF
            state[d] = rotate(state[d] ^ state[a], 8)
            state[c] = (state[c] + state[d]) & 0xFFFFFFFF
            state[b] = rotate(state[b] ^ state[c], 7)

        for round_index in range(7):
            mix(0, 4, 8, 12, message[0], message[1])
            mix(1, 5, 9, 13, message[2], message[3])
            mix(2, 6, 10, 14, message[4], message[5])
            mix(3, 7, 11, 15, message[6], message[7])
            mix(0, 5, 10, 15, message[8], message[9])
            mix(1, 6, 11, 12, message[10], message[11])
            mix(2, 7, 8, 13, message[12], message[13])
            mix(3, 4, 9, 14, message[14], message[15])
            if round_index != 6:
                message = [message[index] for index in permutation]
        return tuple((state[index] ^ state[index + 8]) & 0xFFFFFFFF for index in range(8)) + tuple(
            (state[index + 8] ^ cv[index]) & 0xFFFFFFFF for index in range(8)
        )

    @dataclass(frozen=True)
    class Output:
        cv: tuple[int, ...]
        words: tuple[int, ...]
        counter: int
        length: int
        flags: int

        def chaining_value(self) -> tuple[int, ...]:
            return compress(self.cv, self.words, self.counter, self.length, self.flags)[:8]

        def root(self) -> bytes:
            words = compress(self.cv, self.words, 0, self.length, self.flags | 8)
            return b"".join(word.to_bytes(4, "little") for word in words)[:32]

    def block_words(block: bytes) -> tuple[int, ...]:
        return struct.unpack("<16I", block.ljust(64, b"\0"))

    def chunk_output(chunk: bytes, counter: int) -> Output:
        cv = iv
        blocks = [chunk[offset:offset + 64] for offset in range(0, len(chunk), 64)] or [b""]
        for index, block in enumerate(blocks[:-1]):
            flags = 1 if index == 0 else 0
            cv = compress(cv, block_words(block), counter, len(block), flags)[:8]
        final = blocks[-1]
        flags = 2 | (1 if len(blocks) == 1 else 0)
        return Output(cv, block_words(final), counter, len(final), flags)

    def parent_output(left: tuple[int, ...], right: tuple[int, ...]) -> Output:
        return Output(iv, left + right, 0, 64, 4)

    chunks = [data[offset:offset + 1024] for offset in range(0, len(data), 1024)] or [b""]
    stack: list[tuple[int, ...]] = []
    for index, chunk in enumerate(chunks[:-1]):
        value = chunk_output(chunk, index).chaining_value()
        total = index + 1
        while total & 1 == 0:
            value = parent_output(stack.pop(), value).chaining_value()
            total >>= 1
        stack.append(value)
    output = chunk_output(chunks[-1], len(chunks) - 1)
    while stack:
        output = parent_output(stack.pop(), output.chaining_value())
    return output.root()


def encode_cancel(reason: int = 1) -> bytes:
    return struct.pack("<8sI4x", b"HYPCAN01", reason)


def encode_window_update(increment: int) -> bytes:
    if increment <= 0:
        raise ClientError("window update must be positive")
    return struct.pack("<8sQ", b"HYPWIN01", increment)


def _envelope(encoded: bytes, magic: bytes) -> tuple[int, bytes]:
    if len(encoded) < 16:
        raise ClientError("product envelope is truncated")
    found_magic, length, kind, reserved = struct.unpack_from("<8sIHH", encoded)
    if found_magic != magic or length != len(encoded) or reserved != 0 or len(encoded) > MAX_PAYLOAD:
        raise ClientError("product envelope is malformed")
    return kind, encoded[16:]


def _bytes(value: bytes) -> bytes:
    if not isinstance(value, bytes) or len(value) > MAX_PAYLOAD:
        raise ClientError("binary value is invalid or too large")
    return struct.pack("<I", len(value)) + value


def _text(value: str) -> bytes:
    return _bytes(value.encode("utf-8"))


def _take_bytes(encoded: bytes, offset: int) -> tuple[bytes, int]:
    if offset + 4 > len(encoded):
        raise ClientError("length-prefixed bytes are truncated")
    length = struct.unpack_from("<I", encoded, offset)[0]
    offset += 4
    if length > MAX_PAYLOAD or offset + length > len(encoded):
        raise ClientError("length-prefixed bytes are invalid")
    return encoded[offset:offset + length], offset + length


def _take_text(encoded: bytes, offset: int, length: int) -> tuple[str, int]:
    if offset + length > len(encoded):
        raise ClientError("text is truncated")
    try:
        return encoded[offset:offset + length].decode("utf-8"), offset + length
    except UnicodeDecodeError as error:
        raise ClientError("text is not valid UTF-8") from error


__all__ = [
    "API_KEY_AUTH_CAPABILITY",
    "FRAME_HEADER_SIZE",
    "FRAME_KINDS",
    "Frame",
    "G6_CAPABILITIES",
    "MAX_PAYLOAD",
    "PROTOCOL_MAJOR",
    "PROTOCOL_MINOR",
    "crc32c",
    "blake3",
    "decode_end",
    "decode_frame",
    "decode_product_error",
    "decode_product_response",
    "decode_welcome",
    "encode_cancel",
    "encode_authenticated_hello",
    "encode_frame",
    "encode_hello",
    "encode_product_request",
    "encode_window_update",
    "operation_required_minor",
    "response_required_minor",
]
