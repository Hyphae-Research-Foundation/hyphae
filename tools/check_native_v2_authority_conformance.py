#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed checker for managed Native v2 authority conformance."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shlex
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "conformance/v2/authority-cases.json"
SCHEMA = "hyphae-native-v2-authority-cases-v1"
CONTRACT_SCHEMA = "hyphae-native-access-control-v1"
SHA256 = frozenset("0123456789abcdef")

READ_OPERATIONS = {
    "security.assignment_list": "SecurityAssignmentList",
    "security.audit_read": "SecurityAuditRead",
    "security.key_list": "SecurityKeyList",
    "security.principal_list": "SecurityPrincipalList",
    "security.role_list": "SecurityRoleList",
    "security.status": "SecurityStatus",
}
WRITE_OPERATIONS = {
    "security.assignment_create_built_in": "SecurityBuiltInAssignmentCreate",
    "security.assignment_create_custom": "SecurityCustomAssignmentCreate",
    "security.assignment_revoke": "SecurityAssignmentRevoke",
    "security.custom_role_create": "SecurityCustomRoleCreate",
    "security.principal_create": "SecurityPrincipalCreate",
    "security.principal_set_enabled": "SecurityPrincipalSetEnabled",
    "security.legacy_bearer_revoke": "SecurityLegacyBearerRevoke",
}
ALL_OPERATIONS = READ_OPERATIONS | WRITE_OPERATIONS
BUILT_IN_ROLES = {
    "admin",
    "auditor",
    "developer",
    "operator",
    "owner",
    "reader",
    "writer",
}
EXPECTED_ROLE_MATRIX = {
    "admin": {"read": "allow", "write": "allow"},
    "auditor": {"read": "allow", "write": "deny"},
    "developer": {"read": "deny", "write": "deny"},
    "operator": {"read": "audit-only", "write": "deny"},
    "owner": {"read": "allow", "write": "allow"},
    "reader": {"read": "deny", "write": "deny"},
    "writer": {"read": "deny", "write": "deny"},
}
AUTH_CASES = {"expired", "malformed", "missing", "revoked", "wrong"}
AUTH_ERROR = {
    "category": "authorization",
    "code": "authorization_denied",
    "details": [],
    "message": "native product operation is not authorized",
    "retry": "never",
    "transaction_state": "none",
}
WIRE_DIGESTS = {
    "security_read_page_responses_blake3": (
        "67c752f3f510e5b4805e097b284b2ef70fd308fa71dd0778aafc42acdf24dfe8"
    ),
    "security_write_requests_blake3": (
        "94b3aade7ed46f3608da3b30a5516db04a7de0e9013b33ebb3752162f17f1afc"
    ),
    "security_write_responses_blake3": (
        "797963aee6cc4aa65f38b40e08c82a2ff63e71f1e96ef28e2466bc3862e0ce34"
    ),
}
PROTOCOL_REJECTIONS = [
    {
        "case": "minor-0-read",
        "dispatch_reached": False,
        "error_code": "invalid_request",
        "minor": 0,
        "operation_kind": "read",
    },
    {
        "case": "minor-0-write",
        "dispatch_reached": False,
        "error_code": "invalid_request",
        "minor": 0,
        "operation_kind": "write",
    },
    {
        "case": "minor-1-write",
        "dispatch_reached": False,
        "error_code": "invalid_request",
        "minor": 1,
        "operation_kind": "write",
    },
]
REQUIRED_LIMITS = {
    "audit_result_rows",
    "authentication_verifiers_per_request",
    "concurrent_transport_authentication_tasks_per_adapter",
    "security_idempotency_records_per_shard",
    "security_idempotency_shards",
    "security_result_rows",
}
FORBIDDEN_FIELDS = {
    "api_key",
    "serialized",
    "secret",
    "verifier",
    "verifier_digest",
}
REQUIREMENTS = {
    *ALL_OPERATIONS,
    *(f"auth.{case}" for case in AUTH_CASES),
    "digests.request",
    "limits.audit",
    "limits.metadata",
    "pagination.audit",
    "pagination.metadata",
    "protocol.minor-0-read",
    "protocol.minor-0-write",
    "protocol.minor-1-write",
    "redaction.security-metadata",
    "revocation.next-operation",
    "roles.allow-deny",
}
ROLE_MATRIX_EVIDENCE = {
    "anchors": ["built_in_role_matrix_partitions_every_managed_security_write"],
    "command": (
        "cargo test --locked -p hyphae-native-product --test security_write_plane "
        "built_in_role_matrix_partitions_every_managed_security_write"
    ),
    "covers": ["roles.allow-deny"],
    "id": "product-role-matrix",
    "platforms": ["linux", "macos", "windows"],
    "source": "crates/hyphae-native-product/tests/security_write_plane.rs",
}
PYTHON_MANAGED_LIVE_COMMAND = (
    "python tools/run_python_managed_v2_conformance.py --binary target/debug/hyphae "
    "--fixture-binary target/debug/hyphae-v2-fixture "
    "--wheel dist/hyphae_sdk-1.2.2-py3-none-any.whl "
    "--output python-managed-v2-conformance.json"
)
PYTHON_MANAGED_LIVE_ID = "python-managed-live"


class AuthorityConformanceError(ValueError):
    """The checked authority corpus is incomplete or inconsistent."""


def fail(message: str) -> None:
    raise AuthorityConformanceError(message)


def exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    if set(value) != expected:
        fail(f"{context} fields differ from the authority corpus")


def canonical_digest(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def authority_contract_digest(contract: dict[str, Any]) -> str:
    semantic_contract = {
        key: value for key, value in contract.items() if key != "$comment"
    }
    return canonical_digest(semantic_contract)


def sorted_unique_strings(value: Any, context: str) -> list[str]:
    if (
        not isinstance(value, list)
        or any(not isinstance(item, str) or not item for item in value)
        or value != sorted(set(value))
    ):
        fail(f"{context} must be sorted unique nonempty strings")
    return value


def validate_digest(value: Any, context: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or not set(value) <= SHA256:
        fail(f"{context} must be a lowercase 256-bit digest")
    return value


def load_contract(path: Path) -> dict[str, Any]:
    try:
        contract = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot load authority contract: {error}")
    if not isinstance(contract, dict) or contract.get("schema") != CONTRACT_SCHEMA:
        fail("authority contract schema differs")
    if contract.get("$comment") != "SPDX-License-Identifier: Apache-2.0":
        fail("authority contract SPDX marker differs")
    return contract


def validate_operations(corpus: dict[str, Any], contract: dict[str, Any]) -> None:
    rows = corpus.get("operations")
    if not isinstance(rows, list) or len(rows) != len(ALL_OPERATIONS):
        fail("authority corpus must contain the exact managed operations")
    ids = [row.get("id") for row in rows if isinstance(row, dict)]
    if ids != sorted(ALL_OPERATIONS) or len(ids) != len(rows):
        fail("authority corpus must contain the exact managed operations")
    contract_rows = {
        row.get("id"): row
        for row in contract.get("operations", [])
        if isinstance(row, dict) and row.get("id") in ALL_OPERATIONS
    }
    if set(contract_rows) != set(ALL_OPERATIONS):
        fail("access-control contract is missing a conformance operation")
    expected_fields = {
        "allowed_roles",
        "denied_roles",
        "id",
        "kind",
        "minimum_minor",
        "permission",
        "scope",
        "source_variant",
    }
    for row in rows:
        exact_keys(row, expected_fields, "operation row")
        operation_id = row["id"]
        kind = "read" if operation_id in READ_OPERATIONS else "write"
        source_variant = ALL_OPERATIONS[operation_id]
        contract_row = contract_rows[operation_id]
        expected = {
            "allowed_roles": contract_row["allowed_roles"],
            "denied_roles": sorted(BUILT_IN_ROLES - set(contract_row["allowed_roles"])),
            "id": operation_id,
            "kind": kind,
            "minimum_minor": 1 if kind == "read" else (3 if operation_id == "security.legacy_bearer_revoke" else 2),
            "permission": contract_row["required_all"][0],
            "scope": "instance",
            "source_variant": source_variant,
        }
        if row != expected:
            fail(f"operation matrix differs for {operation_id}")
        if (
            contract_row.get("status") != "current"
            or contract_row.get("classification") != "fixed"
            or contract_row.get("scope_resolution") != "instance"
            or contract_row.get("inherits_underlying") is not False
            or contract_row.get("source_variant") != source_variant
            or len(contract_row.get("required_all", [])) != 1
        ):
            fail(f"operation matrix contract differs for {operation_id}")


def validate_authentication(corpus: dict[str, Any]) -> None:
    rows = corpus.get("authentication_denial")
    if not isinstance(rows, list) or len(rows) != len(AUTH_CASES):
        fail("authentication denial cases differ")
    cases = [row.get("case") for row in rows if isinstance(row, dict)]
    if cases != sorted(AUTH_CASES):
        fail("authentication denial cases differ")
    for row in rows:
        exact_keys(row, {"case", "error"}, "authentication denial row")
        if row["error"] != AUTH_ERROR:
            fail("authentication denials are not uniform")


def validate_protocol(corpus: dict[str, Any]) -> None:
    protocol = corpus.get("protocol")
    if not isinstance(protocol, dict):
        fail("protocol policy is missing")
    exact_keys(
        protocol,
        {"current_major", "current_minor", "rejections_before_dispatch"},
        "protocol policy",
    )
    if protocol["current_major"] != 1 or protocol["current_minor"] != 3:
        fail("protocol authority must describe Native 1.3")
    if protocol["rejections_before_dispatch"] != PROTOCOL_REJECTIONS:
        fail("minor 0/1 operations must fail before dispatch")


def function_slice(source: str, name: str, next_name: str) -> str:
    start = source.find(f"fn {name}")
    end = source.find(f"fn {next_name}", start + 1)
    if start < 0 or end < 0:
        fail(f"Native protocol source function {name} is missing")
    return source[start:end]


def validate_native_sources(root: Path, corpus: dict[str, Any]) -> None:
    try:
        handshake = (root / "crates/hyphae-native-protocol/src/handshake.rs").read_text(
            encoding="utf-8"
        )
        product = (root / "crates/hyphae-native-protocol/src/product.rs").read_text(
            encoding="utf-8"
        )
        errors = (root / "crates/hyphae-native-product/src/error.rs").read_text(
            encoding="utf-8"
        )
        goldens = (root / "crates/hyphae-native-protocol/tests/golden_vectors.rs").read_text(
            encoding="utf-8"
        )
    except OSError as error:
        fail(f"cannot load Native authority source: {error}")
    if (
        "pub const PROTOCOL_MAJOR: u16 = 1;" not in handshake
        or "pub const PROTOCOL_MINOR: u16 = 3;" not in handshake
    ):
        fail("Native protocol version differs from the corpus")
    minor_body = function_slice(product, "ensure_operation_minor", "ensure_response_minor")
    read_body, separator, remaining = minor_body.partition("=> 1,")
    write_body, separator_two, remaining_writes = remaining.partition("=> 2,")
    if not separator or not separator_two:
        fail("Native protocol minor admission is not canonical")
    variant_pattern = r"ProductOperation::(Security[A-Za-z0-9]+)"
    if set(re.findall(variant_pattern, read_body)) != set(READ_OPERATIONS.values()):
        fail("Native protocol read minor admission differs")
    if set(re.findall(variant_pattern, write_body)) != set(WRITE_OPERATIONS.values()):
        expected_minor_two = set(WRITE_OPERATIONS.values()) - {"SecurityLegacyBearerRevoke"}
        if set(re.findall(variant_pattern, write_body)) != expected_minor_two:
            fail("Native protocol write minor admission differs")
    if "SecurityLegacyBearerRevoke" not in remaining_writes:
        fail("Native protocol terminal legacy revocation minor admission differs")
    idempotency_body = function_slice(
        product, "operation_requires_idempotency", "operation_is_key_lifecycle"
    )
    lifecycle_variants = {
        "SecurityApiKeyIssueSelfStart", "SecurityApiKeyIssueStart",
        "SecurityApiKeyIssueSelfActivate", "SecurityApiKeyIssueActivate",
        "SecurityApiKeyRotateSelfStart", "SecurityApiKeyRotateStart",
        "SecurityApiKeyRotateSelfActivate", "SecurityApiKeyRotateActivate",
        "SecurityApiKeyIssueSelfAbort", "SecurityApiKeyIssueAbort",
        "SecurityApiKeyRotateSelfAbort", "SecurityApiKeyRotateAbort",
        "SecurityApiKeyRevokeSelf", "SecurityApiKeyRevoke",
    }
    if set(re.findall(variant_pattern, idempotency_body)) != set(WRITE_OPERATIONS.values()) | lifecycle_variants:
        fail("Native security write idempotency admission differs")
    authorization_definition = re.compile(
        r"ProductErrorCode::AuthorizationDenied,\s*"
        r"ProductErrorCategory::Authorization,\s*"
        r"Some\(ProductRetry::Never\),\s*"
        r'"native product operation is not authorized",'
    )
    if authorization_definition.search(errors) is None:
        fail("Native uniform authorization error differs")
    if any(value not in goldens for value in WIRE_DIGESTS.values()):
        fail("Native security wire digest evidence differs")
    if any(corpus["digests"].get(name) != value for name, value in WIRE_DIGESTS.items()):
        fail("Native security wire digests differ")


def validate_pagination(corpus: dict[str, Any], limits: dict[str, Any]) -> None:
    pagination = corpus.get("pagination")
    if not isinstance(pagination, dict):
        fail("pagination policy is missing")
    exact_keys(pagination, {"audit", "catalog_visible", "metadata"}, "pagination policy")
    metadata = pagination["metadata"]
    audit = pagination["audit"]
    catalog_visible = pagination["catalog_visible"]
    expected_metadata = {
        "cursor": "authorization_epoch+after_id",
        "deterministic": True,
        "limit": limits["security_result_rows"],
        "operations": sorted(set(READ_OPERATIONS) - {"security.audit_read", "security.status"}),
        "stale_cursor_error": "catalog_conflict",
    }
    expected_audit = {
        "cursor": "retained_event_id",
        "deterministic": True,
        "limit": limits["audit_result_rows"],
        "operation": "security.audit_read",
        "outside_retention_error": "invalid_request",
    }
    if metadata != expected_metadata:
        fail("metadata pagination differs from the canonical policy")
    if audit != expected_audit:
        fail("audit pagination differs from the canonical policy")
    if catalog_visible != {
        "cursor": "opaque_authenticated_bytes",
        "minimum_minor": 3,
        "cross_authority_error": "catalog_conflict",
        "stale_cursor_error": "catalog_conflict",
    }:
        fail("catalog visible pagination differs from the canonical policy")


def validate_limits_and_redaction(corpus: dict[str, Any], contract: dict[str, Any]) -> None:
    limits = corpus.get("limits")
    contract_limits = contract.get("limits")
    if not isinstance(limits, dict) or not isinstance(contract_limits, dict):
        fail("limits are missing")
    expected_limits = {key: contract_limits.get(key) for key in sorted(REQUIRED_LIMITS)}
    if limits != expected_limits or any(
        not isinstance(value, int) or value <= 0 for value in limits.values()
    ):
        fail("limits differ from the access-control contract")
    redaction = corpus.get("redaction")
    if not isinstance(redaction, dict):
        fail("redaction policy is missing")
    exact_keys(
        redaction,
        {"forbidden_fields", "key_summary_is_secret_free", "wire_is_redacted"},
        "redaction policy",
    )
    if (
        set(sorted_unique_strings(redaction["forbidden_fields"], "redaction fields"))
        != FORBIDDEN_FIELDS
        or redaction["key_summary_is_secret_free"] is not True
        or redaction["wire_is_redacted"] is not True
    ):
        fail("redaction policy differs")


def validate_role_matrix_and_revocation(corpus: dict[str, Any]) -> None:
    role_matrix = corpus.get("role_matrix")
    if role_matrix != EXPECTED_ROLE_MATRIX:
        fail("role matrix differs from operation authority")
    revocation = corpus.get("revocation")
    expected = {
        "connection_reuse": "same-connection",
        "effective": "next-operation",
        "error_code": "authorization_denied",
        "reconnect_required": False,
    }
    if revocation != expected:
        fail("revocation must deny the next operation on the same connection")


def validate_digests(corpus: dict[str, Any], contract: dict[str, Any]) -> None:
    digests = corpus.get("digests")
    if not isinstance(digests, dict):
        fail("digests are missing")
    exact_keys(
        digests,
        {
            "authentication_error_sha256",
            "authority_contract_sha256",
            "operation_matrix_sha256",
            "role_matrix_sha256",
            *WIRE_DIGESTS,
        },
        "digests",
    )
    for name, value in digests.items():
        validate_digest(value, name)
    if digests["authority_contract_sha256"] != authority_contract_digest(contract):
        fail("authority contract digest differs")
    if digests["operation_matrix_sha256"] != canonical_digest(corpus["operations"]):
        fail("operation matrix digest differs")
    if digests["role_matrix_sha256"] != canonical_digest(corpus["role_matrix"]):
        fail("role matrix digest differs")
    if digests["authentication_error_sha256"] != canonical_digest(AUTH_ERROR):
        fail("authentication digest differs")
    if any(digests[name] != value for name, value in WIRE_DIGESTS.items()):
        fail("Native security wire digests differ")


def validate_evidence(corpus: dict[str, Any], root: Path) -> None:
    rows = corpus.get("evidence")
    if not isinstance(rows, list) or len(rows) != 9:
        fail("exactly nine evidence rows are required")
    ids = [row.get("id") for row in rows if isinstance(row, dict)]
    if ids != sorted(set(ids)) or len(ids) != len(rows):
        fail("evidence identifiers must be sorted and unique")
    coverage: set[str] = set()
    commands: set[str] = set()
    for row in rows:
        exact_keys(
            row,
            {"anchors", "command", "covers", "id", "platforms", "source"},
            "evidence row",
        )
        covers = set(sorted_unique_strings(row["covers"], "evidence coverage"))
        if not covers or not covers <= REQUIREMENTS:
            fail("evidence coverage contains empty or unknown requirements")
        coverage.update(covers)
        command = row["command"]
        is_python_live = row.get("id") == PYTHON_MANAGED_LIVE_ID
        if is_python_live and command != PYTHON_MANAGED_LIVE_COMMAND:
            fail("Python managed live evidence differs")
        valid_command = isinstance(command, str) and (
            is_python_live
            or (
                command.startswith("cargo test ")
                and "--locked" in shlex.split(command)
            )
        )
        if command in commands or not valid_command:
            fail("evidence commands must be unique reviewed commands")
        commands.add(command)
        source = row["source"]
        if not isinstance(source, str) or Path(source).is_absolute():
            fail("evidence source must be repository-relative")
        path = root / source
        if not path.is_file():
            fail(f"evidence source is missing: {source}")
        source_text = path.read_text(encoding="utf-8")
        anchors = sorted_unique_strings(row["anchors"], "evidence anchors")
        if not anchors or any(anchor not in source_text for anchor in anchors):
            fail(f"evidence anchor is missing from {source}")
        platforms = sorted_unique_strings(row["platforms"], "evidence platforms")
        if not set(platforms) <= {"linux", "macos", "windows"}:
            fail("evidence platforms differ")
        if is_python_live and (
            platforms != ["linux", "macos", "windows"]
            or row.get("source") != "conformance/v2/python_managed_live.py"
            or row.get("anchors")
            != ["assert_security_mutations", "assert_security_reads", "run_live_conformance"]
        ):
            fail("Python managed live evidence differs")
    if coverage != REQUIREMENTS:
        fail("evidence coverage differs from the required authority cases")
    role_matrix_rows = [
        row for row in rows if "roles.allow-deny" in row.get("covers", [])
    ]
    if role_matrix_rows != [ROLE_MATRIX_EVIDENCE]:
        fail("role matrix evidence must name the exhaustive security write-plane test")


def validate(
    corpus: dict[str, Any],
    contract_path: Path = ROOT / "contracts/native-access-control-v1.json",
    root: Path = ROOT,
) -> dict[str, Any]:
    """Validate the canonical corpus against live contracts and evidence."""

    if not isinstance(corpus, dict):
        fail("corpus must be an object")
    exact_keys(
        corpus,
        {
            "authentication_denial",
            "digests",
            "evidence",
            "limits",
            "operations",
            "pagination",
            "protocol",
            "redaction",
            "revocation",
            "role_matrix",
            "schema",
        },
        "corpus",
    )
    if corpus["schema"] != SCHEMA:
        fail("corpus schema differs")
    contract = load_contract(contract_path)
    validate_operations(corpus, contract)
    validate_authentication(corpus)
    validate_protocol(corpus)
    validate_digests(corpus, contract)
    validate_native_sources(root, corpus)
    validate_limits_and_redaction(corpus, contract)
    validate_pagination(corpus, corpus["limits"])
    validate_role_matrix_and_revocation(corpus)
    validate_evidence(corpus, root)
    return {
        "status": "passed",
        "operations": len(corpus["operations"]),
        "authentication_denials": len(corpus["authentication_denial"]),
        "evidence_rows": len(corpus["evidence"]),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, default=CORPUS)
    parser.add_argument(
        "--contract", type=Path, default=ROOT / "contracts/native-access-control-v1.json"
    )
    args = parser.parse_args()
    try:
        corpus = json.loads(args.corpus.read_text(encoding="utf-8"))
        result = validate(corpus, args.contract, ROOT)
    except (OSError, json.JSONDecodeError, AuthorityConformanceError) as error:
        print(f"Native v2 authority conformance failed: {error}")
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
