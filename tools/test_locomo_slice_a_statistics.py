#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Deterministic unit coverage for LoCoMo Slice A statistics."""

from __future__ import annotations

import copy
import hashlib
import json
import math
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from tools.locomo_slice_a_statistics import (
    BOOTSTRAP_REPLICATES,
    BOOTSTRAP_SEED,
    MANIFEST_SCHEMA,
    LOCOMO_DATASET_SHA256,
    RESULT_SCHEMA,
    TRACE_DIGEST_CANONICALIZATION,
    TRACE_PROTOCOL_SCHEMA,
    TRACE_QUERY_SCHEMA,
    TRACE_SCHEMA,
    StatisticalEvaluationError,
    aggregate_metrics,
    build_candidate_manifest,
    canonical_digest,
    conversation_cluster_bootstrap,
    evaluate_manifest,
    exact_paired_sign_flip,
    holm_adjust,
    load_manifest,
    load_trace,
    select_one_standard_error,
)


ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "locomo_slice_a_statistics.py"
METRICS = [
    "evidence_recall@10",
    "mrr@50",
    "ndcg@10",
    "recall_all@10",
    "recall_any@10",
]


def metric_vector(value: float) -> dict[str, float]:
    return {
        "evidence_recall@10": value,
        "mrr@50": 1.0 if value > 0.0 else 0.0,
        "ndcg@10": value,
        "recall_all@10": 1.0 if value == 1.0 else 0.0,
        "recall_any@10": 1.0 if value > 0.0 else 0.0,
    }


def build_fixture(directory: Path, query_counts: list[int] | None = None) -> Path:
    counts = query_counts or [2] * 10
    conversations = []
    roster = []
    source_ordinal = 0
    for conversation_number, query_count in enumerate(counts):
        conversation_id = f"conversation-{conversation_number}"
        queries = []
        for query_number in range(query_count):
            query = {
                "id": f"{conversation_id}:{query_number}",
                "source_ordinal": source_ordinal,
                "segment": str(query_number % 5 + 1),
                "expected_targets": [f"{conversation_id}/target"],
            }
            queries.append(query)
            roster.append((conversation_id, query))
            source_ordinal += 1
        conversations.append({"id": conversation_id, "queries": queries})

    runs = {
        "baseline": [0.2] * 10,
        "simple": [0.7] * 10,
        "complex": [1.0, 1.0, 1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.5, 0.5],
    }
    for conversation_id, query in roster:
        query["expected_targets"] = [
            f"{conversation_id}/target-{index}" for index in range(10)
        ]
    configurations = {
        "baseline": {
            "document_text_views": ["bare"],
            "rrf_weights": [1],
            "analyzer_english_stop": False,
            "analyzer_english_stem": False,
            "bm25_k1_micros": None,
            "bm25_b_micros": None,
            "candidate_limit": 10,
            "qrel_mode": "audited-v2",
        },
        "simple": {
            "document_text_views": ["timestamp"],
            "rrf_weights": [1],
            "analyzer_english_stop": False,
            "analyzer_english_stem": False,
            "bm25_k1_micros": None,
            "bm25_b_micros": None,
            "candidate_limit": 10,
            "qrel_mode": "audited-v2",
        },
        "complex": {
            "document_text_views": ["timestamp", "centered"],
            "rrf_weights": [1, 1],
            "analyzer_english_stop": False,
            "analyzer_english_stem": False,
            "bm25_k1_micros": None,
            "bm25_b_micros": None,
            "candidate_limit": 10,
            "qrel_mode": "audited-v2",
        },
    }
    protocol_digests = {}
    for run_id, conversation_values in runs.items():
        protocol = {
            "schema": TRACE_PROTOCOL_SCHEMA,
            "benchmark": "locomo",
            "dataset_sha256": LOCOMO_DATASET_SHA256,
            "dataset_stats": {"samples": 10, "questions": len(roster)},
            "source_ordinal": "zero-based position in the pinned dataset query array",
            "query_population_total": len(roster),
            "query_population_evaluated": len(roster),
            "eligible_source_ordinals": [query["source_ordinal"] for _, query in roster],
            "candidate_limit": 10,
            "cutoffs": [10],
            "metric_names": METRICS,
            "warmup_per_query": 1,
            "timed_repetitions": 1,
            "logical_ranking_scope": "all returned hits up to candidate_limit",
            "ranking_order": "engine order",
            "ranking_identity": "sample_id/dialog_id",
            "ranking_digest": TRACE_DIGEST_CANONICALIZATION,
            "metric_contributions": "unrounded per-query values; aggregate is arithmetic mean",
            "qrel_mode": "audited-v2",
            "qrel_semantics": "stable first-occurrence deduplication before scoring",
            "latency_clock": "time.perf_counter_ns",
            "latency_unit": "nanoseconds",
            "latency_scope": "client end-to-end timed retrieval; warmup excluded",
            "omitted_dataset_fields": ["answer_text", "query_text", "document_text"],
            "benchmark_protocol": {
                "document_text_views": configurations[run_id]["document_text_views"],
                "rrf_weights": configurations[run_id]["rrf_weights"],
            },
            "execution_context": {
                "candidate": run_id,
                "analyzer_english_stop": False,
                "analyzer_english_stem": False,
                "bm25_k1_micros": None,
                "bm25_b_micros": None,
                "rrf_weights": configurations[run_id]["rrf_weights"],
            },
        }
        protocol_digest = canonical_digest(protocol)
        protocol_digests[run_id] = protocol_digest
        lines = []
        lines.append(
            json.dumps(
                {
                    "record_type": "metadata",
                    "schema": TRACE_SCHEMA,
                    "query_record_schema": TRACE_QUERY_SCHEMA,
                    "digest_canonicalization": TRACE_DIGEST_CANONICALIZATION,
                    "protocol": protocol,
                    "protocol_sha256": protocol_digest,
                },
                sort_keys=True,
            )
        )
        for conversation_id, query in roster:
            conversation_number = int(conversation_id.rsplit("-", 1)[1])
            value = conversation_values[conversation_number]
            targets = query["expected_targets"]
            hits = round(value * len(targets))
            ranking = (
                targets[:hits] + [f"{conversation_id}/distractor"]
                if hits < len(targets)
                else list(targets)
            )
            if value > 0.0:
                metrics = {
                    "evidence_recall@10": hits / len(targets),
                    "mrr@50": 1.0,
                    "ndcg@10": sum(
                        1 / math.log2(position + 2)
                        for position in range(hits)
                    )
                    / sum(
                        1 / math.log2(position + 2)
                        for position in range(10)
                    ),
                    "recall_all@10": 0.0,
                    "recall_any@10": 1.0,
                }
                if hits == len(targets):
                    metrics["recall_all@10"] = 1.0
            else:
                metrics = metric_vector(value)
            ranking_digest = canonical_digest(
                {"query_id": query["id"], "logical_ranking": ranking}
            )
            latency_value = 100 + query["source_ordinal"]
            latency_summary = {
                "samples": 1,
                "p50_nanos": latency_value,
                "p95_nanos": latency_value,
                "p99_nanos": latency_value,
                "p999_nanos": latency_value,
                "maximum_nanos": latency_value,
                "mean_nanos": float(latency_value),
                "total_nanos": latency_value,
                "queries_per_second": round(1_000_000_000 / latency_value, 3),
            }
            lines.append(
                json.dumps(
                    {
                        "record_type": "query",
                        "schema": TRACE_QUERY_SCHEMA,
                        "protocol_sha256": protocol_digest,
                        "source_ordinal": query["source_ordinal"],
                        "result": {
                            "id": query["id"],
                            "sample_id": conversation_id,
                            "conversation_id": conversation_id,
                            "segment": query["segment"],
                            "expected_targets": targets,
                            "scored_targets": targets,
                            "logical_ranking": ranking,
                            "metric_contributions": metrics,
                            "latency": {
                                "clock": "time.perf_counter_ns",
                                "unit": "nanoseconds",
                                "warmup_excluded": True,
                                "samples": [latency_value],
                                "summary": latency_summary,
                            },
                            "changed_rankings": 0,
                            "ranking_sha256": ranking_digest,
                        },
                    },
                    sort_keys=True,
                )
            )
        (directory / f"{run_id}.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")

    trace_digests = {
        run_id: hashlib.sha256((directory / f"{run_id}.jsonl").read_bytes()).hexdigest()
        for run_id in runs
    }
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "slice": "A",
        "primary_metric": "evidence_recall@10",
        "metric_names": METRICS,
        "conversations": conversations,
        "baseline": {
            "id": "baseline",
            "configuration": configurations["baseline"],
            "trace": "baseline.jsonl",
            "trace_sha256": trace_digests["baseline"],
            "trace_protocol_sha256": protocol_digests["baseline"],
        },
        "candidates": [
            {
                "id": "simple",
                "simplicity_rank": 0,
                "configuration": configurations["simple"],
                "trace": "simple.jsonl",
                "trace_sha256": trace_digests["simple"],
                "trace_protocol_sha256": protocol_digests["simple"],
            },
            {
                "id": "complex",
                "simplicity_rank": 1,
                "configuration": configurations["complex"],
                "trace": "complex.jsonl",
                "trace_sha256": trace_digests["complex"],
                "trace_protocol_sha256": protocol_digests["complex"],
            },
        ],
    }
    path = directory / "manifest.json"
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


class LoCoMoSliceAStatisticsTests(unittest.TestCase):
    def test_one_standard_error_chooses_simpler_eligible_candidate(self) -> None:
        selection = select_one_standard_error(
            {
                "simple": [0.70] * 9,
                "complex": [0.90, 0.90, 0.90, 0.90, 0.60, 0.60, 0.60, 0.60, 0.60],
            },
            {"simple": 0, "complex": 1},
        )
        self.assertEqual(selection["best_candidate_id"], "complex")
        self.assertIn("simple", selection["eligible_candidate_ids"])
        self.assertEqual(selection["selected_candidate_id"], "simple")

    def test_exact_sign_flip_enumerates_all_2_to_10_assignments(self) -> None:
        baseline = {f"c{index}": 0.2 for index in range(10)}
        candidate = {f"c{index}": 0.7 for index in range(10)}
        result = exact_paired_sign_flip(candidate, baseline)
        self.assertEqual(result["assignments"], 1024)
        self.assertEqual(result["extreme_assignments"], 2)
        self.assertEqual(result["p_value_fraction"], "1/512")
        self.assertEqual(result["p_value"], 2 / 1024)

    def test_holm_is_step_down_monotone_and_tie_deterministic(self) -> None:
        adjusted = holm_adjust({"z": 0.01, "a": 0.01, "middle": 0.03, "large": 0.5})
        self.assertEqual(list(adjusted), ["a", "large", "middle", "z"])
        self.assertEqual(adjusted["a"], 0.04)
        self.assertEqual(adjusted["z"], 0.04)
        self.assertEqual(adjusted["middle"], 0.06)
        self.assertEqual(adjusted["large"], 0.5)

    def test_cluster_bootstrap_is_seeded_and_preserves_cluster_weighting(self) -> None:
        candidate = {f"c{index}": [float(index), float(index)] for index in range(10)}
        baseline = {f"c{index}": [0.0, 0.0] for index in range(10)}
        first = conversation_cluster_bootstrap(candidate, baseline, seed=7, replicates=200)
        second = conversation_cluster_bootstrap(candidate, baseline, seed=7, replicates=200)
        different = conversation_cluster_bootstrap(candidate, baseline, seed=8, replicates=200)
        self.assertEqual(first, second)
        self.assertNotEqual(
            first["conversation_macro_difference"],
            different["conversation_macro_difference"],
        )
        self.assertEqual(first["conversation_macro_difference"]["estimate"], 4.5)
        self.assertEqual(first["question_micro_difference"]["estimate"], 4.5)

    def test_macro_and_micro_use_distinct_denominators(self) -> None:
        rows = []
        for index in range(10):
            count = 1 if index == 0 else 2
            for query in range(count):
                rows.append(
                    {
                        "conversation_id": f"c{index}",
                        "metrics": {"metric": 1.0 if index == 0 else 0.0},
                    }
                )
        aggregate = aggregate_metrics(rows, ["metric"])
        self.assertEqual(aggregate["conversation_macro"]["metric"], 0.1)
        self.assertEqual(aggregate["question_micro"]["metric"], round(1 / 19, 12))

    def test_end_to_end_nested_loco_emits_only_outer_fold_predictions(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            manifest_path = build_fixture(Path(raw_directory))
            receipt = evaluate_manifest(manifest_path)
        self.assertEqual(receipt["schema"], RESULT_SCHEMA)
        self.assertEqual(receipt["evidence_class"], "local-diagnostic")
        self.assertFalse(receipt["publication"]["authorized"])
        self.assertFalse(receipt["closure_declared"])
        self.assertEqual(len(receipt["results"]["outer_folds"]), 10)
        self.assertEqual(len(receipt["results"]["oof_predictions"]), 20)
        self.assertTrue(
            all(
                fold["selected_candidate_id"] == "simple"
                for fold in receipt["results"]["outer_folds"]
            )
        )
        self.assertTrue(
            all(
                prediction["selected_candidate_id"] == "simple"
                for prediction in receipt["results"]["oof_predictions"]
            )
        )
        comparison = receipt["results"]["comparisons_to_baseline"][
            "evidence_recall@10"
        ]
        self.assertEqual(comparison["exact_paired_sign_flip"]["assignments"], 1024)
        self.assertEqual(
            comparison["conversation_cluster_bootstrap"]["seed"], BOOTSTRAP_SEED
        )
        self.assertEqual(
            comparison["conversation_cluster_bootstrap"]["replicates"],
            BOOTSTRAP_REPLICATES,
        )
        self.assertEqual(
            comparison["conversation_cluster_bootstrap"][
                "conversation_macro_difference"
            ],
            {"estimate": 0.5, "lower": 0.5, "upper": 0.5},
        )

    def test_identity_digests_are_reproducible_and_bind_each_payload(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            manifest_path = build_fixture(Path(raw_directory))
            first = evaluate_manifest(manifest_path)
            second = evaluate_manifest(manifest_path)
        self.assertEqual(first, second)
        identity = first["identity"]
        self.assertEqual(identity["source_sha256"], canonical_digest(first["source"]))
        self.assertEqual(identity["protocol_sha256"], canonical_digest(first["protocol"]))
        self.assertEqual(identity["result_sha256"], canonical_digest(first["results"]))
        identity_keys = {
            key
            for section in (first["source"], first["protocol"], first["results"])
            for key in self._recursive_keys(section)
        }
        self.assertFalse(
            identity_keys.intersection(
                {"timestamp", "generated_at", "started_at", "finished_at", "completed_at"}
            )
        )

    @staticmethod
    def _recursive_keys(value: object) -> list[str]:
        if isinstance(value, dict):
            return [
                key
                for raw_key, child in value.items()
                for key in [raw_key, *LoCoMoSliceAStatisticsTests._recursive_keys(child)]
            ]
        if isinstance(value, list):
            return [
                key
                for child in value
                for key in LoCoMoSliceAStatisticsTests._recursive_keys(child)
            ]
        return []

    def test_cli_writes_the_same_deterministic_receipt_it_prints(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            output = directory / "receipt.json"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(TOOL),
                    "--manifest",
                    str(manifest_path),
                    "--output",
                    str(output),
                ],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(output.read_text(encoding="utf-8"), completed.stdout)
            self.assertEqual(json.loads(completed.stdout)["status"], "passed")

    def test_manifest_builder_uses_exact_trace_roster_and_simplicity_order(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            original = build_fixture(directory)
            expected = json.loads(original.read_text(encoding="utf-8"))
            built_path = directory / "built.json"
            built = build_candidate_manifest(
                directory / "baseline.jsonl",
                [directory / "simple.jsonl", directory / "complex.jsonl"],
                ["simple", "complex"],
                built_path,
            )
            self.assertEqual(built["conversations"], expected["conversations"])
            self.assertEqual(
                [candidate["simplicity_rank"] for candidate in built["candidates"]],
                [0, 1],
            )
            self.assertEqual(load_manifest(built_path), built)

    def test_manifest_rejects_wrong_grouping_and_timestamp_identity_fields(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            altered = copy.deepcopy(manifest)
            altered["conversations"][0]["queries"][0]["id"] = "conversation-1:0"
            manifest_path.write_text(json.dumps(altered), encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "belong exactly"):
                load_manifest(manifest_path)
            altered = copy.deepcopy(manifest)
            altered["candidates"][0]["configuration"]["generated_at"] = "now"
            manifest_path.write_text(json.dumps(altered), encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "timestamp identity"):
                load_manifest(manifest_path)
            altered = copy.deepcopy(manifest)
            altered["candidates"][0]["configuration"]["cache"] = "/tmp/cache"
            manifest_path.write_text(json.dumps(altered), encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "absolute path"):
                load_manifest(manifest_path)

    def test_evaluation_rejects_trace_not_bound_by_frozen_digest(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            trace = directory / "simple.jsonl"
            trace.write_text(trace.read_text(encoding="utf-8") + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "trace digest differs"):
                evaluate_manifest(manifest_path)

    def test_trace_requires_exact_roster_order_and_metric_contract(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            manifest = load_manifest(manifest_path)
            roster = [
                {**query, "conversation_id": conversation["id"]}
                for conversation in manifest["conversations"]
                for query in conversation["queries"]
            ]
            roster.sort(key=lambda query: query["source_ordinal"])
            trace = directory / "simple.jsonl"
            lines = trace.read_text(encoding="utf-8").splitlines()
            lines[1], lines[2] = lines[2], lines[1]
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "source ordinal"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

    def test_trace_recomputes_metrics_and_rejects_stale_protocol_digest(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            manifest = load_manifest(manifest_path)
            roster = [
                {**query, "conversation_id": conversation["id"]}
                for conversation in manifest["conversations"]
                for query in conversation["queries"]
            ]
            roster.sort(key=lambda query: query["source_ordinal"])
            trace = directory / "simple.jsonl"
            lines = trace.read_text(encoding="utf-8").splitlines()
            record = json.loads(lines[1])
            record["result"]["metric_contributions"]["ndcg@10"] -= 0.01
            lines[1] = json.dumps(record)
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "differ from ranking"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

            build_fixture(directory)
            lines = trace.read_text(encoding="utf-8").splitlines()
            metadata = json.loads(lines[0])
            metadata["protocol"]["candidate_limit"] = 11
            lines[0] = json.dumps(metadata)
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "stale digest"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

            build_fixture(directory)
            lines = trace.read_text(encoding="utf-8").splitlines()
            record = json.loads(lines[1])
            del record["result"]["metric_contributions"]["ndcg@10"]
            lines[1] = json.dumps(record)
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "exact metric contract"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

    def test_trace_rejects_metric_invariants_and_changed_rankings(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            manifest = load_manifest(manifest_path)
            roster = [
                {**query, "conversation_id": conversation["id"]}
                for conversation in manifest["conversations"]
                for query in conversation["queries"]
            ]
            roster.sort(key=lambda query: query["source_ordinal"])
            trace = directory / "simple.jsonl"
            lines = trace.read_text(encoding="utf-8").splitlines()
            record = json.loads(lines[1])
            record["result"]["metric_contributions"]["recall_any@10"] = 0.0
            lines[1] = json.dumps(record)
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "recall metrics"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

            build_fixture(directory)
            lines = trace.read_text(encoding="utf-8").splitlines()
            record = json.loads(lines[1])
            record["result"]["changed_rankings"] = 1
            lines[1] = json.dumps(record)
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "nondeterministic"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )

    def test_trace_json_rejects_duplicate_fields(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = Path(raw_directory)
            manifest_path = build_fixture(directory)
            manifest = load_manifest(manifest_path)
            roster = [
                {**query, "conversation_id": conversation["id"]}
                for conversation in manifest["conversations"]
                for query in conversation["queries"]
            ]
            roster.sort(key=lambda query: query["source_ordinal"])
            trace = directory / "duplicate.jsonl"
            original = (directory / "simple.jsonl").read_text(encoding="utf-8").splitlines()
            original[1] = original[1].replace(
                '"source_ordinal": 0', '"source_ordinal": 0, "source_ordinal": 0'
            )
            trace.write_text("\n".join(original) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(StatisticalEvaluationError, "duplicate JSON field"):
                load_trace(
                    trace,
                    roster,
                    METRICS,
                    "simple",
                    manifest["candidates"][0]["trace_protocol_sha256"],
                    manifest["candidates"][0]["configuration"],
                )


if __name__ == "__main__":
    unittest.main()
