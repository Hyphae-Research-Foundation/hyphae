from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_quality_corpus import (
    GateFailure,
    validate_ann_receipt,
    validate_corpus,
    validate_lexical_receipt,
    validate_quality_receipt_set,
)


ROOT = Path(__file__).resolve().parents[1]


class NativeQualityCorpusTests(unittest.TestCase):
    def test_checked_in_corpora_bind_executable_producers_and_nonzero_scale(self) -> None:
        corpus = json.loads(
            (ROOT / "config/native-quality-corpus.json").read_text(encoding="utf-8")
        )

        result = validate_corpus(ROOT, corpus)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["corpus_count"], 2)
        self.assertEqual(result["engines"], ["ann", "lexical"])
        self.assertEqual(result["minimum_documents"], 10_512)
        self.assertEqual(result["minimum_queries"], 104)

    def test_duplicate_ids_missing_symbols_and_zero_scale_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = root / "producer.rs"
            producer.write_text("fn corpus_test() {}\n", encoding="utf-8")
            entry = {
                "id": "corpus",
                "engine": "lexical",
                "producer": "producer.rs",
                "test": "corpus_test",
                "minimum_documents": 1,
                "minimum_queries": 1,
                "metrics": ["score"],
            }
            corpus = {"schema": "hyphae-native-quality-corpus-v1", "corpora": [entry]}
            validate_corpus(root, corpus)

            corpus["corpora"].append(dict(entry))
            with self.assertRaisesRegex(GateFailure, "duplicate corpus"):
                validate_corpus(root, corpus)
            corpus["corpora"] = [dict(entry, test="missing")]
            with self.assertRaisesRegex(GateFailure, "test symbol"):
                validate_corpus(root, corpus)
            corpus["corpora"] = [dict(entry, minimum_queries=0)]
            with self.assertRaisesRegex(GateFailure, "positive scale"):
                validate_corpus(root, corpus)

    def test_unknown_fields_engines_metrics_and_path_traversal_fail_closed(self) -> None:
        entry = {
            "id": "corpus",
            "engine": "unknown",
            "producer": "../outside.rs",
            "test": "test",
            "minimum_documents": 1,
            "minimum_queries": 1,
            "metrics": [],
        }
        corpus = {"schema": "hyphae-native-quality-corpus-v1", "corpora": [entry]}
        with self.assertRaisesRegex(GateFailure, "unknown engine"):
            validate_corpus(ROOT, corpus)
        entry["engine"] = "lexical"
        with self.assertRaisesRegex(GateFailure, "metrics"):
            validate_corpus(ROOT, corpus)
        entry["metrics"] = ["score"]
        with self.assertRaisesRegex(GateFailure, "escapes repository root"):
            validate_corpus(ROOT, corpus)
        entry["producer"] = "missing.rs"
        entry["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown corpus field"):
            validate_corpus(ROOT, corpus)
    def test_checked_in_ann_receipt_is_valid_bounded_observation(self) -> None:
        receipt = json.loads(
            (
                ROOT
                / "docs/gates/evidence/native-ann-kernel-wsl2.json"
            ).read_text(encoding="utf-8")
        )

        result = validate_ann_receipt(receipt)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["evidence_scope"], "bounded-observation")
        self.assertEqual(result["vector_count"], 10_000)
        self.assertEqual(result["query_count"], 100)
        self.assertFalse(result["production_scale"])

    def test_ann_receipt_rejects_bad_recall_scale_digest_and_nonfinite_timings(self) -> None:
        receipt = json.loads(
            (
                ROOT
                / "docs/gates/evidence/native-ann-kernel-wsl2.json"
            ).read_text(encoding="utf-8")
        )
        receipt["recall_floor_met"] = False
        with self.assertRaisesRegex(GateFailure, "recall floor"):
            validate_ann_receipt(receipt)
        receipt["recall_floor_met"] = True
        receipt["recall_at_10"] = 0.94
        with self.assertRaisesRegex(GateFailure, "recall arithmetic"):
            validate_ann_receipt(receipt)
        receipt["recall_at_10"] = 0.97
        receipt["dataset_digest"] = "bad"
        with self.assertRaisesRegex(GateFailure, "digest"):
            validate_ann_receipt(receipt)
        receipt["dataset_digest"] = "d" * 64
        receipt["hnsw_latency_micros"]["p99"] = float("nan")
        with self.assertRaisesRegex(GateFailure, "finite latency"):
            validate_ann_receipt(receipt)
        receipt["hnsw_latency_micros"]["p99"] = 1.0
        receipt["vector_count"] = 0
        with self.assertRaisesRegex(GateFailure, "positive scale"):
            validate_ann_receipt(receipt)

    def test_ann_receipt_rejects_unknown_fields_and_bad_percentile_order(self) -> None:
        receipt = json.loads(
            (
                ROOT
                / "docs/gates/evidence/native-ann-kernel-wsl2.json"
            ).read_text(encoding="utf-8")
        )
        receipt["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown ANN receipt field"):
            validate_ann_receipt(receipt)
        receipt.pop("extra")
        receipt["exact_latency_micros"]["p50"] = receipt["exact_latency_micros"]["p95"] + 1
        with self.assertRaisesRegex(GateFailure, "percentile order"):
            validate_ann_receipt(receipt)
    def test_checked_in_lexical_receipt_is_source_bound_and_valid(self) -> None:
        receipt = json.loads(
            (
                ROOT / "docs/gates/evidence/native-lexical-quality-macos.json"
            ).read_text(encoding="utf-8")
        )
        result = validate_lexical_receipt(receipt)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["document_count"], 512)
        self.assertEqual(result["query_count"], 4)
        self.assertFalse(result["production_scale"])

    def test_lexical_receipt_requires_exact_order_and_reopen_equivalence(self) -> None:
        receipt = {
            "schema": "hyphae-native-lexical-quality-v1",
            "source_commit": "a" * 40,
            "dataset_digest": "b" * 64,
            "document_count": 512,
            "query_count": 4,
            "top_k": 25,
            "exact_score_order_equivalence": True,
            "reopen_equivalence": True,
            "query_result_digests": ["c" * 64, "d" * 64, "e" * 64, "f" * 64],
        }
        result = validate_lexical_receipt(receipt)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["evidence_scope"], "bounded-observation")
        self.assertFalse(result["production_scale"])

        receipt["reopen_equivalence"] = False
        with self.assertRaisesRegex(GateFailure, "equivalence"):
            validate_lexical_receipt(receipt)
        receipt["reopen_equivalence"] = True
        receipt["query_result_digests"].pop()
        with self.assertRaisesRegex(GateFailure, "query digest count"):
            validate_lexical_receipt(receipt)

    def test_lexical_receipt_rejects_unknown_fields_bad_identity_and_zero_scale(self) -> None:
        receipt = {
            "schema": "hyphae-native-lexical-quality-v1",
            "source_commit": "a" * 40,
            "dataset_digest": "b" * 64,
            "document_count": 512,
            "query_count": 1,
            "top_k": 25,
            "exact_score_order_equivalence": True,
            "reopen_equivalence": True,
            "query_result_digests": ["c" * 64],
        }
        receipt["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown lexical receipt field"):
            validate_lexical_receipt(receipt)
        receipt.pop("extra")
        receipt["dataset_digest"] = "bad"
        with self.assertRaisesRegex(GateFailure, "dataset digest"):
            validate_lexical_receipt(receipt)
        receipt["dataset_digest"] = "b" * 64
        receipt["document_count"] = 0
        with self.assertRaisesRegex(GateFailure, "positive scale"):
            validate_lexical_receipt(receipt)
    def test_quality_receipt_set_aggregates_exact_lexical_and_ann_evidence(self) -> None:
        lexical = json.loads(
            (ROOT / "docs/gates/evidence/native-lexical-quality-macos.json").read_text(
                encoding="utf-8"
            )
        )
        ann = json.loads(
            (ROOT / "docs/gates/evidence/native-ann-kernel-wsl2.json").read_text(
                encoding="utf-8"
            )
        )
        result = validate_quality_receipt_set(lexical, ann)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["evidence_scope"], "bounded-observation")
        self.assertEqual(result["engines"], ["ann", "lexical"])
        self.assertFalse(result["production_scale"])
        self.assertEqual(result["total_observations"], 104)

    def test_quality_receipt_set_never_promotes_one_bounded_engine(self) -> None:
        lexical = {
            "schema": "hyphae-native-lexical-quality-v1",
            "source_commit": "a" * 40,
            "dataset_digest": "b" * 64,
            "document_count": 1_000_000,
            "query_count": 1_000,
            "top_k": 25,
            "exact_score_order_equivalence": True,
            "reopen_equivalence": True,
            "query_result_digests": [f"{value:064x}" for value in range(1_000)],
        }
        ann = json.loads(
            (ROOT / "docs/gates/evidence/native-ann-kernel-wsl2.json").read_text(
                encoding="utf-8"
            )
        )
        result = validate_quality_receipt_set(lexical, ann)
        self.assertFalse(result["production_scale"])
        self.assertEqual(result["evidence_scope"], "bounded-observation")


if __name__ == "__main__":
    unittest.main()
