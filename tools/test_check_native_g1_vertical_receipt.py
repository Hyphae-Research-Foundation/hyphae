#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import unittest

from tools.check_native_g1_vertical_receipt import GateFailure, validate_receipt


class NativeG1VerticalReceiptTests(unittest.TestCase):
    def receipt(self) -> dict:
        return {
            "schema": "hyphae-native-g1-vertical-v1",
            "status": "passed",
            "source_commit": "a" * 40,
            "environment": "github-actions-ubuntu",
            "tests": {
                "sql-primary-key-insert-read": {"status": "passed", "test_count": 9},
                "structure-ttl-point-read": {"status": "passed", "test_count": 6},
                "lexical-match": {"status": "passed", "test_count": 6},
                "all-engine-single-csn": {"status": "passed", "test_count": 23},
            },
            "single_csn": 1,
            "engines": ["relational", "structure", "search"],
            "reopen_equivalent": True,
        }

    def test_exact_vertical_passes(self) -> None:
        result = validate_receipt(self.receipt(), "a" * 40)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["test_groups"], 4)
        self.assertEqual(result["engines"], 3)

    def test_missing_engine_or_test_group_fails_closed(self) -> None:
        receipt = self.receipt()
        receipt["engines"].pop()
        with self.assertRaisesRegex(GateFailure, "engine set"):
            validate_receipt(receipt, "a" * 40)
        receipt = self.receipt()
        receipt["tests"].pop("lexical-match")
        with self.assertRaisesRegex(GateFailure, "test group"):
            validate_receipt(receipt, "a" * 40)

    def test_failed_or_zero_test_group_fails_closed(self) -> None:
        receipt = self.receipt()
        receipt["tests"]["structure-ttl-point-read"]["status"] = "failed"
        with self.assertRaisesRegex(GateFailure, "did not pass"):
            validate_receipt(receipt, "a" * 40)
        receipt = self.receipt()
        receipt["tests"]["lexical-match"]["test_count"] = 0
        with self.assertRaisesRegex(GateFailure, "test count"):
            validate_receipt(receipt, "a" * 40)

    def test_wrong_commit_csn_or_reopen_fails_closed(self) -> None:
        with self.assertRaisesRegex(GateFailure, "source commit"):
            validate_receipt(self.receipt(), "b" * 40)
        receipt = self.receipt()
        receipt["single_csn"] = 2
        with self.assertRaisesRegex(GateFailure, "single CSN"):
            validate_receipt(receipt, "a" * 40)
        receipt = self.receipt()
        receipt["reopen_equivalent"] = False
        with self.assertRaisesRegex(GateFailure, "reopen"):
            validate_receipt(receipt, "a" * 40)


if __name__ == "__main__":
    unittest.main()
