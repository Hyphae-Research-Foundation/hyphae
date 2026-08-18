#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import unittest
from unittest.mock import patch

from tools.run_native_ann_durable_qualification import (
    CORPUS_IDENTITIES,
    METRICS,
    QualificationSuiteFailure,
    source_authority,
    validate_suite,
)
from tools.test_check_native_ann_durable_qualification import COMMIT, receipt


TREE = "b" * 40
CORPORA = {
    "squared-l2": "1" * 64,
    "cosine": "2" * 64,
    "negative-dot": "3" * 64,
}


def receipts() -> list[dict]:
    results = []
    for metric in METRICS:
        payload = receipt()
        payload["dataset"]["metric"] = metric
        payload["dataset"]["digest"] = CORPORA[metric]
        results.append(payload)
    return results


class NativeAnnDurableQualificationRunnerTests(unittest.TestCase):
    @patch(
        "tools.run_native_ann_durable_qualification._git",
        side_effect=[COMMIT, TREE, "?? untracked-generator.rs"],
    )
    def test_source_authority_rejects_untracked_generator_substitution(
        self, _git: object
    ) -> None:
        with self.assertRaisesRegex(QualificationSuiteFailure, "clean exact source"):
            source_authority(COMMIT)

    def test_frozen_corpus_identities_cover_every_metric(self) -> None:
        self.assertEqual(set(CORPUS_IDENTITIES), set(METRICS))
        for digest in CORPUS_IDENTITIES.values():
            self.assertRegex(digest, r"^[0-9a-f]{64}$")
            self.assertNotEqual(digest, "0" * 64)

    def test_accepts_exact_three_metric_local_suite_without_closure(self) -> None:
        audit = validate_suite(receipts(), COMMIT, TREE, CORPORA)
        self.assertEqual(audit["status"], "passed")
        self.assertEqual(audit["scope"], "local-durable-ann-smoke")
        self.assertEqual(audit["evidence_kind"], "correctness-qualification")
        self.assertEqual(audit["metrics"], list(METRICS))
        self.assertFalse(audit["closure_declared"])
        self.assertFalse(audit["g7_closure_declared"])

    def test_rejects_missing_metric_or_cross_tree_receipt(self) -> None:
        with self.assertRaisesRegex(QualificationSuiteFailure, "exact metric set"):
            validate_suite(receipts()[:-1], COMMIT, TREE, CORPORA)
        payloads = receipts()
        payloads[1]["source"]["tree"] = "4" * 40
        with self.assertRaisesRegex(QualificationSuiteFailure, "source tree"):
            validate_suite(payloads, COMMIT, TREE, CORPORA)

    def test_rejects_fallback_and_single_generation_consolidation(self) -> None:
        payloads = receipts()
        payloads[0]["quality"]["certified_selected_queries"] -= 1
        payloads[0]["quality"]["full_fanout_fallback_queries"] = 1
        payloads[0]["quality"]["maximum_searched_partitions"] = 16
        with self.assertRaisesRegex(QualificationSuiteFailure, "partition budget"):
            validate_suite(payloads, COMMIT, TREE, CORPORA)

        payloads = copy.deepcopy(receipts())
        consolidation = payloads[0]["lifecycle"]["consolidation"]
        consolidation["partitioned_base_preserved"] = False
        consolidation["routing_outcome_after"] = "single-generation-fallback"
        consolidation["total_partitions_after"] = 1
        with self.assertRaisesRegex(QualificationSuiteFailure, "partition"):
            validate_suite(payloads, COMMIT, TREE, CORPORA)

    def test_rejects_g7_sized_receipt_substitution(self) -> None:
        payloads = receipts()
        payloads[0]["dataset"]["vectors"] = 1_000_000
        payloads[0]["build"]["vector_count"] = 1_000_000
        with self.assertRaisesRegex(QualificationSuiteFailure, "local smoke bound"):
            validate_suite(payloads, COMMIT, TREE, CORPORA)


if __name__ == "__main__":
    unittest.main()
