#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import unittest

from tools.check_native_g4_receipt import GateFailure, validate


class ReceiptTests(unittest.TestCase):
    def receipt(self):
        return {"schema": "hyphae-native-g4-receipt-v1", "status": "passed", "source_commit": "a" * 40, "requirement": "ann-search", "suite_manifest_sha256": "b" * 64, "corpus_manifest_sha256": "c" * 64, "corpora": ["vectors"], "platform": "linux", "toolchain": "1.96.0", "suites": [{"name": "ann", "test_count": 2, "log_sha256": "d" * 64}], "test_count": 2, "scope": "bounded-correctness", "production_scale": False, "claims": [], "closure_declared": False}

    def test_accepts_exact_identity_without_claim(self):
        result = validate(self.receipt(), "a" * 40, "ann-search", "b" * 64, "c" * 64)
        self.assertEqual((result["suite_count"], result["closure_declared"]), (1, False))

    def test_rejects_wrong_identity_count_or_claim(self):
        for field, value in (("source_commit", "e" * 40), ("corpus_manifest_sha256", "f" * 64), ("test_count", 3), ("claims", ["G4 complete"]), ("closure_declared", True)):
            receipt = self.receipt()
            receipt[field] = value
            with self.subTest(field=field), self.assertRaises(GateFailure):
                validate(receipt, "a" * 40, "ann-search", "b" * 64, "c" * 64)


if __name__ == "__main__":
    unittest.main()
