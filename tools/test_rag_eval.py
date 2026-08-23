#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Deterministic unit coverage for the RAG relevance harness metrics."""

from __future__ import annotations

import unittest

from tools.rag_eval import (
    DATASETS,
    MAX_DATASET_DOCUMENTS,
    RECEIPT_SCHEMA,
    mrr_at_k,
    ndcg_at_k,
    recall_at_k,
)


class RagEvalMetricTests(unittest.TestCase):
    def test_ndcg_is_one_for_a_perfect_graded_ranking(self) -> None:
        relevant = {"a": 3, "b": 2, "c": 1}
        self.assertAlmostEqual(ndcg_at_k(["a", "b", "c"], relevant, 10), 1.0)

    def test_ndcg_penalizes_late_relevance_deterministically(self) -> None:
        relevant = {"a": 1}
        first = ndcg_at_k(["a", "x", "y"], relevant, 10)
        third = ndcg_at_k(["x", "y", "a"], relevant, 10)
        self.assertAlmostEqual(first, 1.0)
        self.assertAlmostEqual(third, 1.0 / 2.0)
        self.assertEqual(ndcg_at_k(["x", "y"], relevant, 10), 0.0)

    def test_ndcg_respects_the_cutoff(self) -> None:
        relevant = {"a": 1}
        self.assertEqual(ndcg_at_k(["x", "a"], relevant, 1), 0.0)

    def test_ndcg_is_zero_without_judged_documents(self) -> None:
        self.assertEqual(ndcg_at_k(["x"], {}, 10), 0.0)

    def test_recall_counts_only_positively_judged_documents(self) -> None:
        relevant = {"a": 1, "b": 1, "c": 0}
        self.assertAlmostEqual(recall_at_k(["a", "x", "c"], relevant, 3), 0.5)
        self.assertAlmostEqual(recall_at_k(["a", "b"], relevant, 2), 1.0)
        self.assertEqual(recall_at_k(["x"], {"c": 0}, 5), 0.0)

    def test_mrr_uses_the_first_relevant_position(self) -> None:
        relevant = {"a": 1, "b": 2}
        self.assertAlmostEqual(mrr_at_k(["x", "b", "a"], relevant, 10), 0.5)
        self.assertEqual(mrr_at_k(["x", "y"], relevant, 2), 0.0)
        self.assertAlmostEqual(mrr_at_k(["a"], relevant, 1), 1.0)

    def test_frozen_dataset_inventory_stays_under_the_collection_cap(self) -> None:
        self.assertEqual(RECEIPT_SCHEMA, "hyphae-rag-relevance-receipt-v1")
        for name, entry in DATASETS.items():
            with self.subTest(dataset=name):
                self.assertLessEqual(entry["documents"], MAX_DATASET_DOCUMENTS)
                self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")


if __name__ == "__main__":
    unittest.main()
