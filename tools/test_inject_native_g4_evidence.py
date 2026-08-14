#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.inject_native_g4_evidence import GateFailure, inject


class InjectionTests(unittest.TestCase):
    def baseline(self):
        return {"schema": "hyphae-native-g4-readiness-evidence-v1", "gate": "G4", "evidence": {}, "claims": [], "closure_declared": False}

    def test_injects_content_bound_audit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            audit = root / "audit.json"
            audit.write_text(json.dumps({"schema": "hyphae-native-g4-receipt-audit-v1", "status": "passed", "source_commit": "a" * 40, "requirement": "ann-search", "claims": [], "closure_declared": False}))
            result = inject(root, self.baseline(), "ann-search", Path("audit.json"), "a" * 40)
            self.assertEqual(result["evidence"]["ann-search"]["artifact_sha256"], hashlib.sha256(audit.read_bytes()).hexdigest())

    def test_rejects_claim_and_wrong_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for commit, claims in (("b" * 40, []), ("a" * 40, ["complete"])):
                (root / "audit.json").write_text(json.dumps({"schema": "hyphae-native-g4-receipt-audit-v1", "status": "passed", "source_commit": commit, "requirement": "ann-search", "claims": claims, "closure_declared": False}))
                with self.subTest(commit=commit, claims=claims), self.assertRaises(GateFailure):
                    inject(root, self.baseline(), "ann-search", Path("audit.json"), "a" * 40)


if __name__ == "__main__":
    unittest.main()
