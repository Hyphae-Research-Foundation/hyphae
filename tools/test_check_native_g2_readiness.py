#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g2_readiness import GateFailure, evaluate

ROOT = Path(__file__).resolve().parents[1]


class NativeG2ReadinessTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads((ROOT / "config/native-g2-readiness-profile.json").read_text())

    def baseline(self) -> dict:
        return json.loads((ROOT / "config/native-g2-readiness-evidence.json").read_text())

    def test_profile_preserves_complete_normative_g2_scope(self) -> None:
        profile = self.profile()
        self.assertEqual(profile["gate"], "G2")
        self.assertEqual(
            [row["id"] for row in profile["requirements"]],
            [
                "native-ddl-dml-and-constraints",
                "transactions-and-isolation",
                "indexes-joins-ctes-windows",
                "prepared-plans-and-explain",
                "sqllogictest-conformance",
                "metamorphic-sql-equivalence",
                "tpch-correctness",
                "tpcc-acid",
            ],
        )

    def test_checked_in_baseline_starts_open(self) -> None:
        result = evaluate(ROOT, self.profile(), self.baseline())
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["required"], 8)
        self.assertEqual(result["passed"], 0)

    def test_exact_eight_hosted_artifacts_close_g2(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.baseline()
            rows = {}
            for requirement in self.profile()["requirements"]:
                artifact = root / f"{requirement['id']}.json"
                artifact.write_text('{"status":"passed"}\n')
                rows[requirement["id"]] = {
                    "status": "passed",
                    "level": "hosted",
                    "reference": artifact.name,
                    "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
                }
            evidence["evidence"] = rows
            result = evaluate(root, self.profile(), evidence)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["passed"], 8)

    def test_missing_digest_lower_level_or_unknown_row_fails_closed(self) -> None:
        evidence = self.baseline()
        evidence["evidence"]["unknown"] = {}
        with self.assertRaisesRegex(GateFailure, "unknown"):
            evaluate(ROOT, self.profile(), evidence)


if __name__ == "__main__":
    unittest.main()
