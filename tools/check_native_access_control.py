#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed checker for the Native access-control v1 design contract."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "hyphae-native-access-control-v1"
PERMISSION = re.compile(r"^[a-z][a-z0-9]*(?:\.[a-z][a-z0-9_]*)*$")
OPERATION = re.compile(r"^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$")
VARIANT = re.compile(r"^    ([A-Z][A-Za-z0-9]*)(?:\s*\{|\(|,)")
SCOPE_KINDS = {"instance", "catalog_subtree", "catalog_object"}
OPERATION_SCOPE_RESOLUTION = {
    "instance",
    "request_object",
    "resolved_name",
    "request_page",
    "parent_object",
    "prepared_objects",
    "bound_sql_objects",
    "bound_sql_parent_objects",
    "default_keyspace",
    "transaction_union",
    "originating_principal",
    "underlying_operation",
}

EXPECTED_PERMISSIONS = {
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
}

EXPECTED_ROLES = {
    "admin": EXPECTED_PERMISSIONS - {"ownership.manage"},
    "auditor": {
        "audit.read",
        "backup.verify",
        "catalog.read",
        "credential.self_manage",
        "discover",
        "observe",
        "proof.verify",
        "security.read",
    },
    "developer": {
        "catalog.read",
        "catalog.write",
        "credential.self_manage",
        "data.read",
        "data.write",
        "discover",
        "observe",
        "proof.generate",
        "proof.verify",
        "search.execute",
    },
    "operator": {
        "audit.read",
        "backup.create",
        "backup.verify",
        "catalog.read",
        "credential.self_manage",
        "discover",
        "maintain",
        "observe",
        "proof.verify",
    },
    "owner": EXPECTED_PERMISSIONS,
    "reader": {
        "catalog.read",
        "credential.self_manage",
        "data.read",
        "discover",
        "proof.generate",
        "proof.verify",
        "search.execute",
    },
    "writer": {
        "catalog.read",
        "credential.self_manage",
        "data.read",
        "data.write",
        "discover",
        "proof.generate",
        "proof.verify",
        "search.execute",
    },
}

EXPECTED_LIMITS = {
    "principals": 4096,
    "custom_roles": 1024,
    "grants_per_role": 256,
    "assignments_per_principal": 128,
    "keys_per_principal": 64,
    "display_name_bytes": 128,
    "audit_event_bytes": 4096,
    "retained_audit_events": 100000,
    "audit_result_rows": 1000,
    "maximum_rotation_overlap_seconds": 604800,
    "authentication_verifiers_per_request": 1,
    "authorization_cache_entries": 4096,
}


class AccessControlValidationError(ValueError):
    """The access-control authority is incomplete or internally inconsistent."""


def fail(message: str) -> None:
    raise AccessControlValidationError(message)


def exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    if set(value) != expected:
        fail(f"{context} fields differ from the contract")


def sorted_unique_strings(value: Any, context: str) -> list[str]:
    if (
        not isinstance(value, list)
        or any(not isinstance(item, str) or not item for item in value)
        or value != sorted(set(value))
    ):
        fail(f"{context} must be sorted unique nonempty strings")
    return value


def product_operation_variants(source: Path) -> set[str]:
    text = source.read_text(encoding="utf-8")
    marker = "pub enum ProductOperation"
    start = text.find(marker)
    if start < 0:
        fail("ProductOperation source enum is missing")
    opening = text.find("{", start + len(marker))
    if opening < 0:
        fail("ProductOperation source enum has no body")
    depth = 0
    closing = -1
    for offset, character in enumerate(text[opening:], start=opening):
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth == 0:
                closing = offset
                break
    if closing < 0:
        fail("ProductOperation source enum is unterminated")
    variants = {
        match.group(1)
        for line in text[opening + 1 : closing].splitlines()
        if (match := VARIANT.match(line)) is not None
    }
    if not variants:
        fail("ProductOperation source enum has no recognized variants")
    return variants


def validate_permissions(contract: dict[str, Any]) -> dict[str, set[str]]:
    rows = contract.get("permissions")
    if not isinstance(rows, list):
        fail("permissions must be a list")
    ids: list[str] = []
    scopes_by_permission: dict[str, set[str]] = {}
    for row in rows:
        if not isinstance(row, dict):
            fail("permission row must be an object")
        exact_keys(row, {"id", "scopes"}, "permission row")
        permission = row["id"]
        if not isinstance(permission, str) or PERMISSION.fullmatch(permission) is None:
            fail("permission identifier is not canonical")
        scopes = set(sorted_unique_strings(row["scopes"], f"permission {permission} scopes"))
        if not scopes or not scopes.issubset(SCOPE_KINDS):
            fail(f"permission {permission} contains an invalid scope")
        ids.append(permission)
        scopes_by_permission[permission] = scopes
    if ids != sorted(set(ids)) or set(ids) != EXPECTED_PERMISSIONS:
        fail("permission registry differs from Native access-control v1")
    return scopes_by_permission


def validate_roles(contract: dict[str, Any]) -> dict[str, set[str]]:
    rows = contract.get("built_in_roles")
    if not isinstance(rows, list):
        fail("built_in_roles must be a list")
    roles: dict[str, set[str]] = {}
    role_ids: list[str] = []
    for row in rows:
        if not isinstance(row, dict):
            fail("built-in role row must be an object")
        exact_keys(row, {"id", "permissions"}, "built-in role row")
        role = row["id"]
        if not isinstance(role, str) or OPERATION.fullmatch(role) is None:
            fail("built-in role identifier is not canonical")
        permissions = set(
            sorted_unique_strings(row["permissions"], f"built-in role {role} permissions")
        )
        if not permissions.issubset(EXPECTED_PERMISSIONS):
            fail(f"built-in role {role} contains an unknown permission")
        roles[role] = permissions
        role_ids.append(role)
    if role_ids != sorted(set(role_ids)) or roles != EXPECTED_ROLES:
        fail("built-in role registry differs from Native access-control v1")
    if {"data.read", "data.write", "catalog.write", "restore", "security.manage"} & roles["operator"]:
        fail("operator role exceeds its least-privilege boundary")
    if {"data.read", "data.write", "catalog.write", "restore", "security.manage"} & roles["auditor"]:
        fail("auditor role exceeds its metadata-only boundary")
    return roles


def validate_operations(
    contract: dict[str, Any], roles: dict[str, set[str]], source: Path
) -> tuple[int, int]:
    rows = contract.get("operations")
    if not isinstance(rows, list) or not rows:
        fail("operations must be a nonempty list")
    expected_fields = {
        "id",
        "status",
        "source_variant",
        "classification",
        "required_all",
        "scope_resolution",
        "inherits_underlying",
        "allowed_roles",
    }
    seen_ids: set[str] = set()
    covered_variants: set[str] = set()
    operations_by_variant: dict[str, list[dict[str, Any]]] = {}
    planned = 0
    for row in rows:
        if not isinstance(row, dict):
            fail("operation row must be an object")
        exact_keys(row, expected_fields, "operation row")
        operation = row["id"]
        if (
            not isinstance(operation, str)
            or OPERATION.fullmatch(operation) is None
            or operation in seen_ids
        ):
            fail("operation identifier is duplicate or noncanonical")
        seen_ids.add(operation)
        status = row["status"]
        if status not in {"current", "planned-1.2"}:
            fail(f"operation {operation} has an invalid status")
        variant = row["source_variant"]
        if status == "current":
            if not isinstance(variant, str) or not variant:
                fail(f"current operation {operation} has no source variant")
            covered_variants.add(variant)
            operations_by_variant.setdefault(variant, []).append(row)
        else:
            planned += 1
            if variant is not None:
                fail(f"planned operation {operation} claims a current source variant")
        required = sorted_unique_strings(row["required_all"], f"operation {operation} permissions")
        if not required or not set(required).issubset(EXPECTED_PERMISSIONS):
            fail(f"operation {operation} lacks canonical required permissions")
        allowed = sorted_unique_strings(row["allowed_roles"], f"operation {operation} roles")
        expected_allowed = sorted(
            role for role, permissions in roles.items() if set(required).issubset(permissions)
        )
        if allowed != expected_allowed:
            fail(f"operation {operation} role matrix differs from its permissions")
        if row["scope_resolution"] not in OPERATION_SCOPE_RESOLUTION:
            fail(f"operation {operation} has an invalid scope resolver")
        if not isinstance(row["classification"], str) or not row["classification"]:
            fail(f"operation {operation} has no classifier")
        if not isinstance(row["inherits_underlying"], bool):
            fail(f"operation {operation} has an invalid inheritance flag")

    source_variants = product_operation_variants(source)
    if covered_variants != source_variants:
        missing = sorted(source_variants - covered_variants)
        unknown = sorted(covered_variants - source_variants)
        fail(f"ProductOperation matrix drift: missing={missing}, unknown={unknown}")

    require_variant_rule(operations_by_variant, "Backup", ["backup.create"], False)
    require_variant_rule(operations_by_variant, "Restore", ["restore"], False)
    require_variant_rule(operations_by_variant, "AdminStatus", ["observe"], False)
    require_variant_rule(operations_by_variant, "Telemetry", ["observe"], False)
    require_variant_rule(operations_by_variant, "AdminCheckpoint", ["maintain"], False)
    require_variant_rule(operations_by_variant, "Doctor", ["maintain"], False)
    require_variant_rule(operations_by_variant, "Prove", ["proof.generate"], True)

    sql_rows = operations_by_variant.get("ExecuteSql", [])
    sql_rules = {
        row["classification"]: tuple(row["required_all"]) for row in sql_rows
    }
    if sql_rules != {
        "parsed_and_bound_ddl": ("catalog.write",),
        "parsed_and_bound_dml": ("catalog.read", "data.write"),
        "parsed_and_bound_read": ("catalog.read", "data.read"),
    }:
        fail("ExecuteSql must distinguish parsed read, DML, and DDL authority")
    return len(source_variants), planned


def require_variant_rule(
    operations: dict[str, list[dict[str, Any]]],
    variant: str,
    required: list[str],
    inherits_underlying: bool,
) -> None:
    rows = operations.get(variant)
    if (
        rows is None
        or len(rows) != 1
        or rows[0]["required_all"] != required
        or rows[0]["inherits_underlying"] is not inherits_underlying
    ):
        fail(f"{variant} permission rule differs from the normative boundary")


def validate_key_format(contract: dict[str, Any]) -> None:
    key_format = contract.get("key_format")
    if not isinstance(key_format, dict):
        fail("key_format must be an object")
    exact_keys(
        key_format,
        {
            "version",
            "prefix",
            "alphabet",
            "key_id_bits",
            "key_id_characters",
            "secret_bits",
            "secret_characters",
            "serialized_bytes",
            "pattern",
            "verifier",
            "verifier_domain_hex",
            "secret_return",
        },
        "key_format",
    )
    expected = {
        "version": 1,
        "prefix": "hyp1_",
        "alphabet": "lowercase-hexadecimal",
        "key_id_bits": 128,
        "key_id_characters": 32,
        "secret_bits": 256,
        "secret_characters": 64,
        "serialized_bytes": 102,
        "pattern": r"^hyp1_[0-9a-f]{32}_[0-9a-f]{64}$",
        "verifier": "blake3-domain-separated-v1",
        "verifier_domain_hex": "6879706861652d6170692d6b65792d763100",
        "secret_return": "restricted-file-once",
    }
    if key_format != expected:
        fail("key format differs from Native access-control v1")
    expression = re.compile(key_format["pattern"])
    canonical = f"hyp1_{'1' * 32}_{'a' * 64}"
    if len(canonical.encode()) != key_format["serialized_bytes"] or expression.fullmatch(canonical) is None:
        fail("key format length or grammar is internally inconsistent")
    for invalid in (
        canonical.upper(),
        canonical + "\n",
        canonical[:-1],
        "hyp2_" + canonical[5:],
    ):
        if expression.fullmatch(invalid) is not None:
            fail("key format accepts a noncanonical credential")


def validate_policies(contract: dict[str, Any]) -> None:
    scope = contract.get("scope_policy")
    authorization = contract.get("authorization_policy")
    bootstrap = contract.get("bootstrap_policy")
    migration = contract.get("migration_policy")
    recovery = contract.get("recovery_policy")
    audit = contract.get("audit_policy")
    for name, value in (
        ("scope_policy", scope),
        ("authorization_policy", authorization),
        ("bootstrap_policy", bootstrap),
        ("migration_policy", migration),
        ("recovery_policy", recovery),
        ("audit_policy", audit),
    ):
        if not isinstance(value, dict):
            fail(f"{name} must be an object")
    if scope.get("kinds") != ["instance", "catalog_subtree", "catalog_object"]:
        fail("scope policy does not define the canonical scope set")
    if scope.get("name_is_authority") is not False:
        fail("scope policy permits mutable names as authority")
    required_authorization = {
        "deny_by_default": True,
        "role_inheritance": False,
        "negative_grants": False,
        "key_may_widen_principal": False,
        "revocation_effective": "next-operation",
        "expiry_check": "every-operation",
        "prepared_reauthorization": True,
        "commit_reauthorization": True,
        "unknown_state": "deny",
    }
    if authorization != required_authorization:
        fail("authorization policy weakens the fail-closed contract")
    if (
        bootstrap.get("mode") != "offline-exclusive-lock"
        or bootstrap.get("allowed_when") != "no-durable-principals"
        or bootstrap.get("initial_role") != "owner"
        or bootstrap.get("terminal_output_default") is not False
    ):
        fail("bootstrap policy differs from the offline one-owner boundary")
    if (
        migration.get("legacy_bearer_minor_releases") != 1
        or migration.get("automatic_persistence") is not False
        or migration.get("explicit_legacy_revocation") is not True
    ):
        fail("legacy bearer migration is not bounded and explicit")
    if (
        recovery.get("mode") != "offline-exclusive-lock"
        or recovery.get("revokes_existing_owner_credentials") is not True
        or recovery.get("increments_authorization_epoch") is not True
        or recovery.get("changes_user_data") is not False
        or recovery.get("bypasses_integrity_validation") is not False
    ):
        fail("owner recovery weakens the offline fail-closed boundary")
    if audit.get("secrets_allowed") is not False or audit.get("authentication_failures") != "bounded-telemetry":
        fail("audit policy can leak secrets or amplify unauthenticated writes")


def validate_limits(contract: dict[str, Any]) -> None:
    limits = contract.get("limits")
    if limits != EXPECTED_LIMITS:
        fail("access-control limits differ from the bounded v1 defaults")
    if any(isinstance(value, bool) or not isinstance(value, int) or value <= 0 for value in limits.values()):
        fail("access-control limits must be positive integers")


def validate_documents(repository: Path) -> None:
    references = {
        "docs/adr/0028-native-identity-rbac-and-api-keys.md": "contracts/native-access-control-v1.json",
        "docs/native/access-control-v1.md": "hyp1_<key_id>_<secret>",
        "docs/security/native-access-control-threat-model.md": "Authorization invariants",
    }
    for relative, marker in references.items():
        path = repository / relative
        if not path.is_file() or marker not in path.read_text(encoding="utf-8"):
            fail(f"required access-control document is missing its authority marker: {relative}")


def validate(contract: dict[str, Any], source: Path, repository: Path = ROOT) -> dict[str, Any]:
    if not isinstance(contract, dict):
        fail("access-control contract must be an object")
    exact_keys(
        contract,
        {
            "$comment",
            "schema",
            "status",
            "permission_encoding",
            "permissions",
            "built_in_roles",
            "operations",
            "key_format",
            "scope_policy",
            "authorization_policy",
            "bootstrap_policy",
            "migration_policy",
            "recovery_policy",
            "audit_policy",
            "limits",
        },
        "access-control contract",
    )
    if contract["$comment"] != "SPDX-License-Identifier: AGPL-3.0-only":
        fail("access-control contract lacks its canonical SPDX marker")
    if contract["schema"] != SCHEMA or contract["status"] != "normative-design-implementation-pending":
        fail("access-control schema identity or honest status is invalid")
    if contract["permission_encoding"] != {
        "internal_bits": 64,
        "wire_encoding": "lowercase-dotted-ascii",
        "unknown_permission": "deny",
    }:
        fail("permission encoding differs from the contract")
    validate_permissions(contract)
    roles = validate_roles(contract)
    current_variants, planned = validate_operations(contract, roles, source)
    validate_key_format(contract)
    validate_policies(contract)
    validate_limits(contract)
    validate_documents(repository)
    return {
        "schema": SCHEMA,
        "status": "contract-complete-implementation-pending",
        "permissions": len(EXPECTED_PERMISSIONS),
        "built_in_roles": len(EXPECTED_ROLES),
        "current_product_variants": current_variants,
        "planned_operations": planned,
        "key_version": 1,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--contract",
        type=Path,
        default=ROOT / "contracts/native-access-control-v1.json",
    )
    parser.add_argument(
        "--operation-source",
        type=Path,
        default=ROOT / "crates/hyphae-native-product/src/operation.rs",
    )
    args = parser.parse_args()
    try:
        contract = json.loads(args.contract.read_text(encoding="utf-8"))
        result = validate(contract, args.operation_source)
    except (AccessControlValidationError, OSError, json.JSONDecodeError) as error:
        print(f"native access-control validation failed: {error}")
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
