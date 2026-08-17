#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the Native access-control v1 design contract checker."""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_access_control import (
    AccessControlValidationError,
    ROOT,
    product_operation_variants,
    validate,
)


CONTRACT = ROOT / "contracts/native-access-control-v1.json"
SOURCE = ROOT / "crates/hyphae-native-product/src/operation.rs"

READ_ONLY_SLICE = {
    "backup.verify": "backup.verify",
}

CURRENT_SECURITY_READ_SLICE = {
    "security.assignment_list": "security.read",
    "security.audit_read": "audit.read",
    "security.key_list": "security.read",
    "security.principal_list": "security.read",
    "security.role_list": "security.read",
    "security.status": "security.read",
}

CURRENT_SECURITY_WRITE_SLICE = {
    "security.assignment_create_built_in": "SecurityBuiltInAssignmentCreate",
    "security.assignment_create_custom": "SecurityCustomAssignmentCreate",
    "security.assignment_revoke": "SecurityAssignmentRevoke",
    "security.custom_role_create": "SecurityCustomRoleCreate",
    "security.principal_create": "SecurityPrincipalCreate",
    "security.principal_set_enabled": "SecurityPrincipalSetEnabled",
}


def payload() -> dict:
    return json.loads(CONTRACT.read_text(encoding="utf-8"))


def operation(contract: dict, operation_id: str) -> dict:
    return next(row for row in contract["operations"] if row["id"] == operation_id)


def role(contract: dict, role_id: str) -> dict:
    return next(row for row in contract["built_in_roles"] if row["id"] == role_id)


class NativeAccessControlContractTests(unittest.TestCase):
    def test_checked_in_contract_is_complete_and_current(self) -> None:
        result = validate(payload(), SOURCE)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["permissions"], 18)
        self.assertEqual(result["built_in_roles"], 7)
        self.assertEqual(result["current_product_variants"], 69)
        self.assertEqual(result["planned_operations"], 2)

    def test_backup_verify_remains_planned_and_instance_scoped(self) -> None:
        contract = payload()
        for operation_id, permission in READ_ONLY_SLICE.items():
            with self.subTest(operation=operation_id):
                row = operation(contract, operation_id)
                self.assertEqual(row["status"], "planned-1.2")
                self.assertIsNone(row["source_variant"])
                self.assertEqual(row["classification"], "fixed")
                self.assertEqual(row["required_all"], [permission])
                self.assertEqual(row["scope_resolution"], "instance")
                self.assertFalse(row["inherits_underlying"])

    def test_backup_verify_cannot_claim_current_without_a_safe_transport_boundary(self) -> None:
        for operation_id in READ_ONLY_SLICE:
            with self.subTest(operation=operation_id):
                contract = payload()
                row = operation(contract, operation_id)
                row["status"] = "current"
                row["source_variant"] = "Capabilities"
                with self.assertRaisesRegex(
                    AccessControlValidationError,
                    f"planned read-only operation {operation_id}",
                ):
                    validate(contract, SOURCE)

    def test_security_read_slice_is_current_and_instance_scoped(self) -> None:
        contract = payload()
        for operation_id, permission in CURRENT_SECURITY_READ_SLICE.items():
            with self.subTest(operation=operation_id):
                row = operation(contract, operation_id)
                self.assertEqual(row["status"], "current")
                self.assertIsInstance(row["source_variant"], str)
                self.assertEqual(row["classification"], "fixed")
                self.assertEqual(row["required_all"], [permission])
                self.assertEqual(row["scope_resolution"], "instance")
                self.assertFalse(row["inherits_underlying"])

    def test_security_read_slice_cannot_regress_to_planned(self) -> None:
        for operation_id in CURRENT_SECURITY_READ_SLICE:
            with self.subTest(operation=operation_id):
                contract = payload()
                row = operation(contract, operation_id)
                row["status"] = "planned-1.2"
                row["source_variant"] = None
                with self.assertRaisesRegex(
                    AccessControlValidationError,
                    f"current security read operation {operation_id}",
                ):
                    validate(contract, SOURCE)

    def test_security_write_slice_is_action_specific_and_instance_scoped(self) -> None:
        contract = payload()
        for operation_id, variant in CURRENT_SECURITY_WRITE_SLICE.items():
            with self.subTest(operation=operation_id):
                row = operation(contract, operation_id)
                self.assertEqual(row["status"], "current")
                self.assertEqual(row["source_variant"], variant)
                self.assertEqual(row["classification"], "fixed")
                self.assertEqual(row["required_all"], ["security.manage"])
                self.assertEqual(row["scope_resolution"], "instance")
                self.assertFalse(row["inherits_underlying"])

    def test_security_write_slice_cannot_collapse_back_to_ambiguous_families(self) -> None:
        contract = payload()
        contract["operations"].append(
            {
                "id": "security.principal_write",
                "status": "planned-1.2",
                "source_variant": None,
                "classification": "fixed",
                "required_all": ["security.manage"],
                "scope_resolution": "instance",
                "inherits_underlying": False,
                "allowed_roles": ["admin", "owner"],
            }
        )
        with self.assertRaisesRegex(
            AccessControlValidationError, "ambiguous security write family"
        ):
            validate(contract, SOURCE)

    def test_every_current_product_variant_has_a_matrix_rule(self) -> None:
        contract = payload()
        contract["operations"] = [
            row for row in contract["operations"] if row["source_variant"] != "Restore"
        ]
        with self.assertRaisesRegex(AccessControlValidationError, "ProductOperation matrix drift"):
            validate(contract, SOURCE)

    def test_new_product_variant_without_contract_fails_closed(self) -> None:
        source = SOURCE.read_text(encoding="utf-8")
        changed = source.replace(
            "pub enum ProductOperation {",
            "pub enum ProductOperation {\n    FuturePrivilegeBoundary,",
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "operation.rs"
            path.write_text(changed, encoding="utf-8")
            self.assertIn("FuturePrivilegeBoundary", product_operation_variants(path))
            with self.assertRaisesRegex(AccessControlValidationError, "ProductOperation matrix drift"):
                validate(payload(), path)

    def test_restore_never_inherits_backup_authority(self) -> None:
        contract = payload()
        row = operation(contract, "restore")
        row["required_all"] = ["backup.create"]
        row["allowed_roles"] = ["admin", "operator", "owner"]
        with self.assertRaisesRegex(AccessControlValidationError, "Restore permission rule"):
            validate(contract, SOURCE)

    def test_observation_never_inherits_maintenance(self) -> None:
        contract = payload()
        row = operation(contract, "admin.status")
        row["required_all"] = ["maintain"]
        row["allowed_roles"] = ["admin", "operator", "owner"]
        with self.assertRaisesRegex(AccessControlValidationError, "AdminStatus permission rule"):
            validate(contract, SOURCE)

    def test_owner_must_retain_complete_authority(self) -> None:
        contract = payload()
        role(contract, "owner")["permissions"].remove("ownership.manage")
        with self.assertRaisesRegex(AccessControlValidationError, "built-in role registry"):
            validate(contract, SOURCE)

    def test_operator_and_auditor_cannot_acquire_record_access(self) -> None:
        for role_id in ("operator", "auditor"):
            with self.subTest(role=role_id):
                contract = payload()
                permissions = role(contract, role_id)["permissions"]
                permissions.append("data.read")
                permissions.sort()
                with self.assertRaises(AccessControlValidationError):
                    validate(contract, SOURCE)

    def test_operation_role_matrix_is_derived_from_permissions(self) -> None:
        contract = payload()
        operation(contract, "backup.create")["allowed_roles"].append("writer")
        operation(contract, "backup.create")["allowed_roles"].sort()
        with self.assertRaisesRegex(AccessControlValidationError, "role matrix"):
            validate(contract, SOURCE)

    def test_proof_generation_cannot_bypass_wrapped_operation(self) -> None:
        contract = payload()
        operation(contract, "proof.generate")["inherits_underlying"] = False
        with self.assertRaisesRegex(AccessControlValidationError, "Prove permission rule"):
            validate(contract, SOURCE)

    def test_sql_read_dml_and_ddl_are_independent(self) -> None:
        contract = payload()
        operation(contract, "sql.execute_ddl")["required_all"] = ["data.write"]
        operation(contract, "sql.execute_ddl")["allowed_roles"] = [
            "admin",
            "developer",
            "owner",
            "writer",
        ]
        with self.assertRaisesRegex(AccessControlValidationError, "ExecuteSql must distinguish"):
            validate(contract, SOURCE)

    def test_sql_scope_policy_requires_binding_reuse_and_split_explain_authority(self) -> None:
        for field, value in (
            ("sql_authorization", "parsed-bound-object-set"),
            ("sql_binding_reuse", "rebind-before-execution"),
            ("sql_dml_dependencies", "target-only"),
            ("sql_explain_authorization", "instance"),
        ):
            with self.subTest(field=field):
                contract = payload()
                contract["scope_policy"][field] = value
                with self.assertRaisesRegex(
                    AccessControlValidationError, "binder-owned SQL authorization"
                ):
                    validate(contract, SOURCE)

    def test_sql_explain_and_staging_scope_resolvers_are_fail_closed(self) -> None:
        for operation_id, scope, message in (
            (
                "admin.explain_sql",
                "instance",
                "AdminExplainSql must split instance observe",
            ),
            (
                "transaction.stage_sql",
                "transaction_union",
                "TransactionStageSql must retain",
            ),
        ):
            with self.subTest(operation=operation_id):
                contract = payload()
                operation(contract, operation_id)["scope_resolution"] = scope
                with self.assertRaisesRegex(AccessControlValidationError, message):
                    validate(contract, SOURCE)

    def test_default_scalar_scope_requires_the_durable_binding_resolver(self) -> None:
        for operation_id in ("structure.get", "structure.set", "structure.ttl"):
            with self.subTest(operation=operation_id):
                contract = payload()
                operation(contract, operation_id)["scope_resolution"] = "instance"
                with self.assertRaisesRegex(
                    AccessControlValidationError, "durable default scalar keyspace"
                ):
                    validate(contract, SOURCE)

    def test_default_scalar_binding_policy_cannot_be_weakened(self) -> None:
        for field in ("default_scalar_authorization", "default_scalar_binding"):
            with self.subTest(field=field):
                contract = payload()
                contract["scope_policy"][field] = "instance"
                with self.assertRaisesRegex(
                    AccessControlValidationError, "durable default scalar binding"
                ):
                    validate(contract, SOURCE)

    def test_key_grammar_rejects_ambiguous_or_weaker_formats(self) -> None:
        for field, value in (
            ("secret_bits", 128),
            ("pattern", "^hyp1_.+$"),
            ("secret_return", "terminal"),
        ):
            with self.subTest(field=field):
                contract = payload()
                contract["key_format"][field] = value
                with self.assertRaisesRegex(AccessControlValidationError, "key format"):
                    validate(contract, SOURCE)

    def test_mutable_names_cannot_become_scope_authority(self) -> None:
        contract = payload()
        contract["scope_policy"]["name_is_authority"] = True
        with self.assertRaisesRegex(AccessControlValidationError, "mutable names"):
            validate(contract, SOURCE)

    def test_revocation_and_unknown_state_must_fail_closed(self) -> None:
        for field, value in (
            ("revocation_effective", "next-session"),
            ("unknown_state", "allow"),
            ("commit_reauthorization", False),
        ):
            with self.subTest(field=field):
                contract = payload()
                contract["authorization_policy"][field] = value
                with self.assertRaisesRegex(AccessControlValidationError, "authorization policy"):
                    validate(contract, SOURCE)

    def test_legacy_verifier_must_be_durable_keyed_and_downgrade_safe(self) -> None:
        for field, value in (
            ("catalog_format", "HYACAT04"),
            ("bearer_verifier_persisted", False),
            ("bearer_verifier", "bare-digest"),
            ("bearer_verifier_key", "process-local"),
            ("offline_bare_digest_authentication", True),
            ("enabled_state_requires_verifier", False),
            ("older_format_enabled_state", "accept"),
        ):
            with self.subTest(field=field):
                contract = copy.deepcopy(payload())
                contract["migration_policy"][field] = value
                with self.assertRaisesRegex(
                    AccessControlValidationError, "legacy bearer migration"
                ):
                    validate(contract, SOURCE)

    def test_recovery_cannot_bypass_integrity_or_keep_owner_keys(self) -> None:
        for field, value in (
            ("bypasses_integrity_validation", True),
            ("revokes_existing_owner_credentials", False),
        ):
            with self.subTest(field=field):
                contract = copy.deepcopy(payload())
                contract["recovery_policy"][field] = value
                with self.assertRaisesRegex(AccessControlValidationError, "owner recovery"):
                    validate(contract, SOURCE)

    def test_limits_cannot_be_removed_zeroed_or_made_unbounded(self) -> None:
        for field, value in (
            ("principals", 0),
            ("keys_per_principal", -1),
            ("authentication_verifiers_per_request", 2),
            ("authorization_cache_entries", True),
        ):
            with self.subTest(field=field):
                contract = copy.deepcopy(payload())
                contract["limits"][field] = value
                with self.assertRaisesRegex(AccessControlValidationError, "limits"):
                    validate(contract, SOURCE)


if __name__ == "__main__":
    unittest.main()
