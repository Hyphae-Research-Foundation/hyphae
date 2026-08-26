#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Deterministic unit coverage for external memory benchmark adapters."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.long_term_memory_benchmarks import (
    BenchmarkError,
    binary_ndcg,
    execute_query,
    fuse_rrf_hits,
    locomo_document_text,
    locomo_query_metrics,
    longmemeval_query_metrics,
    normalize_locomo_evidence,
    official_longmemeval_ndcg,
    parse_locomo_timestamp,
    read_trace,
    run_queries,
)


class LongTermMemoryBenchmarkTests(unittest.TestCase):
    @staticmethod
    def trace_prepared() -> dict:
        return {
            "documents": [],
            "document_lookup": {
                1: "sample/D1:1",
                2: "sample/D1:2",
                3: "sample/D1:3",
            },
            "queries": [
                {
                    "id": "sample:0",
                    "sample_id": "sample",
                    "conversation_id": "sample",
                    "text": "SECRET QUESTION ONE",
                    "answer": "SECRET ANSWER ONE",
                    "segment": "1",
                    "targets": ["sample/D1:1", "sample/D1:1", "sample/D1:2"],
                },
                {
                    "id": "sample:1",
                    "sample_id": "sample",
                    "conversation_id": "sample",
                    "text": "SECRET QUESTION TWO",
                    "answer": "SECRET ANSWER TWO",
                    "segment": "2",
                    "targets": ["sample/D1:3"],
                },
            ],
            "protocol": {
                "document_granularity": "dialog-turn",
                "identity_scope": "sample_id/dialog_id",
            },
        }

    def test_locomo_evidence_normalizes_release_irregularities(self) -> None:
        self.assertEqual(normalize_locomo_evidence("D8:6; D9:17"), ["D8:6", "D9:17"])
        self.assertEqual(normalize_locomo_evidence("D:11:26"), ["D11:26"])
        self.assertEqual(normalize_locomo_evidence("D30:05"), ["D30:5"])
        self.assertEqual(normalize_locomo_evidence("D"), ["unresolved:D"])

    def test_locomo_enriched_view_keeps_one_anchor_identity(self) -> None:
        rendered = ["A said one", "B said two", "A said three"]
        text = locomo_document_text(rendered, 1, "3 pm on 1 May 2023", "timestamp-previous")
        self.assertIn("Session date and time: 3 pm on 1 May 2023", text)
        self.assertIn("Previous turn: A said one", text)
        self.assertIn("Evidence turn: B said two", text)
        self.assertNotIn("A said three", text)

    def test_binary_ndcg_has_exact_endpoints(self) -> None:
        self.assertAlmostEqual(binary_ndcg(["a", "b"], ["a", "b"], 10), 1.0)
        self.assertEqual(binary_ndcg(["x"], ["a"], 1), 0.0)
        self.assertGreater(binary_ndcg(["x", "a"], ["a"], 10), 0.0)

    def test_rrf_fusion_deduplicates_anchor_ids(self) -> None:
        fused = fuse_rrf_hits(
            [
                [{"object_id": 2}, {"object_id": 1}],
                [{"object_id": 1}, {"object_id": 3}],
            ],
            3,
        )
        self.assertEqual(fused, [1, 2, 3])

    def test_weighted_rrf_uses_fixed_point_branch_weights(self) -> None:
        fused = fuse_rrf_hits(
            [
                [{"object_id": 1}, {"object_id": 2}],
                [{"object_id": 2}, {"object_id": 1}],
            ],
            2,
            [1, 4],
        )
        self.assertEqual(fused, [2, 1])

    def test_locomo_timestamp_parses_to_canonical_micros(self) -> None:
        self.assertEqual(
            parse_locomo_timestamp("1:56 pm on 8 May, 2023"),
            parse_locomo_timestamp("1:56pm on 8 May, 2023"),
        )
        self.assertGreater(
            parse_locomo_timestamp("1:56 pm on 8 May, 2023"),
            parse_locomo_timestamp("9:00 am on 8 May, 2023"),
        )
        self.assertEqual(parse_locomo_timestamp("not a date"), 0)

    def test_session_cover_interleaves_distinct_sessions_deterministically(self) -> None:
        branch_a = [{"object_id": 1}, {"object_id": 2}, {"object_id": 3}]
        branch_b = [{"object_id": 4}, {"object_id": 5}]
        sessions_a = {1: "s1", 2: "s1", 3: "s2"}
        sessions_b = {4: "s1", 5: "s3"}
        plain = fuse_rrf_hits([branch_a, branch_b], 5)
        self.assertEqual(plain, [1, 4, 2, 5, 3])
        covered = fuse_rrf_hits(
            [branch_a, branch_b],
            5,
            session_values=[sessions_a, sessions_b],
            session_cover=2,
        )
        self.assertEqual(covered, [1, 5, 4, 2, 3])
        again = fuse_rrf_hits(
            [branch_a, branch_b],
            5,
            session_values=[sessions_a, sessions_b],
            session_cover=2,
        )
        self.assertEqual(covered, again)

    def test_session_cover_budgets_validate_and_single_session_is_untouched(self) -> None:
        branch = [{"object_id": 1}, {"object_id": 2}]
        with self.assertRaises(BenchmarkError):
            fuse_rrf_hits([branch], 2, session_cover=9)
        single = fuse_rrf_hits(
            [branch],
            2,
            session_values=[{1: "s1", 2: "s1"}],
            session_cover=2,
        )
        self.assertEqual(single, [1, 2])

    def test_slice_b_adds_native_doc_value_branches(self) -> None:
        calls: list[dict] = []

        class FakeResponse:
            value = {"hits": []}

        class FakeClient:
            def search_collection(self, collection, request):
                calls.append({"collection": collection, "request": request})
                hits = [
                    {"object_id": 1},
                    {"object_id": 2},
                ]
                if request.get("parent_dedupe"):
                    hits = [{"object_id": 2}]
                if request["filter"] != {"kind": "match_all"}:
                    hits = [{"object_id": 3}]
                response = FakeResponse()
                response.value = {"hits": hits}
                return response

        query = {
            "collections": [21],
            "text": "q",
            "rrf_weights": [1],
            "slice_b": {"session_window": [10, 20], "session_quota": 2},
        }
        ranking = execute_query(FakeClient(), query, 10, 100, slice_b=True)
        self.assertEqual(len(calls), 3)
        window = calls[1]["request"]
        quota = calls[2]["request"]
        self.assertEqual(window["filter"]["kind"], "all")
        self.assertEqual(window["filter"]["filters"][0]["field"], "session_ts")
        self.assertEqual(quota["parent_dedupe"], {"field": "session", "first_k": 2})
        self.assertIn(3, ranking)
        self.assertIn(2, ranking)
        plain = execute_query(FakeClient(), query, 10, 100, slice_b=False)
        self.assertEqual(len(calls), 4)
        self.assertEqual(plain, [1, 2])

    def test_longmemeval_metrics_match_official_boolean_recall(self) -> None:
        query = {
            "targets": ["answer_a", "answer_b"],
            "corpus_ids": ["answer_a", "filler", "answer_b"],
        }
        metrics = longmemeval_query_metrics(
            ["answer_a", "filler", "answer_b"], query, [1, 3]
        )
        self.assertEqual(metrics["recall_any@1"], 1.0)
        self.assertEqual(metrics["recall_all@1"], 0.0)
        self.assertEqual(metrics["recall_all@3"], 1.0)
        self.assertGreater(metrics["ndcg_any@3"], 0.8)
        self.assertLess(metrics["ndcg_any@3"], 1.0)

    def test_official_ndcg_uses_binary_relevance(self) -> None:
        perfect = official_longmemeval_ndcg(
            ["answer_a", "answer_b", "filler"],
            ["answer_a", "answer_b"],
            ["answer_a", "filler", "answer_b"],
            3,
        )
        delayed = official_longmemeval_ndcg(
            ["filler", "answer_a", "answer_b"],
            ["answer_a", "answer_b"],
            ["answer_a", "filler", "answer_b"],
            3,
        )
        self.assertAlmostEqual(perfect, 1.0)
        self.assertLess(delayed, perfect)

    def test_audited_qrels_deduplicate_while_raw_mode_is_compatible(self) -> None:
        query = {"targets": ["a", "a", "b"]}
        audited = locomo_query_metrics(["a"], query, [1], "audited-v2")
        raw = locomo_query_metrics(["a"], query, [1], "raw-compat")
        self.assertEqual(audited["evidence_recall@1"], 0.5)
        self.assertEqual(raw["evidence_recall@1"], 2 / 3)
        self.assertEqual(audited["recall_any@1"], raw["recall_any@1"])

    def test_longmemeval_audited_mode_deduplicates_qrels(self) -> None:
        longmemeval_query = {
            "targets": ["answer_a", "answer_a", "answer_b"],
            "corpus_ids": ["answer_a", "answer_a", "answer_b", "filler"],
        }
        audited_longmemeval = longmemeval_query_metrics(
            ["answer_a", "answer_b", "filler"], longmemeval_query, [3], "audited-v2"
        )
        raw_longmemeval = longmemeval_query_metrics(
            ["answer_a", "answer_b", "filler"], longmemeval_query, [3], "raw-compat"
        )
        self.assertEqual(audited_longmemeval["ndcg_any@3"], 1.0)
        self.assertLess(raw_longmemeval["ndcg_any@3"], 1.0)
        audited_duplicate_ranking = longmemeval_query_metrics(
            ["answer_a", "answer_a", "answer_b"],
            longmemeval_query,
            [3],
            "audited-v2",
        )
        self.assertLess(audited_duplicate_ranking["ndcg_any@3"], 1.0)

    def test_trace_resumes_without_duplicates_and_keeps_per_query_changes(self) -> None:
        prepared = self.trace_prepared()
        with tempfile.TemporaryDirectory() as directory:
            trace_path = Path(directory) / "queries.jsonl"
            with (
                patch(
                    "tools.long_term_memory_benchmarks.execute_query",
                    side_effect=[[1, 2], [1, 2], [2, 1]],
                ),
                patch(
                    "tools.long_term_memory_benchmarks.time.perf_counter_ns",
                    side_effect=[0, 10, 20, 30],
                ),
            ):
                first, first_correctness = run_queries(
                    object(), prepared, "locomo", 2, 1, trace_path, candidate_limit=5
                )
            with (
                patch(
                    "tools.long_term_memory_benchmarks.execute_query",
                    side_effect=[[3], [3], [3]],
                ),
                patch(
                    "tools.long_term_memory_benchmarks.time.perf_counter_ns",
                    side_effect=[40, 50, 60, 70],
                ),
            ):
                second, second_correctness = run_queries(
                    object(), prepared, "locomo", 2, None, trace_path,
                    start_after=1, candidate_limit=5,
                )

            self.assertEqual(first[0]["changed_rankings"], 1)
            self.assertEqual(second[0]["changed_rankings"], 0)
            self.assertEqual(first_correctness["changed_rankings_on_repeat"], 1)
            self.assertEqual(second_correctness["changed_rankings_on_repeat"], 0)
            with trace_path.open(encoding="utf-8") as handle:
                metadata, records = read_trace(handle, trace_path, prepared=prepared)
            self.assertIsNotNone(metadata)
            self.assertEqual([record["source_ordinal"] for record in records], [0, 1])
            self.assertEqual(records[0]["result"]["scored_targets"], [
                "sample/D1:1", "sample/D1:2"
            ])
            encoded = trace_path.read_text(encoding="utf-8")
            self.assertNotIn("SECRET QUESTION", encoded)
            self.assertNotIn("SECRET ANSWER", encoded)

            with self.assertRaisesRegex(BenchmarkError, "already contains source ordinal"):
                run_queries(
                    object(), prepared, "locomo", 2, None, trace_path,
                    start_after=1, candidate_limit=5,
                )
            with self.assertRaisesRegex(BenchmarkError, "protocol metadata differs"):
                run_queries(
                    object(), prepared, "locomo", 2, None, trace_path,
                    start_after=1, candidate_limit=5, qrel_mode="raw-compat",
                )

    def test_trace_validation_rejects_duplicate_existing_ordinals(self) -> None:
        prepared = self.trace_prepared()
        with tempfile.TemporaryDirectory() as directory:
            trace_path = Path(directory) / "queries.jsonl"
            with (
                patch(
                    "tools.long_term_memory_benchmarks.execute_query",
                    side_effect=[[1], [1]],
                ),
                patch(
                    "tools.long_term_memory_benchmarks.time.perf_counter_ns",
                    side_effect=[0, 10],
                ),
            ):
                run_queries(
                    object(), prepared, "locomo", 1, 1, trace_path, candidate_limit=5
                )
            records = trace_path.read_text(encoding="utf-8").splitlines()
            duplicate = json.loads(records[1])
            trace_path.write_text(
                "\n".join([*records, json.dumps(duplicate, sort_keys=True)]) + "\n",
                encoding="utf-8",
            )
            with trace_path.open(encoding="utf-8") as handle:
                with self.assertRaisesRegex(BenchmarkError, "duplicate source ordinals"):
                    read_trace(handle, trace_path, prepared=prepared)

    def test_trace_validation_rejects_duplicate_json_fields(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            trace_path = Path(directory) / "queries.jsonl"
            trace_path.write_text(
                '{"record_type":"metadata","record_type":"metadata"}\n',
                encoding="utf-8",
            )
            with trace_path.open(encoding="utf-8") as handle:
                with self.assertRaisesRegex(BenchmarkError, "invalid JSON"):
                    read_trace(handle, trace_path)

    def test_trace_validation_rejects_tampered_ranking_digest(self) -> None:
        prepared = self.trace_prepared()
        with tempfile.TemporaryDirectory() as directory:
            trace_path = Path(directory) / "queries.jsonl"
            with (
                patch(
                    "tools.long_term_memory_benchmarks.execute_query",
                    side_effect=[[1], [1]],
                ),
                patch(
                    "tools.long_term_memory_benchmarks.time.perf_counter_ns",
                    side_effect=[0, 10],
                ),
            ):
                run_queries(
                    object(), prepared, "locomo", 1, 1, trace_path, candidate_limit=5
                )
            records = trace_path.read_text(encoding="utf-8").splitlines()
            query_record = json.loads(records[1])
            query_record["result"]["logical_ranking"] = ["sample/D1:2"]
            trace_path.write_text(
                "\n".join([records[0], json.dumps(query_record, sort_keys=True)]) + "\n",
                encoding="utf-8",
            )
            with trace_path.open(encoding="utf-8") as handle:
                with self.assertRaisesRegex(BenchmarkError, "ranking digest differs"):
                    read_trace(handle, trace_path, prepared=prepared)


if __name__ == "__main__":
    unittest.main()
