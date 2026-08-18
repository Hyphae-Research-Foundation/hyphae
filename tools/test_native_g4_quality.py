# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

from tools.native_g4_quality import QualityFailure, evaluate_corpus, ndcg_at_10


ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "config/native-g4-quality-corpus-v1.json"


class NativeG4QualityTests(unittest.TestCase):
    def test_checked_in_fixture_emits_structured_bounded_receipt(self) -> None:
        receipt = evaluate_corpus(CORPUS.read_bytes())
        self.assertEqual(receipt["schema"], "hyphae-native-g4-quality-receipt-v1")
        self.assertEqual(receipt["status"], "passed")
        self.assertEqual(receipt["scope"], "bounded")
        self.assertEqual(receipt["document_count"], 12)
        self.assertEqual(receipt["query_count"], 4)
        self.assertEqual(len(receipt["corpus_sha256"]), 64)
        self.assertGreaterEqual(receipt["lexical"]["mean_ndcg_ppm"], 850_000)
        self.assertGreaterEqual(receipt["hybrid"]["mean_ndcg_ppm"], 850_000)

    def test_integer_ndcg_has_exact_endpoints_and_floor_rounding(self) -> None:
        discounts = [1_000_000_000] * 10
        qrels = {"best": 3, "second": 1}
        self.assertEqual(ndcg_at_10(["best", "second"], qrels, discounts, 1_000_000), 1_000_000)
        self.assertEqual(ndcg_at_10(["noise"], qrels, discounts, 1_000_000), 0)
        self.assertEqual(ndcg_at_10(["second"], qrels, discounts, 1_000_000), 125_000)

    def test_duplicate_ranking_and_bad_qrels_fail_closed(self) -> None:
        discounts = [1_000_000_000] * 10
        with self.assertRaisesRegex(QualityFailure, "duplicate"):
            ndcg_at_10(["a", "a"], {"a": 1}, discounts, 1_000_000)
        with self.assertRaisesRegex(QualityFailure, "grades"):
            ndcg_at_10(["a"], {"a": -1}, discounts, 1_000_000)

    def test_negative_control_detects_relevance_leakage(self) -> None:
        corpus = json.loads(CORPUS.read_text(encoding="utf-8"))
        corpus["queries"][0]["negative"] = copy.deepcopy(corpus["queries"][0]["lexical"])
        with self.assertRaisesRegex(QualityFailure, "negative control"):
            evaluate_corpus(json.dumps(corpus).encode())

    def test_unknown_documents_and_schema_fields_fail_closed(self) -> None:
        corpus = json.loads(CORPUS.read_text(encoding="utf-8"))
        corpus["queries"][0]["hybrid"].append("unknown")
        with self.assertRaisesRegex(QualityFailure, "unknown document"):
            evaluate_corpus(json.dumps(corpus).encode())
        corpus = json.loads(CORPUS.read_text(encoding="utf-8"))
        corpus["extra"] = True
        with self.assertRaisesRegex(QualityFailure, "unknown corpus field"):
            evaluate_corpus(json.dumps(corpus).encode())

    def test_noninteger_metric_and_thresholds_fail_closed(self) -> None:
        corpus = json.loads(CORPUS.read_text(encoding="utf-8"))
        corpus["metric"]["scale"] = "1000000"
        with self.assertRaisesRegex(QualityFailure, "scale or discounts"):
            evaluate_corpus(json.dumps(corpus).encode())
        corpus = json.loads(CORPUS.read_text(encoding="utf-8"))
        corpus["thresholds"]["lexical_mean_ndcg_ppm"] = 1.5
        with self.assertRaisesRegex(QualityFailure, "integer metric units"):
            evaluate_corpus(json.dumps(corpus).encode())


if __name__ == "__main__":
    unittest.main()
