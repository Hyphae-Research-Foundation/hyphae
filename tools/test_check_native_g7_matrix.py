#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

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
                commit_evidence = value["cells"]["strict-group-commit"][
                    "group_commit_evidence"
                ]
                commit_evidence["producer_concurrency"] = concurrency
                commit_evidence["maximum_active_producers"] = concurrency
                receipts.append(value)
    return {
        "schema": "hyphae-native-g7-matrix-v4",
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

    def test_legacy_v3_matrix_cannot_carry_v4_receipts(self) -> None:
        payload = matrix()
        payload["schema"] = "hyphae-native-g7-matrix-v3"
        with self.assertRaisesRegex(GateFailure, "identity or open state"):
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

    def test_mixed_recovered_group_commit_state_fails(self) -> None:
        payload = matrix()
        reopen = payload["receipts"][0]["cells"]["strict-group-commit"][
            "group_commit_evidence"
        ]["reopen"]
        reopen["expected_state_digest"] = "c" * 64
        reopen["recovered_state_digest"] = "c" * 64
        with self.assertRaisesRegex(GateFailure, "recovered group-commit state"):
            validate_matrix(payload, "a" * 40, expected_tree=TREE)

    def test_matrix_rejects_one_legacy_group_commit_evidence(self) -> None:
        payload = matrix()
        payload["receipts"][0]["cells"]["strict-group-commit"][
            "group_commit_evidence"
        ]["schema"] = "hyphae-native-g7-strict-group-commit-evidence-v1"
        with self.assertRaisesRegex(GateFailure, "strict group-commit configuration"):
            validate_matrix(payload, "a" * 40, expected_tree=TREE)

    def test_matrix_allows_distinct_commit_receipt_digests(self) -> None:
        payload = matrix()
        for index, value in enumerate(payload["receipts"], start=1):
            value["cells"]["strict-group-commit"]["group_commit_evidence"][
                "commit_receipt_digest"
            ] = f"{index:064x}"
        result = validate_matrix(payload, "a" * 40, expected_tree=TREE)
        self.assertEqual(result["receipts"], 6)

    def test_matrix_rejects_a_different_authoritative_source_tree(self) -> None:
        with self.assertRaisesRegex(GateFailure, "source tree"):
            validate_matrix(matrix(), "a" * 40, expected_tree="e" * 40)


if __name__ == "__main__":
    unittest.main()
