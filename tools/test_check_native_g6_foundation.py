#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import hashlib
import json
import unittest
from pathlib import Path

from tools.check_native_g6_foundation import GateFailure, REQUIREMENTS, validate, validate_suite_command

ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
FILES = {
    "profile": "native-g6-readiness-profile.json",
    "evidence": "native-g6-readiness-evidence.json",
    "inventory": "native-g6-inventory.json",
    "authority": "native-g6-authority-manifest.json",
    "workload": "native-g6-workload-manifest.json",
    "suite": "native-g6-suite-manifest.json",
    "predecessor": "native-g6-predecessor-manifest.json",
}


def load(name: str) -> dict:
    return json.loads((ROOT / "config" / name).read_text(encoding="utf-8"))


def payloads() -> list[dict]:
    return [
        load("native-g6-readiness-profile.json"),
        load("native-g6-readiness-evidence.json"),
        load("native-g6-inventory.json"),
        load("native-g6-authority-manifest.json"),
        load("native-g6-workload-manifest.json"),
        load("native-g6-suite-manifest.json"),
        load("native-g6-predecessor-manifest.json"),
    ]


def digests() -> dict[str, str]:
    return {
        name: hashlib.sha256((ROOT / "config" / filename).read_bytes()).hexdigest()
        for name, filename in FILES.items()
    }


def validate_changed(changed: list[dict]) -> dict:
    return validate(ROOT, *changed, COMMIT, digests())


class NativeG6FoundationTests(unittest.TestCase):
    def test_checked_in_foundation_is_complete_but_open(self) -> None:
        result = validate_changed(payloads())
        self.assertEqual(result["status"], "foundation-complete")
        self.assertEqual(result["requirements"], len(REQUIREMENTS))
        self.assertEqual(result["implemented_requirements"], 14)
        self.assertEqual(result["partial_requirements"], 0)
        self.assertEqual(result["planned_requirements"], 0)
        self.assertEqual(
            result["rust_toolchain"],
            {
                "channel": "1.96.0",
                "rustc": "rustc 1.96.0 (ac68faa20 2026-05-25)",
                "cargo": "cargo 1.96.0 (30a34c682 2026-05-25)",
            },
        )
        self.assertEqual(result["closure_status"], "open")
        self.assertFalse(result["closure_declared"])

    def test_contract_tamper_fails_closed(self) -> None:
        changed = payloads()
        changed[3]["contracts"][0]["sha256"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "mismatched G6 contract"):
            validate_changed(changed)

    def test_missing_contract_fails_closed(self) -> None:
        changed = payloads()
        del changed[3]["contracts"][0]
        with self.assertRaisesRegex(GateFailure, "authority set is incomplete"):
            validate_changed(changed)

    def test_predecessor_tamper_fails_closed(self) -> None:
        changed = payloads()
        changed[6]["predecessors"][3]["source_commit"] = "0" * 40
        with self.assertRaisesRegex(GateFailure, "differs from gate status"):
            validate_changed(changed)

    def test_evidence_or_claim_cannot_preclose_gate(self) -> None:
        changed = payloads()
        changed[1]["evidence"][REQUIREMENTS[0]] = {"status": "passed"}
        with self.assertRaisesRegex(GateFailure, "evidence must remain empty"):
            validate_changed(changed)
        changed = payloads()
        changed[0]["claims"] = ["G6 closed"]
        with self.assertRaisesRegex(GateFailure, "open and claim-free"):
            validate_changed(changed)

    def test_suite_rows_cannot_claim_implementation_without_suites(self) -> None:
        changed = payloads()
        changed[5]["requirements"][0]["suites"] = []
        with self.assertRaisesRegex(GateFailure, "invalid G6 implementation status"):
            validate_changed(changed)

    def test_workload_acceptance_drift_fails_closed(self) -> None:
        changed = payloads()
        changed[4]["workloads"][3]["acceptance"].remove("filter-aware-ann")
        with self.assertRaisesRegex(GateFailure, "workload acceptance mismatch"):
            validate_changed(changed)

    def test_required_surface_drift_fails_closed(self) -> None:
        changed = payloads()
        changed[0]["required_sdks"] = ["rust", "python"]
        with self.assertRaisesRegex(GateFailure, "product scope mismatch"):
            validate_changed(changed)

    def test_implemented_inventory_may_have_no_gaps(self) -> None:
        changed = payloads()
        changed[2]["requirements"][13]["status"] = "implemented-unhosted"
        changed[2]["requirements"][13]["gaps"] = []
        changed[5]["requirements"][13]["status"] = "implemented-unhosted"
        uncovered = changed[5]["requirements"][13]["uncovered_acceptance"]
        changed[5]["requirements"][13]["uncovered_acceptance"] = []
        changed[5]["requirements"][13]["suites"][0]["acceptance"].extend(uncovered)
        result = validate_changed(changed)
        self.assertEqual(result["implemented_requirements"], 14)
        self.assertEqual(result["partial_requirements"], 0)

    def test_partial_rows_name_exact_uncovered_acceptance(self) -> None:
        changed = payloads()
        row = changed[5]["requirements"][13]
        changed[2]["requirements"][13]["status"] = "partial"
        changed[2]["requirements"][13]["gaps"] = ["failure paths"]
        row["status"] = "partial-unhosted"
        row["uncovered_acceptance"] = ["failure-paths"]
        row["suites"][0]["acceptance"].remove("failure-paths")
        row["uncovered_acceptance"] = []
        with self.assertRaisesRegex(GateFailure, "acceptance coverage"):
            validate_changed(changed)

    def test_acceptance_overlap_fails_closed(self) -> None:
        changed = payloads()
        changed[5]["requirements"][0]["suites"][1]["acceptance"] = ["redaction"]
        with self.assertRaisesRegex(GateFailure, "acceptance coverage"):
            validate_changed(changed)
        changed = payloads()
        changed[5]["requirements"][0]["uncovered_acceptance"] = ["redaction"]
        with self.assertRaisesRegex(GateFailure, "acceptance coverage"):
            validate_changed(changed)

    def test_python_and_node_suite_commands_are_allowlisted(self) -> None:
        validate_suite_command(["python3", "-m", "unittest", "tools.test_example"])
        validate_suite_command(["python", "-m", "unittest", "tools.test_example"])
        validate_suite_command(["node", "--test", "sdk/typescript/example.test.js"])
        validate_suite_command(["npm", "test", "--prefix", "sdks/typescript"])

    def test_unsafe_or_unallowlisted_suite_commands_fail(self) -> None:
        for command in (["python3", "-c", "pass"], ["python3", "-m", "http.server"], ["node", "-e", "0"], ["sh", "test.sh"], ["node", "--test", "../escape.js"]):
            with self.subTest(command=command), self.assertRaises(GateFailure):
                validate_suite_command(command)


if __name__ == "__main__":
    unittest.main()
