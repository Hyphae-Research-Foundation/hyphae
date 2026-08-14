#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_gate_closures import (
    GateFailure,
    validate,
    validate_g7_c60_closure,
    validate_g8_aggregate,
)

ROOT = Path(__file__).resolve().parents[1]


class NativeGateClosureTests(unittest.TestCase):
    def test_checked_in_closure_prefix_is_complete_and_bound(self) -> None:
        result = validate(ROOT)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(
            result["closed"],
            ["G0", "G1", "G2", "G3", "G4", "G5", "G6", "G7", "G8"],
        )
        self.assertEqual(result["open"], [])

    def test_g7_c60_closure_rejects_authority_drift(self) -> None:
        closure_path = (
            ROOT
            / "docs/gates/evidence/closures/native-g7-ff188af.json"
        )
        closure = json.loads(closure_path.read_text(encoding="utf-8"))
        for path, value in (
            (("source_tree",), "0" * 40),
            (("contract_profile_sha256",), "0" * 64),
            (("retained_evidence", "sha256"), "0" * 64),
            (("production_scale",), False),
            (("authority_execution", "dedicated"), True),
            (("canonical_latency_certified",), True),
            (("dedicated_hardware_certified",), True),
            (("background_interference_certified",), True),
            (("claims",), []),
            (("closure_declared",), False),
        ):
            candidate = json.loads(json.dumps(closure))
            target = candidate
            for segment in path[:-1]:
                target = target[segment]
            target[path[-1]] = value
            with self.subTest(path=path):
                with self.assertRaises(GateFailure):
                    validate_g7_c60_closure(ROOT, candidate)

    def test_g8_closure_rejects_source_coverage_and_claim_drift(self) -> None:
        closure_path = (
            ROOT
            / "docs/gates/evidence/closures/native-g8-e88f2ea.json"
        )
        closure = json.loads(closure_path.read_text(encoding="utf-8"))
        for path, value in (
            (("source_commit",), "0" * 40),
            (("claims",), []),
            (("closure_declared",), False),
            (
                ("requirements", "native-soak", "linux", "receipt_sha256"),
                "0" * 64,
            ),
            (
                ("requirements", "native-soak", "linux", "audit", "status"),
                "failed",
            ),
        ):
            candidate = json.loads(json.dumps(closure))
            target = candidate
            for segment in path[:-1]:
                target = target[segment]
            target[path[-1]] = value
            with self.subTest(path=path):
                with self.assertRaises(GateFailure):
                    validate_g8_aggregate(ROOT, candidate)

    def fixture(self, directory: str) -> Path:
        root = Path(directory)
        (root / "config").mkdir()
        (root / "docs/gates/evidence/closures").mkdir(parents=True)
        source = "a" * 40
        reference = "docs/gates/evidence/closures/native-g0-aaaaaaa.json"
        closure = {
            "schema": "hyphae-native-g0-closure-v1",
            "gate": "G0",
            "status": "passed",
            "source_commit": source,
            "workflow_run": 1,
            "required": 1,
            "passed": 1,
            "requirements": ["contract"],
            "artifact": "artifact",
        }
        encoded = json.dumps(closure, sort_keys=True).encode()
        (root / reference).write_bytes(encoded)
        profile = {
            "gate": "G0",
            "requirements": [{"id": "contract", "required_evidence_level": "hosted"}],
        }
        (root / "config/native-g0-readiness-profile.json").write_text(json.dumps(profile))
        gates = [{
            "id": "G0",
            "status": "closed",
            "source_commit": source,
            "evidence": reference,
            "evidence_sha256": hashlib.sha256(encoded).hexdigest(),
        }] + [{"id": f"G{index}", "status": "open"} for index in range(1, 9)]
        status = {
            "schema": "hyphae-native-gate-status-v1",
            "program": "native-local-phase-1",
            "authority": "docs/gates/native-local-phase-1.md",
            "gates": gates,
        }
        (root / "config/native-gate-status.json").write_text(json.dumps(status))
        (root / "docs/README.md").write_text(
            "[G0](gates/evidence/closures/native-g0-aaaaaaa.json)"
        )
        (root / "docs/gates/native-gate-status.md").write_text(
            "[G0](evidence/closures/native-g0-aaaaaaa.json) `aaaaaaa`"
        )
        (root / "docs/gates/evidence/README.md").write_text(
            "[G0](closures/native-g0-aaaaaaa.json)"
        )
        return root

    def test_digest_tamper_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            status_path = root / "config/native-gate-status.json"
            status = json.loads(status_path.read_text())
            status["gates"][0]["evidence_sha256"] = "0" * 64
            status_path.write_text(json.dumps(status))
            with self.assertRaisesRegex(GateFailure, "digest mismatch"):
                validate(root)

    def test_noncontiguous_closure_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            status_path = root / "config/native-gate-status.json"
            status = json.loads(status_path.read_text())
            status["gates"][0] = {"id": "G0", "status": "open"}
            source = "b" * 40
            status["gates"][1] = {
                "id": "G1",
                "status": "closed",
                "source_commit": source,
                "evidence": "docs/gates/evidence/closures/native-g1-bbbbbbb.json",
                "evidence_sha256": "c" * 64,
            }
            status_path.write_text(json.dumps(status))
            with self.assertRaisesRegex(GateFailure, "after an open"):
                validate(root)

    def test_source_bound_filename_and_indexes_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            status_path = root / "config/native-gate-status.json"
            status = json.loads(status_path.read_text())
            status["gates"][0]["evidence"] = (
                "docs/gates/evidence/closures/native-g0-wrong.json"
            )
            status_path.write_text(json.dumps(status))
            with self.assertRaisesRegex(GateFailure, "not source-bound"):
                validate(root)


if __name__ == "__main__":
    unittest.main()
