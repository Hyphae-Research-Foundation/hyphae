#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g4_readiness import GateFailure, evaluate

ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
SUITE = "b" * 64
CORPUS = "c" * 64


class ReadinessTests(unittest.TestCase):
    def profile(self):
        return json.loads((ROOT / "config/native-g4-readiness-profile.json").read_text())

    def baseline(self):
        return json.loads((ROOT / "config/native-g4-readiness-evidence.json").read_text())

    def test_checked_in_baseline_is_open_zero_of_twelve(self):
        result = evaluate(ROOT, self.profile(), self.baseline(), COMMIT, SUITE, CORPUS)
        self.assertEqual((result["status"], result["required"], result["passed"]), ("not-ready", 12, 0))
        self.assertFalse(result["closure_declared"])

    def test_twelve_exact_sha_corpus_bound_audits_are_ready_without_closure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.baseline()
            for row in self.profile()["requirements"]:
                identifier = row["id"]
                artifact = root / f"{identifier}.json"
                artifact.write_text(json.dumps({"schema": "hyphae-native-g4-receipt-audit-v1", "status": "passed", "source_commit": COMMIT, "requirement": identifier, "suite_manifest_sha256": SUITE, "corpus_manifest_sha256": CORPUS, "corpora": ["fixture"], "suite_count": 1, "test_count": 1, "scope": "bounded-correctness", "production_scale": False, "claims": [], "closure_declared": False}))
                evidence["evidence"][identifier] = {"status": "passed", "level": "hosted", "reference": artifact.name, "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest()}
            result = evaluate(root, self.profile(), evidence, COMMIT, SUITE, CORPUS)
            self.assertEqual((result["status"], result["passed"], result["closure_declared"]), ("ready", 12, False))

    def test_wrong_digest_and_unknown_evidence_fail_closed(self):
        evidence = self.baseline()
        evidence["evidence"]["invented"] = {}
        with self.assertRaises(GateFailure):
            evaluate(ROOT, self.profile(), evidence, COMMIT, SUITE, CORPUS)
        with self.assertRaises(GateFailure):
            evaluate(ROOT, self.profile(), self.baseline(), COMMIT, "bad", CORPUS)


if __name__ == "__main__":
    unittest.main()
