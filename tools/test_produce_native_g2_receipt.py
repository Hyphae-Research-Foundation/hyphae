#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import unittest

from tools.produce_native_g2_receipt import GateFailure, build_receipt, parse_test_count


class NativeG2GenericReceiptTests(unittest.TestCase):
    def test_builds_any_known_requirement_from_exact_logs(self) -> None:
        receipt = build_receipt(
            "a" * 40,
            "tpcc-acid",
            [("tpcc-loader", "test result: ok. 1 passed; 0 failed; 0 ignored\n")],
            "b" * 64,
        )
        self.assertEqual(receipt["requirement"], "tpcc-acid")
        self.assertEqual(receipt["test_count"], 1)

    def test_unknown_requirement_and_ambiguous_logs_fail(self) -> None:
        with self.assertRaises(GateFailure):
            build_receipt(
                "a" * 40,
                "invented",
                [("suite", "test result: ok. 1 passed; 0 failed;\n")],
                "b" * 64,
            )
        self.assertEqual(
            parse_test_count(
                "test result: ok. 1 passed; 0 failed;\n"
                "test result: ok. 2 passed; 0 failed;\n"
            ),
            3,
        )
        with self.assertRaises(GateFailure):
            parse_test_count("test result: ok. 0 passed; 0 failed;\n")


if __name__ == "__main__":
    unittest.main()
