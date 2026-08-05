#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g3_readiness import GateFailure, evaluate

ROOT = Path(__file__).resolve().parents[1]


class NativeG3ReadinessTests(unittest.TestCase):
    def profile(self):
        return json.loads((ROOT / "config/native-g3-readiness-profile.json").read_text())

    def baseline(self):
        return json.loads((ROOT / "config/native-g3-readiness-evidence.json").read_text())

    def test_checked_in_baseline_is_open_zero_of_eleven(self) -> None:
        result = evaluate(ROOT, self.profile(), self.baseline())
        self.assertEqual(result["status"], "failed")
        self.assertEqual((result["required"], result["passed"]), (11, 0))

    def test_eleven_content_bound_hosted_audits_close_g3(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            evidence = self.baseline()
            for row in self.profile()["requirements"]:
                identifier = row["id"]
                artifact = root / f"{identifier}.json"
                artifact.write_text(json.dumps({
                    "schema": "hyphae-native-g3-receipt-audit-v1",
                    "status": "passed",
                    "requirement": identifier,
                    "test_count": 1,
                    "scope": "bounded-correctness",
                    "production_scale": False,
                }))
                evidence["evidence"][identifier] = {
                    "status": "passed",
                    "level": "hosted",
                    "reference": artifact.name,
                    "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
                }
            result = evaluate(root, self.profile(), evidence)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["passed"], 11)

    def test_unknown_row_or_missing_artifact_fails_closed(self) -> None:
        evidence = self.baseline()
        evidence["evidence"]["invented"] = {}
        with self.assertRaises(GateFailure):
            evaluate(ROOT, self.profile(), evidence)


if __name__ == "__main__":
    unittest.main()
