#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import copy
import unittest

from tools.check_native_g7_matrix import GateFailure, validate_matrix
from tools.test_check_native_g7_receipt import TREE, interference_receipt, receipt


def matrix() -> dict:
    receipts = []
    for state in ("warm",):
        for background in ("control", "interference"):
            for concurrency in (1, 8, 32):
                value = interference_receipt() if background == "interference" else receipt()
                value["state"] = state
                value["concurrency"] = concurrency
                receipts.append(value)
    return {
        "schema": "hyphae-native-g7-matrix-v3",
        "gate": "G7",
        "status": "closure-candidate",
        "source_commit": "a" * 40,
        "platform": "linux",
        "states": ["warm"],
        "concurrency": [1, 8, 32],
        "background_modes": ["control", "interference"],
        "receipts": receipts,
        "claims": [],
        "closure_declared": False,
    }


class G7MatrixTests(unittest.TestCase):
    def test_complete_matrix(self) -> None:
        result = validate_matrix(matrix(), "a" * 40, expected_tree=TREE)
        self.assertEqual(result["receipts"], 6)

    def test_complete_darwin_matrix(self) -> None:
        payload = matrix()
        payload["platform"] = "darwin"
        for value in payload["receipts"]:
            value["platform"] = "darwin"
            value["build"]["target"] = "aarch64-apple-darwin"
        result = validate_matrix(payload, "a" * 40, expected_tree=TREE)
        self.assertEqual(result["platform"], "darwin")

    def test_missing_cell_fails(self) -> None:
        payload = matrix()
        payload["receipts"][0] = copy.deepcopy(payload["receipts"][0])
        payload["receipts"][0]["cells"].pop("hybrid-top10")
        with self.assertRaises(GateFailure):
            validate_matrix(payload, "a" * 40, expected_tree=TREE)

    def test_mixed_build_identity_fails(self) -> None:
        payload = matrix()
        payload["receipts"][0]["build"]["binary_sha256"] = "d" * 64
        with self.assertRaises(GateFailure):
            validate_matrix(payload, "a" * 40, expected_tree=TREE)

    def test_mixed_initial_ann_generation_fails(self) -> None:
        payload = matrix()
        payload["receipts"][0]["initial_ann_bulk"]["aggregate_identity"] = "e" * 64
        with self.assertRaisesRegex(GateFailure, "ANN generation"):
            validate_matrix(payload, "a" * 40, expected_tree=TREE)

    def test_matrix_rejects_a_different_authoritative_source_tree(self) -> None:
        with self.assertRaisesRegex(GateFailure, "source tree"):
            validate_matrix(matrix(), "a" * 40, expected_tree="e" * 40)


if __name__ == "__main__":
    unittest.main()
