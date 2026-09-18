# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_postgresql_application_core import (
    CONFIGURATION,
    CONTAINER_REFERENCE,
    EXECUTABLE_AUTHORITIES,
    GateFailure,
    REQUIRED_CASES,
    resolve_contained_file,
    validate_profile,
)


ROOT = Path(__file__).resolve().parents[1]
PROFILE = ROOT / "conformance/postgresql/application-core-v1.json"


class PostgreSQLApplicationCoreTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads(PROFILE.read_text(encoding="utf-8"))

    def test_checked_in_profile_is_exact_and_blocks_parity(self) -> None:
        profile = self.profile()
        result = validate_profile(ROOT, profile)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["case_count"], 11)
        self.assertEqual(result["mandatory_count"], 11)
        self.assertEqual(
            result["classification_counts"],
            {"conformant": 3, "partial": 2, "unsupported": 5, "unverified": 1},
        )
        self.assertEqual(result["open_mandatory_count"], 8)
        self.assertFalse(result["parity_eligible"])
        self.assertEqual(result["parity_status"], "blocked")
        self.assertFalse(result["generic_postgresql_compatible_permitted"])
        self.assertFalse(result["oracle_receipt_claim_authority"])
        self.assertEqual({case["id"] for case in profile["cases"]}, REQUIRED_CASES)
        typed = next(case for case in profile["cases"] if case["id"] == "typed-parameters")
        self.assertEqual(
            typed["hyphae"]["executable_authority"]["test_name"],
            "parameterized_insert_persists_declared_integer_text_and_boolean_types",
        )
        grouped = next(case for case in profile["cases"] if case["id"] == "group-by")
        self.assertEqual(
            grouped["hyphae"]["executable_authority"]["test_name"],
            "projected_group_key_and_count_are_explicitly_ordered",
        )
        joined = next(case for case in profile["cases"] if case["id"] == "basic-joins")
        self.assertEqual(
            joined["hyphae"]["executable_authority"]["test_name"],
            "unix::uds_sql_matches_physical_plans_recovers_failures_and_reopens",
        )

    def test_claim_is_forbidden_while_any_mandatory_cell_is_open(self) -> None:
        mutated = self.profile()
        mutated["profile"]["parity_claim"]["asserted"] = True
        with self.assertRaisesRegex(GateFailure, "claim is forbidden"):
            validate_profile(ROOT, mutated)

    def test_generic_postgresql_compatibility_is_permanently_forbidden(self) -> None:
        mutated = self.profile()
        mutated["profile"]["parity_claim"]["generic_postgresql_compatible_permitted"] = True
        with self.assertRaisesRegex(GateFailure, "parity claim is malformed"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["profile"]["parity_claim"]["exact_language"] = "PostgreSQL-compatible"
        with self.assertRaisesRegex(GateFailure, "parity claim is malformed"):
            validate_profile(ROOT, mutated)

    def test_opening_a_conformant_cell_revokes_its_authority(self) -> None:
        mutated = self.profile()
        typed = next(case for case in mutated["cases"] if case["id"] == "typed-parameters")
        typed["hyphae"]["classification"] = "unverified"
        typed["hyphae"]["executable_authority"] = None

        result = validate_profile(ROOT, mutated)
        self.assertFalse(result["parity_eligible"])
        self.assertIn("typed-parameters", result["open_mandatory_cases"])

    def test_textual_closure_and_arbitrary_existing_files_cannot_create_eligibility(self) -> None:
        baseline = self.profile()
        typed_authority = copy.deepcopy(EXECUTABLE_AUTHORITIES["typed-parameters"])
        open_cases = [
            case
            for case in baseline["cases"]
            if case["hyphae"]["classification"] != "conformant"
        ]
        self.assertEqual(len(open_cases), 8)
        for open_case in open_cases:
            with self.subTest(case=open_case["id"]):
                mutated = copy.deepcopy(baseline)
                cell = next(case for case in mutated["cases"] if case["id"] == open_case["id"])
                cell["hyphae"]["classification"] = "conformant"
                cell["hyphae"]["evidence"] = [
                    "crates/hyphae-native-runtime/tests/local_sql_select.rs"
                ]
                cell["hyphae"]["executable_authority"] = typed_authority
                with self.assertRaisesRegex(GateFailure, "case-specific executable"):
                    validate_profile(ROOT, mutated)

    def test_conformant_authority_is_exact_and_cannot_be_reused_or_edited(self) -> None:
        baseline = self.profile()
        typed = next(case for case in baseline["cases"] if case["id"] == "typed-parameters")

        mutated = copy.deepcopy(baseline)
        next(case for case in mutated["cases"] if case["id"] == "typed-parameters")["hyphae"][
            "executable_authority"
        ] = None
        with self.assertRaisesRegex(GateFailure, "case-specific executable"):
            validate_profile(ROOT, mutated)

        mutated = copy.deepcopy(baseline)
        authority = next(
            case for case in mutated["cases"] if case["id"] == "typed-parameters"
        )["hyphae"]["executable_authority"]
        authority["test_name"] = "prepare_codec_enforces_every_boundary"
        authority["command"][7] = "prepare_codec_enforces_every_boundary"
        with self.assertRaisesRegex(GateFailure, "case-specific executable"):
            validate_profile(ROOT, mutated)

        mutated = copy.deepcopy(baseline)
        identifiers = next(case for case in mutated["cases"] if case["id"] == "identifiers")
        identifiers["hyphae"]["executable_authority"] = copy.deepcopy(
            typed["hyphae"]["executable_authority"]
        )
        with self.assertRaisesRegex(GateFailure, "open case"):
            validate_profile(ROOT, mutated)

    def test_case_deletion_demotion_and_unknown_status_fail_closed(self) -> None:
        mutated = self.profile()
        mutated["cases"].pop()
        with self.assertRaisesRegex(GateFailure, "exact mandatory P0"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["mandatory"] = False
        with self.assertRaisesRegex(GateFailure, "mandatory P0"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["hyphae"]["classification"] = "waived"
        with self.assertRaisesRegex(GateFailure, "unknown classification"):
            validate_profile(ROOT, mutated)

    def test_pin_configuration_evidence_and_boundary_mutations_fail_closed(self) -> None:
        profile = self.profile()
        self.assertEqual(profile["postgresql"]["container"]["reference"], CONTAINER_REFERENCE)
        self.assertEqual(profile["postgresql"]["configuration"], CONFIGURATION)

        mutated = copy.deepcopy(profile)
        mutated["postgresql"]["container"]["reference"] = "postgres:18.6"
        with self.assertRaisesRegex(GateFailure, "reviewed manifest"):
            validate_profile(ROOT, mutated)

        mutated = copy.deepcopy(profile)
        mutated["postgresql"]["configuration"]["timezone"] = "localtime"
        with self.assertRaisesRegex(GateFailure, "UTF-8/C/UTC"):
            validate_profile(ROOT, mutated)

        mutated = copy.deepcopy(profile)
        mutated["cases"][0]["hyphae"]["evidence"] = []
        with self.assertRaisesRegex(GateFailure, "repository evidence"):
            validate_profile(ROOT, mutated)

        mutated = copy.deepcopy(profile)
        mutated["boundary"]["workspace_excluded"] = False
        with self.assertRaisesRegex(GateFailure, "external-only boundary"):
            validate_profile(ROOT, mutated)

    def test_oracle_and_evidence_traversal_fail_closed(self) -> None:
        mutated = self.profile()
        mutated["cases"][0]["oracle"] = (
            "conformance/postgresql/cases/../cases/identifiers.sql"
        )
        with self.assertRaisesRegex(GateFailure, "must not be absolute or contain"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["hyphae"]["evidence"][0] = (
            "docs/native/../native/catalog-v1.md"
        )
        with self.assertRaisesRegex(GateFailure, "must not be absolute or contain"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["oracle"] = str(
            (ROOT / "conformance/postgresql/cases/identifiers.sql").resolve()
        )
        with self.assertRaisesRegex(GateFailure, "must not be absolute"):
            validate_profile(ROOT, mutated)

    def test_case_requirements_and_oracles_cannot_be_swapped(self) -> None:
        mutated = self.profile()
        mutated["cases"][0]["requirement"] = mutated["cases"][1]["requirement"]
        with self.assertRaisesRegex(GateFailure, "requirement changed"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["oracle"], mutated["cases"][1]["oracle"] = (
            mutated["cases"][1]["oracle"],
            mutated["cases"][0]["oracle"],
        )
        with self.assertRaisesRegex(GateFailure, "oracle is not case-specific"):
            validate_profile(ROOT, mutated)

        mutated = self.profile()
        mutated["cases"][0]["oracle_sha256"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "oracle bytes differ"):
            validate_profile(ROOT, mutated)

    def test_symlink_escape_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "repository"
            allowed = root / "cases"
            allowed.mkdir(parents=True)
            outside = base / "outside.sql"
            outside.write_text("SELECT 1;", encoding="utf-8")
            (allowed / "escape.sql").symlink_to(outside)

            with self.assertRaisesRegex(GateFailure, "escapes its allowed directory"):
                resolve_contained_file(root, "cases/escape.sql", Path("cases"), "oracle")

            (root / "evidence.md").symlink_to(outside)
            with self.assertRaisesRegex(GateFailure, "escapes its allowed directory"):
                resolve_contained_file(root, "evidence.md", Path("."), "evidence")

    def test_external_harness_is_not_a_cargo_package_or_workspace_member(self) -> None:
        cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertNotIn('"conformance/postgresql"', cargo)
        self.assertFalse((ROOT / "conformance/postgresql/Cargo.toml").exists())
        validate_profile(ROOT, self.profile())

    def test_profile_runner_and_receipt_tests_are_wired_into_ci(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        for authority in [
            "tools.test_check_postgresql_application_core",
            "tools.test_check_postgresql_application_core_receipt",
            "tools.test_run_postgresql_application_core",
            "tools/check_postgresql_application_core.py",
        ]:
            self.assertIn(authority, workflow)


if __name__ == "__main__":
    unittest.main()
