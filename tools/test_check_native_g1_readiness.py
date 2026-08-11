#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g1_readiness import GateFailure, evaluate

ROOT = Path(__file__).resolve().parents[1]


class NativeG1ReadinessTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads((ROOT / "config/native-g1-readiness-profile.json").read_text())

    def baseline(self) -> dict:
        return json.loads((ROOT / "config/native-g1-readiness-evidence.json").read_text())

    def test_checked_in_profile_has_exact_seven_substrate_requirements(self) -> None:
        profile = self.profile()
        self.assertEqual(profile["schema"], "hyphae-native-g1-readiness-profile-v1")
        self.assertEqual(
            [row["id"] for row in profile["requirements"]],
            [
                "native-page-blob-wal-catalog-mvcc",
                "partitioned-memory-and-scheduler",
                "no-redb-on-native-target-path",
                "three-engine-minimal-vertical",
                "single-csn-all-engine-commit",
                "commit-checkpoint-crash-matrix",
                "embedded-and-local-protocol-latency",
            ],
        )

    def test_checked_in_baseline_remains_open(self) -> None:
        result = evaluate(ROOT, self.profile(), self.baseline())
        self.assertEqual(result["gate"], "G1")
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["required"], 7)
        self.assertEqual(result["passed"], 0)

    def test_exact_hosted_evidence_closes_g1(self) -> None:
        profile = self.profile()
        evidence = self.baseline()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = {}
            for requirement in profile["requirements"]:
                artifact = root / f"{requirement['id']}.json"
                artifact.write_text('{"status":"passed"}\n')
                import hashlib

                rows[requirement["id"]] = {
                    "status": "passed",
                    "level": requirement["required_evidence"],
                    "reference": artifact.name,
                    "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
                }
            evidence["evidence"] = rows
            result = evaluate(root, profile, evidence)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["passed"], 7)

    def test_hosted_workflow_produces_exact_sha_substrate_and_crash_receipts(self) -> None:
        workflow = (ROOT / ".github/workflows/native-g1.yml").read_text(encoding="utf-8")
        self.assertIn("cargo test -p hyphae-native-pages", workflow)
        self.assertIn("tools/check_native_g1_substrate.py", workflow)
        self.assertIn("--example process_crash_matrix", workflow)
        self.assertIn("tools/check_native_g1_crash_receipt.py", workflow)
        self.assertIn("github.event.pull_request.head.sha || github.sha", workflow)
        self.assertIn("native-g1-substrate-audit.json", workflow)
        self.assertIn("native-g1-crash-audit.json", workflow)
        self.assertIn("tools/check_native_g1_vertical_receipt.py", workflow)
        self.assertIn("native-g1-vertical-audit.json", workflow)
        self.assertIn("tools/check_native_g1_latency_receipts.py", workflow)
        self.assertIn("native-g1-latency-aggregate.json", workflow)
        self.assertIn("tools/assemble_native_g1_evidence.py", workflow)
        self.assertIn("native-g1-final-readiness.json", workflow)
        self.assertIn("dtolnay/rust-toolchain@4be7066ada62dd38de10e7b70166bc74ed198c30", workflow)
        self.assertIn('"passed": 7', workflow)

    def test_missing_digest_or_lower_level_fails_closed(self) -> None:
        profile = self.profile()
        evidence = self.baseline()
        evidence["evidence"][profile["requirements"][0]["id"]] = {
            "status": "passed",
            "level": "local",
            "reference": "missing.json",
        }
        with self.assertRaises(GateFailure):
            evaluate(ROOT, profile, evidence)


if __name__ == "__main__":
    unittest.main()
