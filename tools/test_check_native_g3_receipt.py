#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import unittest

from tools.check_native_g3_receipt import GateFailure, validate


class ReceiptAuditTests(unittest.TestCase):
    def receipt(self):
        return {
            "schema": "hyphae-native-g3-receipt-v2", "status": "passed",
            "source_commit": "a" * 40, "requirement": "streams",
            "manifest_sha256": "b" * 64, "platform": "linux-x86_64",
            "toolchain": "1.96.0", "suites": [
                {"name": "stream-model", "test_count": 2, "log_sha256": "c" * 64},
            ], "test_count": 2, "scope": "bounded-correctness", "production_scale": False,
        }

    def test_accepts_exact_identity(self):
        audit = validate(self.receipt(), "a" * 40, "streams", "b" * 64)
        self.assertEqual((audit["suite_count"], audit["test_count"]), (1, 2))

    def test_rejects_wrong_commit_or_count(self):
        with self.assertRaises(GateFailure):
            validate(self.receipt(), "d" * 40, "streams", "b" * 64)
        receipt = self.receipt()
        receipt["test_count"] = 3
        with self.assertRaises(GateFailure):
            validate(receipt, "a" * 40, "streams", "b" * 64)


if __name__ == "__main__":
    unittest.main()
