#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the installed Python SDK against one real managed Native daemon."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any, Callable

from hyphae_sdk.v2 import (
    HttpTransport,
    HyphaeClient,
    LocalTransport,
    ProductError,
    RequestOptions,
    Response,
)


READ_OPERATIONS = [
    "security_assignment_list",
    "security_audit_read",
    "security_key_list",
    "security_principal_list",
    "security_role_list",
    "security_status",
]
WRITE_OPERATIONS = [
    "security_assignment_revoke",
    "security_built_in_assignment_create",
    "security_custom_assignment_create",
    "security_custom_role_create",
    "security_principal_create",
    "security_principal_set_enabled",
]
LIFECYCLE_OPERATIONS = [
    "security_api_key_issue_abort",
    "security_api_key_issue_activate",
    "security_api_key_issue_start",
    "security_api_key_revoke",
    "security_legacy_bearer_revoke",
]
FORBIDDEN_FIELDS = {
    "api_key",
    "secret",
    "serialized",
    "verifier",
    "verifier_digest",
}


def _credential(path: Path) -> str:
    value = path.read_text(encoding="ascii")
    if not value or "\r" in value or "\n" in value:
        raise RuntimeError("managed credential file is malformed")
    return value


def _same_response(local: Response, http: Response, operation: str) -> None:
    if local.kind != http.kind or local.value != http.value:
        raise AssertionError(f"{operation} differs across local and HTTP transports")
    _reject_credential_fields(local.value)
    _reject_credential_fields(http.value)


def _reject_credential_fields(value: Any) -> None:
    if isinstance(value, dict):
        for key, item in value.items():
            if str(key).casefold() in FORBIDDEN_FIELDS:
                raise AssertionError("security response contains credential-bearing metadata")
            _reject_credential_fields(item)
    elif isinstance(value, list):
        for item in value:
            _reject_credential_fields(item)
    elif isinstance(value, str) and value.startswith("hyp1_"):
        raise AssertionError("security response contains credential material")


def _fingerprint(value: object) -> str:
    def encode_bytes(candidate: object) -> dict[str, str]:
        if isinstance(candidate, bytes):
            return {"$bytes": candidate.hex()}
        raise TypeError(f"unsupported fingerprint value: {type(candidate).__name__}")

    return json.dumps(
        value,
        default=encode_bytes,
        sort_keys=True,
        separators=(",", ":"),
    )


def _page_sequence(
    local: HyphaeClient,
    http: HyphaeClient,
    operation: str,
    invoke: Callable[[HyphaeClient, object | None, RequestOptions], Response],
    request_id: int,
) -> object:
    cursor: object | None = None
    first_cursor: object | None = None
    seen_cursors: set[str] = set()
    seen_items: set[str] = set()
    for page_index in range(64):
        options = RequestOptions(request_id=request_id + page_index)
        local_page = invoke(local, cursor, options)
        http_page = invoke(http, cursor, options)
        _same_response(local_page, http_page, operation)
        item_field = "events" if operation == "security_audit_read" else "items"
        items = local_page.value.get(item_field)
        if not isinstance(items, list) or not items:
            raise AssertionError(f"{operation} returned an empty live page")
        for item in items:
            fingerprint = _fingerprint(item)
            if fingerprint in seen_items:
                raise AssertionError(f"{operation} repeated an item across pages")
            seen_items.add(fingerprint)
        next_cursor = local_page.value.get("next_cursor")
        if page_index == 0:
            first_cursor = next_cursor
        if next_cursor is None:
            if first_cursor is None:
                raise AssertionError(f"{operation} fixture did not produce a continuation")
            return first_cursor
        cursor_fingerprint = _fingerprint(next_cursor)
        if cursor_fingerprint in seen_cursors:
            raise AssertionError(f"{operation} cursor did not advance")
        seen_cursors.add(cursor_fingerprint)
        cursor = next_cursor
    raise AssertionError(f"{operation} pagination did not terminate")


def assert_security_reads(local: HyphaeClient, http: HyphaeClient) -> object:
    options = RequestOptions(request_id=100)
    local_status = local.security_status(options=options)
    http_status = http.security_status(options=options)
    _same_response(local_status, http_status, "security_status")
    if local_status.value.get("bootstrapped") is not True:
        raise AssertionError("managed fixture is not bootstrapped")

    principal_cursor = _page_sequence(
        local,
        http,
        "security_principal_list",
        lambda client, cursor, request: client.security_principal_list(
            cursor=cursor, limit=1, options=request
        ),
        110,
    )
    _page_sequence(
        local,
        http,
        "security_role_list",
        lambda client, cursor, request: client.security_role_list(
            cursor=cursor, limit=1, options=request
        ),
        120,
    )
    _page_sequence(
        local,
        http,
        "security_assignment_list",
        lambda client, cursor, request: client.security_assignment_list(
            cursor=cursor, limit=1, options=request
        ),
        130,
    )
    _page_sequence(
        local,
        http,
        "security_key_list",
        lambda client, cursor, request: client.security_key_list(
            cursor=cursor, limit=1, options=request
        ),
        140,
    )
    _page_sequence(
        local,
        http,
        "security_audit_read",
        lambda client, cursor, request: client.security_audit_read(
            cursor=cursor if isinstance(cursor, int) else None,
            limit=1,
            options=request,
        ),
        150,
    )
    return principal_cursor


def _mutation_pair(
    operation: str,
    local_call: Callable[[], Response],
    http_call: Callable[[], Response],
) -> Response:
    local = local_call()
    http = http_call()
    _same_response(local, http, operation)
    return local


def _mutation_options(request_id: int, token: int) -> RequestOptions:
    return RequestOptions(request_id=request_id, idempotency_token=token)


def _authorization_denied(call: Callable[[], Response]) -> None:
    try:
        call()
    except ProductError as error:
        if (
            error.code != "authorization_denied"
            or error.category != "authorization"
            or error.retry != "never"
        ):
            raise AssertionError("revocation returned the wrong denial") from error
        return
    raise AssertionError("revoked authority executed another operation")


def _catalog_conflict(call: Callable[[], Response]) -> None:
    try:
        call()
    except ProductError as error:
        if error.code != "catalog_conflict":
            raise AssertionError("stale cursor returned the wrong error") from error
        return
    raise AssertionError("stale cursor was accepted")


def _readback_items(
    local: HyphaeClient,
    http: HyphaeClient,
    operation: str,
    invoke: Callable[[HyphaeClient, RequestOptions], Response],
    request_id: int,
) -> list[dict[str, Any]]:
    options = RequestOptions(request_id=request_id)
    local_page = invoke(local, options)
    http_page = invoke(http, options)
    _same_response(local_page, http_page, operation)
    if local_page.value.get("next_cursor") is not None:
        raise AssertionError(f"{operation} readback unexpectedly exceeded its bound")
    items = local_page.value.get("items")
    if not isinstance(items, list) or not all(isinstance(item, dict) for item in items):
        raise AssertionError(f"{operation} readback is invalid")
    return items


def assert_security_mutations(
    owner_local: HyphaeClient,
    owner_http: HyphaeClient,
    auditor_local: HyphaeClient,
    auditor_http: HyphaeClient,
    auditor_assignment_id: int,
    stale_principal_cursor: object,
) -> int:
    created = _mutation_pair(
        "security_principal_create",
        lambda: owner_local.security_principal_create(
            "Python managed application",
            options=_mutation_options(201, 101),
        ),
        lambda: owner_http.security_principal_create(
            "Python managed application",
            options=_mutation_options(201, 101),
        ),
    )
    try:
        owner_http.security_principal_create(
            "Python managed conflicting application",
            options=_mutation_options(202, 101),
        )
    except ProductError as error:
        if error.code != "idempotency_conflict":
            raise AssertionError("idempotent conflict returned the wrong error") from error
    else:
        raise AssertionError("idempotent conflict was accepted")
    principal_id = int(created.value["principal_id"])
    _catalog_conflict(
        lambda: auditor_local.security_principal_list(
            cursor=stale_principal_cursor,
            limit=1,
            options=RequestOptions(request_id=203),
        )
    )
    _catalog_conflict(
        lambda: auditor_http.security_principal_list(
            cursor=stale_principal_cursor,
            limit=1,
            options=RequestOptions(request_id=203),
        )
    )

    _mutation_pair(
        "security_principal_set_enabled",
        lambda: owner_local.security_principal_set_enabled(
            principal_id,
            True,
            options=_mutation_options(211, 102),
        ),
        lambda: owner_http.security_principal_set_enabled(
            principal_id,
            True,
            options=_mutation_options(211, 102),
        ),
    )
    role = _mutation_pair(
        "security_custom_role_create",
        lambda: owner_local.security_custom_role_create(
            "Python managed scoped reader",
            [{"permission": "data.read", "scope": {"kind": "instance"}}],
            options=_mutation_options(221, 103),
        ),
        lambda: owner_http.security_custom_role_create(
            "Python managed scoped reader",
            [{"permission": "data.read", "scope": {"kind": "instance"}}],
            options=_mutation_options(221, 103),
        ),
    )
    built_in_assignment = _mutation_pair(
        "security_built_in_assignment_create",
        lambda: owner_local.security_built_in_assignment_create(
            principal_id,
            "reader",
            {"kind": "instance"},
            options=_mutation_options(231, 104),
        ),
        lambda: owner_http.security_built_in_assignment_create(
            principal_id,
            "reader",
            {"kind": "instance"},
            options=_mutation_options(231, 104),
        ),
    )
    custom_assignment = _mutation_pair(
        "security_custom_assignment_create",
        lambda: owner_local.security_custom_assignment_create(
            principal_id,
            int(role.value["role_id"]),
            options=_mutation_options(241, 105),
        ),
        lambda: owner_http.security_custom_assignment_create(
            principal_id,
            int(role.value["role_id"]),
            options=_mutation_options(241, 105),
        ),
    )
    _mutation_pair(
        "security_assignment_revoke",
        lambda: owner_local.security_assignment_revoke(
            auditor_assignment_id,
            options=_mutation_options(251, 106),
        ),
        lambda: owner_http.security_assignment_revoke(
            auditor_assignment_id,
            options=_mutation_options(251, 106),
        ),
    )
    principals = _readback_items(
        owner_local,
        owner_http,
        "security_principal_list",
        lambda client, options: client.security_principal_list(
            limit=1000, options=options
        ),
        252,
    )
    roles = _readback_items(
        owner_local,
        owner_http,
        "security_role_list",
        lambda client, options: client.security_role_list(limit=1000, options=options),
        253,
    )
    assignments = _readback_items(
        owner_local,
        owner_http,
        "security_assignment_list",
        lambda client, options: client.security_assignment_list(
            limit=1000, options=options
        ),
        254,
    )
    if not any(
        item.get("id") == principal_id and item.get("enabled") is True
        for item in principals
    ):
        raise AssertionError("created principal was not durably readable")
    role_id = int(role.value["role_id"])
    if not any(item.get("kind") == "custom" and item.get("id") == role_id for item in roles):
        raise AssertionError("created custom role was not durably readable")
    assignment_ids = {item.get("id") for item in assignments}
    expected_assignments = {
        int(built_in_assignment.value["assignment_id"]),
        int(custom_assignment.value["assignment_id"]),
    }
    if not expected_assignments <= assignment_ids or auditor_assignment_id in assignment_ids:
        raise AssertionError("assignment mutation readback differs")
    _authorization_denied(
        lambda: auditor_local.security_status(options=RequestOptions(request_id=260))
    )
    _authorization_denied(
        lambda: auditor_http.security_status(options=RequestOptions(request_id=260))
    )
    return principal_id


def assert_security_lifecycle(
    owner_local: HyphaeClient,
    owner_http: HyphaeClient,
    principal_id: int,
) -> None:
    issue_arguments = {
        "principal_id": principal_id,
        "label": "Python managed terminal lifecycle",
        "roles": ["reader"],
        "custom_roles": [],
        "permission_ceiling": [
            "catalog.read",
            "credential.self_manage",
            "data.read",
            "discover",
            "proof.generate",
            "proof.verify",
            "search.execute",
        ],
        "scope_ceiling": [{"kind": "instance"}],
        "expires_at_micros": None,
    }
    started = owner_local.security_api_key_issue_start(
        issue_arguments,
        options=_mutation_options(301, 201),
    )
    try:
        owner_http.security_api_key_issue_start(
            issue_arguments,
            options=_mutation_options(302, 201),
        )
    except ProductError as error:
        if error.code != "secret_delivery_consumed":
            raise AssertionError("key Start replay returned the wrong error") from error
    else:
        raise AssertionError("key Start replay redelivered a secret")

    key_id = started.value["key_id"]
    secret = started.value["secret"]
    confirmation_digest = _api_key_confirmation_digest(bytes(secret.expose()))
    activated = owner_local.security_api_key_activate(
        key_id,
        confirmation_digest,
        options=_mutation_options(303, 202),
    )
    replayed_activation = owner_http.security_api_key_activate(
        key_id,
        confirmation_digest,
        options=_mutation_options(304, 202),
    )
    _same_response(activated, replayed_activation, "security_api_key_issue_activate")
    secret.close()

    aborted = owner_local.security_api_key_issue_start(
        {**issue_arguments, "label": "Python managed pending abort"},
        options=_mutation_options(305, 203),
    )
    aborted_key_id = aborted.value["key_id"]
    aborted.value["secret"].close()
    first_abort = owner_http.security_api_key_abort(
        aborted_key_id,
        options=_mutation_options(306, 204),
    )
    replayed_abort = owner_local.security_api_key_abort(
        aborted_key_id,
        options=_mutation_options(307, 204),
    )
    _same_response(first_abort, replayed_abort, "security_api_key_issue_abort")

    first_revoke = owner_local.security_api_key_revoke(
        key_id,
        options=_mutation_options(308, 205),
    )
    replayed_revoke = owner_http.security_api_key_revoke(
        key_id,
        options=_mutation_options(309, 205),
    )
    _same_response(first_revoke, replayed_revoke, "security_api_key_revoke")

    first_legacy_revoke = owner_http.security_legacy_bearer_revoke(
        options=_mutation_options(310, 206)
    )
    replayed_legacy_revoke = owner_local.security_legacy_bearer_revoke(
        options=_mutation_options(311, 206)
    )
    _same_response(
        first_legacy_revoke,
        replayed_legacy_revoke,
        "security_legacy_bearer_revoke",
    )


def _api_key_confirmation_digest(serialized: bytes) -> bytes:
    if (
        len(serialized) != 102
        or not serialized.startswith(b"hyp1_")
        or serialized[37:38] != b"_"
    ):
        raise AssertionError("key Start returned a malformed secret")
    try:
        key_id = bytes.fromhex(serialized[5:37].decode("ascii"))
        key_secret = bytes.fromhex(serialized[38:].decode("ascii"))
    except (UnicodeDecodeError, ValueError) as error:
        raise AssertionError("key Start returned a malformed secret") from error
    from hyphae_sdk.v2.protocol import blake3

    return blake3(b"hyphae-api-key-v1\0" + key_id + key_secret)


def run_live_conformance(arguments: argparse.Namespace) -> dict[str, object]:
    owner_credential = _credential(arguments.owner_key_file)
    auditor_credential = _credential(arguments.auditor_key_file)
    fixture = json.loads(arguments.fixture_metadata.read_text(encoding="utf-8"))
    if set(fixture) != {
        "auditor_assignment_id",
        "key_id",
        "principal_id",
        "schema",
    } or fixture.get("schema") != "hyphae-python-managed-v2-fixture-v1":
        raise RuntimeError("managed fixture metadata is invalid")
    assignment_id = int(fixture["auditor_assignment_id"], 16)

    auditor_transport = LocalTransport(arguments.local_endpoint, api_key=auditor_credential)
    owner_transport = LocalTransport(arguments.local_endpoint, api_key=owner_credential)
    with (
        HyphaeClient(auditor_transport) as auditor_local,
        HyphaeClient(HttpTransport(arguments.http_base_url, bearer_token=auditor_credential))
        as auditor_http,
        HyphaeClient(owner_transport) as owner_local,
        HyphaeClient(HttpTransport(arguments.http_base_url, bearer_token=owner_credential))
        as owner_http,
    ):
        stale_principal_cursor = assert_security_reads(auditor_local, auditor_http)
        if auditor_transport.negotiated_minor != 3:
            raise AssertionError("managed Auditor local transport did not negotiate minor 3")
        lifecycle_principal_id = assert_security_mutations(
            owner_local,
            owner_http,
            auditor_local,
            auditor_http,
            assignment_id,
            stale_principal_cursor,
        )
        assert_security_lifecycle(
            owner_local,
            owner_http,
            lifecycle_principal_id,
        )
        if owner_transport.negotiated_minor != 3:
            raise AssertionError("managed Owner local transport did not negotiate minor 3")
    return {
        "schema": "hyphae-python-managed-v2-transcript-v1",
        "status": "passed",
        "protocol": {"major": 1, "minor": 3},
        "operations": {
            "lifecycle": LIFECYCLE_OPERATIONS,
            "reads": READ_OPERATIONS,
            "writes": WRITE_OPERATIONS,
        },
        "cases": {
            "conflict": True,
            "next_operation_revocation": True,
            "pagination": True,
            "readback": True,
            "redaction": True,
            "replay": True,
            "stale_cursor": True,
            "terminal_replay": True,
        },
    }


def _write_json(path: Path, value: dict[str, object]) -> None:
    canonical = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    path.write_bytes(canonical)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--local-endpoint", required=True)
    parser.add_argument("--http-base-url", required=True)
    parser.add_argument("--owner-key-file", type=Path, required=True)
    parser.add_argument("--auditor-key-file", type=Path, required=True)
    parser.add_argument("--fixture-metadata", type=Path, required=True)
    parser.add_argument("--transcript-out", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    transcript = run_live_conformance(arguments)
    _write_json(arguments.transcript_out, transcript)
    print(json.dumps({"status": "passed"}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
