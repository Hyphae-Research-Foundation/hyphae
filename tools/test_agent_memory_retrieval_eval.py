#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Deterministic unit coverage for the Agent Memory retrieval eval."""

from __future__ import annotations

import copy
import unittest
from pathlib import Path

from tools.agent_memory_retrieval_eval import (
    DOMAINS,
    FIXTURE_SCHEMA,
    HARNESSES,
    EvalError,
    latency_summary,
    load_fixture,
    mrr_at_k,
    ndcg_at_k,
    nearest_rank,
    recall_at_k,
)

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "config" / "agent-memory-lexical-eval-v1.json"


class AgentMemoryRetrievalEvalTests(unittest.TestCase):
    def test_checked_fixture_covers_every_domain_harness_and_cell(self) -> None:
        fixture = load_fixture(FIXTURE)
        self.assertEqual(fixture["schema"], FIXTURE_SCHEMA)
        self.assertEqual(
            {document["domain"] for document in fixture["documents"]}, set(DOMAINS)
        )
        self.assertEqual(
            {document["harness"] for document in fixture["documents"]}, set(HARNESSES)
        )
        self.assertTrue(all(document["model"] for document in fixture["documents"]))

    def test_metrics_have_deterministic_endpoints(self) -> None:
        qrels = {1: 3, 2: 2, 3: 1}
        self.assertAlmostEqual(ndcg_at_k([1, 2, 3], qrels, 10), 1.0)
        self.assertAlmostEqual(recall_at_k([1, 9, 3], qrels, 3), 2 / 3)
        self.assertAlmostEqual(mrr_at_k([9, 2, 1], qrels, 10), 0.5)
        self.assertEqual(ndcg_at_k([9], qrels, 1), 0.0)

    def test_nearest_rank_and_latency_summary_are_exact(self) -> None:
        values = [10, 20, 30, 40, 50]
        self.assertEqual(nearest_rank(values, 50), 30)
        self.assertEqual(nearest_rank(values, 99), 50)
        summary = latency_summary(values)
        self.assertEqual(summary["samples"], 5)
        self.assertEqual(summary["p95_nanos"], 50)
        self.assertEqual(summary["total_nanos"], 150)

    def test_fixture_validation_rejects_missing_cell_query(self) -> None:
        fixture = load_fixture(FIXTURE)
        altered = copy.deepcopy(fixture)
        altered["queries"] = [
            query
            for query in altered["queries"]
            if query["segment"]["model"] != "unknown"
        ]
        self.assertRaises(EvalError, self._validate_object, altered)

    @staticmethod
    def _validate_object(fixture: dict) -> None:
        import json
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.json"
            path.write_text(json.dumps(fixture), encoding="utf-8")
            load_fixture(path)


if __name__ == "__main__":
    unittest.main()
