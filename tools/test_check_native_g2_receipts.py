#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import unittest

from tools.check_native_g2_receipts import GateFailure, validate_receipt


class NativeG2ReceiptTests(unittest.TestCase):
    def receipt(self) -> dict:
        return {
            "schema": "hyphae-native-g2-receipt-v1",
            "status": "passed",
            "source_commit": "a" * 40,
            "requirement": "tpcc-acid",
            "test_suites": ["tpcc_acid_g2", "tpcc_loader_g2"],
            "test_count": 6,
            "corpus_sha256": "b" * 64,
            "scope": "bounded-correctness",
            "production_scale": False,
        }

    def test_complete_content_bound_receipt_passes(self) -> None:
        result = validate_receipt(self.receipt(), "a" * 40, "tpcc-acid")
        self.assertEqual(result["status"], "passed")

    def test_wrong_commit_requirement_or_digest_fails_closed(self) -> None:
        for field, value, message in (
            ("source_commit", "c" * 40, "commit"),
            ("requirement", "tpch-correctness", "requirement"),
            ("corpus_sha256", "not-a-digest", "digest"),
        ):
            receipt = self.receipt()
            receipt[field] = value
            with self.assertRaisesRegex(GateFailure, message):
                validate_receipt(receipt, "a" * 40, "tpcc-acid")

    def test_empty_suites_zero_tests_or_claimed_production_fails_closed(self) -> None:
        for mutation, message in (
            (("test_suites", []), "suite"),
            (("test_count", 0), "test count"),
            (("production_scale", True), "production"),
        ):
            receipt = self.receipt()
            receipt[mutation[0]] = mutation[1]
            with self.assertRaisesRegex(GateFailure, message):
                validate_receipt(receipt, "a" * 40, "tpcc-acid")

    def test_unknown_fields_fail_closed(self) -> None:
        receipt = self.receipt()
        receipt["narrative_override"] = True
        with self.assertRaisesRegex(GateFailure, "fields"):
            validate_receipt(receipt, "a" * 40, "tpcc-acid")


if __name__ == "__main__":
    unittest.main()
