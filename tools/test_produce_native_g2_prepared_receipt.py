#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import unittest

from tools.produce_native_g2_prepared_receipt import GateFailure, build_receipt, parse_test_count


class NativeG2PreparedReceiptTests(unittest.TestCase):
    def test_parse_exact_cargo_test_count(self) -> None:
        output = "test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 338 filtered out"
        self.assertEqual(parse_test_count(output), 13)

    def test_failed_or_ambiguous_test_output_fails_closed(self) -> None:
        for output in (
            "test result: FAILED. 12 passed; 1 failed",
            "running 13 tests",
            "test result: ok. 0 passed; 0 failed",
        ):
            with self.assertRaises(GateFailure):
                parse_test_count(output)

    def test_receipt_binds_commit_suites_counts_and_digest(self) -> None:
        receipt = build_receipt(
            "a" * 40,
            [
                ("sql-unit", "test result: ok. 13 passed; 0 failed; 0 ignored"),
                ("local-sql-select", "test result: ok. 9 passed; 0 failed; 0 ignored"),
            ],
            "b" * 64,
        )
        self.assertEqual(receipt["status"], "passed")
        self.assertEqual(receipt["requirement"], "prepared-plans-and-explain")
        self.assertEqual(receipt["test_count"], 22)
        self.assertEqual(receipt["test_suites"], ["sql-unit", "local-sql-select"])
        self.assertFalse(receipt["production_scale"])

    def test_invalid_commit_or_digest_fails_closed(self) -> None:
        with self.assertRaisesRegex(GateFailure, "commit"):
            build_receipt("bad", [("suite", "test result: ok. 1 passed; 0 failed")], "b" * 64)
        with self.assertRaisesRegex(GateFailure, "digest"):
            build_receipt("a" * 40, [("suite", "test result: ok. 1 passed; 0 failed")], "bad")


if __name__ == "__main__":
    unittest.main()
