#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
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


def payload() -> dict:
    return json.loads(CONTRACT.read_text(encoding="utf-8"))


def operation(contract: dict, operation_id: str) -> dict:
    return next(row for row in contract["operations"] if row["id"] == operation_id)


def role(contract: dict, role_id: str) -> dict:
    return next(row for row in contract["built_in_roles"] if row["id"] == role_id)


class NativeAccessControlContractTests(unittest.TestCase):
    def test_checked_in_contract_is_complete_and_honestly_pending(self) -> None:
        result = validate(payload(), SOURCE)
        self.assertEqual(result["status"], "contract-complete-implementation-pending")
        self.assertEqual(result["permissions"], 18)
        self.assertEqual(result["built_in_roles"], 7)
        self.assertEqual(result["current_product_variants"], 41)
        self.assertEqual(result["planned_operations"], 11)

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
