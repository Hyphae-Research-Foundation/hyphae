#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import unittest
from pathlib import Path

from tools.check_native_g6_foundation import GateFailure, REQUIREMENTS, validate

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
        self.assertEqual(result["implemented_requirements"], 0)
        self.assertEqual(result["partial_requirements"], 2)
        self.assertEqual(result["planned_requirements"], len(REQUIREMENTS) - 2)
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


if __name__ == "__main__":
    unittest.main()
