from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_quality_corpus import GateFailure, validate_corpus


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


if __name__ == "__main__":
    unittest.main()
