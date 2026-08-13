#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import copy
import hashlib
import json
import subprocess
import unittest
from pathlib import Path

from tools.check_native_g7_receipt import (
    GateFailure,
    resolve_expected_tree,
    validate as validate_receipt,
    validate_strict_group_commit_cell,
)


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
TREE = "d" * 40


def _object_id(value: int) -> str:
    return f"{value:032x}"


def _hybrid_oracle() -> dict:
    lexical_ranking = [_object_id(1)]
    vector_ranking = [_object_id(value) for value in range(1, 11)]
    fused_results = []
    for final_rank, object_id in enumerate(vector_ranking, start=1):
        lexical_rank = 1 if object_id == lexical_ranking[0] else None
        vector_rank = final_rank
        lexical_contribution = (
            1_000_000_000 // (60 + lexical_rank) if lexical_rank is not None else 0
        )
        vector_contribution = 1_000_000_000 // (60 + vector_rank)
        fused_results.append({
            "object_id": object_id,
            "lexical_rank": lexical_rank,
            "vector_rank": vector_rank,
            "lexical_contribution": lexical_contribution,
            "vector_contribution": vector_contribution,
            "fusion_score": lexical_contribution + vector_contribution,
            "final_rank": final_rank,
        })
    digest = hashlib.sha256(
        json.dumps(
            fused_results,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("ascii")
    ).hexdigest()
    return {
        "status": "passed",
        "method": "independent-branch-rrf-v1",
        "root_identity": "5" * 64,
        "snapshot_csn": 1,
        "rrf_constant": 60,
        "contribution_scale": 1_000_000_000,
        "lexical_weight": 1,
        "vector_weight": 1,
        "result_limit": 10,
        "tie_break": "fusion-score-desc-object-id-asc",
        "lexical_ranking": lexical_ranking,
        "vector_ranking": vector_ranking,
        "fused_results": fused_results,
        "result_digest": digest,
        "oracle_digest": digest,
    }


def _hybrid_evidence() -> dict:
    return {
        "per_query_worker_limit": 44,
        "query_queue_wait_millis": 60_000,
        "preferred_partition_budget": 32,
        "hybrid_read_view_open": {
            "root_identity": "5" * 64,
            "snapshot_csn": 1,
            "lexical_index_identity": "9" * 64,
            "ann_view_identity": "7" * 64,
            "lexical_plan_scope": "query-bound-encoded-postings-v1",
            "planned_physical_entries": 4,
            "planned_physical_bytes": 16_502,
            "observed_physical_entries": 4,
            "observed_physical_bytes": 200,
            "admitted_retained_memory_bytes": 1_048_576,
            "retained_memory_bytes": 1_000,
        },
        "hybrid_read_view_query_interval": {
            "observations": 1_000_000,
            "hydrations": 0,
            "physical_page_reads": 0,
            "index_scoped_restores": 0,
            "full_state_loads": 0,
            "full_catalog_loads": 0,
            "lexical_execution": "decode-bm25-rank-per-observation-v1",
            "peak_admission_executions": 1_000_000,
            "peak_admission_class": "foreground-bounded",
            "peak_admission_compute_threads": 44,
            "peak_admission_io_slots": 0,
            "peak_admission_memory_bytes_min": 2_000_000,
            "peak_admission_memory_bytes_max": 2_000_000,
            "result_retention_executions": 1_000_000,
            "result_retention_class": "foreground-bounded",
            "result_retention_compute_threads": 0,
            "result_retention_io_slots": 0,
            "result_retention_memory_bytes_min": 1_000_000,
            "result_retention_memory_bytes_max": 1_000_000,
            "fusion_executions": 1_000_000,
            "fusion_class": "foreground-bounded",
            "fusion_compute_threads": 1,
            "fusion_io_slots": 0,
            "fusion_memory_bytes": 0,
            "provider": "hybrid-read-view-interval-counters-v1",
        },
        "hybrid_ann_routing_interval": {
            "observations": 1_000_000,
            "selected_certified": 1_000_000,
            "full_fanout_requested": 0,
            "full_fanout_budget_fallback": 0,
            "single_generation_fallback": 0,
            "next_partition_lower_bound_present": 1_000_000,
            "selected_partitions_max": 8,
            "execution_workers_max": 8,
            "execution_worker_batches_max": 8,
            "execution_waves_max": 4,
            "minimum_next_partition_lower_bound": 0.25,
            "maximum_kth_distance": 0.20,
        },
        "hybrid_oracle": _hybrid_oracle(),
    }


def _lexical_read_view_open() -> dict:
    return {
        "root_identity": "5" * 64,
        "snapshot_csn": 1,
        "lexical_index_identity_algorithm": "blake3-search-root-page-object-format-v1",
        "lexical_index_identity": "9" * 64,
        "lexical_plan_scope": "query-bound-encoded-postings-v1",
        "index_id": _object_id(7),
        "planned_terms": 1,
        "retained_postings": 1,
        "maximum_retained_postings": 10,
        "maximum_retained_bytes": 1_048_576,
        "planned_physical_entries": 4,
        "planned_physical_bytes": 16_502,
        "observed_physical_entries": 4,
        "observed_physical_bytes": 200,
        "admitted_retained_memory_bytes": 1_048_576,
        "retained_memory_bytes": 1_000,
        "open_physical_page_reads": 1_000,
    }


def _bm25_evidence() -> dict:
    return {
        "route": "native-retained-lexical-read-view",
        "lexical_read_view_open": _lexical_read_view_open(),
        "lexical_read_view_query_interval": {
            "observations": 1_000_000,
            "postings_evaluated": 1_000_000,
            "execution_sequence_first": 100_001,
            "execution_sequence_last": 1_100_000,
            "receipt_physical_page_reads": 0,
            "process_physical_page_reads": 0,
            "full_state_loads": 0,
            "full_catalog_loads": 0,
            "lexical_execution": "decode-bm25-rank-per-observation-v1",
            "provider": "lexical-read-view-interval-counters-v1",
        },
    }


def _engine_work_evidence(*, memory_bytes: int) -> dict:
    return {
        "class": "foreground-bounded",
        "compute_threads": 1,
        "io_slots": 1,
        "memory_bytes": memory_bytes,
        "queue_ticket": None,
        "initial_queue_depth": 0,
        "queue_time_nanos": 0,
        "execution_time_nanos": 1,
    }


def _filtered_bm25_evidence() -> dict:
    return {
        "route": "native-root-bound-filter-before-rank",
        "correctness_scope": "lexical-and-structure-one-root-query-bound",
        "corpus_filter_density": 0.5,
        "candidate_filter_selectivity": 1.0,
        "filtered_lexical_read_view_open": {
            "root_identity": "5" * 64,
            "snapshot_csn": 1,
            "lexical_index_identity": "9" * 64,
            "lexical_plan_scope": "query-bound-encoded-postings-v1",
            "structure_filter_identity_algorithm": (
                "blake3-structure-root-key-prefix-value-time-v1"
            ),
            "structure_filter_value_scope": "inline-scalar-only-v1",
            "structure_filter_identity": "f" * 64,
            "retained_filter_records": 1,
            "planned_filter_physical_entries": 1,
            "planned_filter_physical_bytes": 256,
            "observed_filter_physical_entries": 1,
            "observed_filter_physical_bytes": 64,
            "retained_filter_memory_bytes": 128,
            "filter_planning": _engine_work_evidence(memory_bytes=1_024),
            "filter_hydration": _engine_work_evidence(memory_bytes=256),
            "open_filter_physical_page_reads": 1,
        },
        "filtered_lexical_read_view_query_interval": {
            "observations": 1_000_000,
            "execution_sequence_first": 100_001,
            "execution_sequence_last": 1_100_000,
            "postings_scored": 1_000_000,
            "filter_records_evaluated": 1_000_000,
            "filter_records_matched": 1_000_000,
            "receipt_physical_page_reads": 0,
            "process_physical_page_reads": 0,
            "full_state_loads": 0,
            "full_catalog_loads": 0,
            "filter_execution": "decode-expiry-inline-value-filter-before-rank-v1",
            "provider": "filtered-lexical-read-view-interval-counters-v1",
        },
    }


def _latency_summary(
    p50: int = 1,
    p95: int = 2,
    p99: int = 3,
    p999: int = 4,
    maximum: int = 5,
) -> dict:
    return {
        "p50": p50,
        "p95": p95,
        "p99": p99,
        "p999": p999,
        "maximum": maximum,
    }


def _strict_group_commit_evidence(
    observations: int = 1_000_000,
    concurrency: int = 1,
) -> dict:
    cohort_width = 32
    full_cohorts, remainder = divmod(observations, cohort_width)
    cohort_count = full_cohorts + int(remainder > 0)
    cohort_size_histogram = {str(cohort_width): full_cohorts}
    if remainder:
        cohort_size_histogram[str(remainder)] = 1
    return {
        "schema": "hyphae-native-g7-strict-group-commit-evidence-v1",
        "latency_scope": "scheduler-enqueue-through-durable-response-v1",
        "throughput_scope": "bounded-cohort-window-wall-time-v1",
        "submission_mode": "explicit-bounded-cohort-v1",
        "producer_concurrency": concurrency,
        "maximum_active_producers": concurrency,
        "cohort_width": cohort_width,
        "scheduler_queue_capacity": 64,
        "outstanding_limit": cohort_width,
        "maximum_outstanding": cohort_width,
        "logical_commits": observations,
        "cohort_count": cohort_count,
        "final_cohort_size": remainder or cohort_width,
        "cohort_size_histogram": cohort_size_histogram,
        "cohort_position_histogram": {
            str(position): full_cohorts + int(position < remainder)
            for position in range(cohort_width)
        },
        "first_commit_csn": 3,
        "last_commit_csn": observations + 2,
        "distinct_commit_csns": observations,
        "commit_receipt_digest_algorithm": (
            "blake3-csn-ordered-native-commit-receipts-v1"
        ),
        "commit_receipt_digest": "a" * 64,
        "page_synchronizations": cohort_count,
        "wal_synchronizations": cohort_count,
        "cohort_execution_nanos_total": cohort_count * 3,
        "page_synchronization_nanos_total": cohort_count,
        "wal_synchronization_nanos_total": cohort_count,
        "timing_sample_count": observations,
        "timings_nanoseconds": {
            "admission_wait": _latency_summary(0, 0, 0, 0, 0),
            "queue_wait": _latency_summary(),
            "cohort_execution": _latency_summary(),
            "page_synchronization": _latency_summary(),
            "wal_synchronization": _latency_summary(),
            "end_to_end": _latency_summary(),
        },
        "reopen": {
            "provider": "single-reopened-root-snapshot-full-key-digest-v1",
            "baseline_visible_csn": 2,
            "baseline_committed_transactions": 2,
            "reopened_visible_csn": observations + 2,
            "reopened_committed_transactions": observations + 2,
            "verified_logical_commits": observations,
            "missing_keys": 0,
            "mismatched_values": 0,
            "state_digest_algorithm": "blake3-logical-id-key-value-v1",
            "expected_state_digest": "b" * 64,
            "recovered_state_digest": "b" * 64,
            "open_time_nanos": 100,
            "verification_time_nanos": 1_000,
        },
    }


def validate(
    payload: dict,
    expected_commit: str,
    **kwargs: object,
) -> dict:
    kwargs.setdefault("expected_tree", TREE)
    return validate_receipt(payload, expected_commit, **kwargs)


def readiness_profile() -> dict:
    return json.loads(
        (ROOT / "config/native-g7-readiness-profile.json").read_text(encoding="utf-8")
    )


def receipt() -> dict:
    cell = {
        "status": "measured",
        "p50": 1,
        "p95": 2,
        "p99": 3,
        "p999": 4,
        "maximum": 5,
        "throughput_per_second": 1.0,
        "recall_at_10": 1.0,
        "materialization": {
            "full_state_loads": 0,
            "full_catalog_loads": 0,
            "provider": "process-interval-atomic-counters",
        },
    }
    cells = {
        name: dict(cell)
        for name in {
            "embedded-structure-point-get",
            "embedded-prepared-sql-primary-key",
            "local-structure-point-get",
            "local-prepared-sql-primary-key",
            "indexed-sql-bounded-read",
            "two-index-join-bounded-read",
            "bm25-top10",
            "filtered-bm25-top10",
            "ann-top10-recall-095",
            "hybrid-top10",
            "strict-group-commit",
        }
    }
    cells["ann-top10-recall-095"].update({
        "per_query_worker_limit": 44,
        "query_queue_wait_millis": 60_000,
        "preferred_partition_budget": 32,
        "ann_routing_interval": {
            "observations": 1_000_000,
            "execution_workers_max": 8,
            "execution_worker_batches_max": 32,
            "execution_waves_max": 1,
            "selected_certified": 1_000_000,
            "full_fanout_requested": 0,
            "full_fanout_budget_fallback": 0,
            "single_generation_fallback": 0,
            "next_partition_lower_bound_present": 1_000_000,
            "selected_partitions_max": 8,
            "minimum_next_partition_lower_bound": 0.25,
            "maximum_kth_distance": 0.20,
        },
        "post_open_hydration_performed": False,
        "post_open_physical_page_reads": 0,
        "post_open_restore_count": 0,
        "ann_read_view_query_interval": {
            "physical_page_reads": 0,
            "index_scoped_restores": 0,
            "provider": "database-page-counter-plus-process-ann-restore-counter",
        },
        "ann_read_view_open": {
            "root_identity": "5" * 64,
            "snapshot_csn": 1,
            "base_build_identity": "6" * 64,
            "view_identity": "7" * 64,
            "routing_policy_identity": "8" * 64,
            "logical_partitions": 64,
            "planned_physical_entries": 1_000_000,
            "planned_physical_bytes": 1_000_000_000,
            "observed_physical_entries": 1_000_000,
            "observed_physical_bytes": 1_000_000_000,
            "planned_peak_memory_bytes": 2_000_000_000,
            "retained_memory_bytes": 1_000_000_000,
            "hydration_restore_count": 1,
            "process_physical_page_read_delta": 1_000,
            "governor_generation": 1,
        },
    })
    cells["bm25-top10"].update(_bm25_evidence())
    cells["filtered-bm25-top10"].update(_filtered_bm25_evidence())
    cells["hybrid-top10"].update(_hybrid_evidence())
    cells["strict-group-commit"].update({
        "durability": "group-physical-sync",
        "group_commit_evidence": _strict_group_commit_evidence(),
    })
    return {
        "schema": "hyphae-native-g7-receipt-v4",
        "gate": "G7",
        "status": "passed",
        "evidence_class": "closure-candidate",
        "source_commit": "a" * 40,
        "platform": "linux",
        "state": "warm",
        "concurrency": 1,
        "background_mode": "control",
        "build": {
            "rustc": "rustc 1.96.0\nhost: x86_64-unknown-linux-gnu",
            "cargo": "cargo 1.96.0",
            "profile": "release",
            "target": "x86_64-unknown-linux-gnu",
            "os": "Linux-test",
            "binary_sha256": "c" * 64,
            "source_tree": TREE,
        },
        "dataset": {
            "observations": 1_000_000,
            "warmup": 100_000,
            "search_documents": 1_000_000,
            "vector_count": 1_000_000,
            "vector_dimension": 384,
            "generator": "deterministic-v2",
            "digest": "b" * 64,
        },
        "workload": {
            "structure_keys": 2_048,
            "sql_rows": 128,
            "point_value_bytes": 64,
            "search_documents": 1_000_000,
            "vector_count": 1_000_000,
            "vector_dimension": 384,
            "lexical_rare_documents": 1,
            "filtered_documents": 500_000,
            "result_limit": 10,
            "lexical_index_state": "committed-hot",
            "vector_index_state": "committed-hot",
        },
        "durability": {
            "read_seed": "memory-committed",
            "search_seed": "memory-committed",
            "commit_cell": "group-physical-sync",
        },
        "proofs_included": False,
        "correctness": {
            "cell_assertions": "passed",
            "ann_recall_floor": 0.95,
            "cross_engine_visibility": "native-same-snapshot-search",
        },
        "initial_ann_bulk": {
            "schema": "hyphae-native-g7-initial-ann-bulk-v1",
            "source_commit": "a" * 40,
            "dataset_digest": "b" * 64,
            "builder": "partitioned-hnsw-v1",
            "partition_policy": "g7-fixed-64-logical-partitions-v1",
            "input_identity": "1" * 64,
            "aggregate_identity": "2" * 64,
            "planned_vectors": 1_000_000,
            "planned_partitions": 64,
            "planned_workers": 44,
            "planned_memory_bytes": 4_000_000_000,
            "worker_batches": 48,
            "total_time_nanos": 1,
            "hardware_profile_fingerprint": "3" * 64,
            "governor_policy_schema": "hyphae-native-governor-policy-v1",
            "governor_mode": "mixed",
            "calibration_cache_key": "test-calibration",
            "topology_digest": "4" * 64,
            "topology_workers": 48,
            "hard_affinity": True,
            "governor_execution": {
                "class": "bulk",
                "compute_threads": 44,
                "io_slots": 0,
                "memory_bytes": 4_000_000_000,
                "queue_ticket": None,
                "initial_queue_depth": 0,
                "queue_time_nanos": 0,
                "execution_time_nanos": 1,
            },
        },
        "execution_authority": {
            "status": "measured",
            "topology_digest": "4" * 64,
            "runner_executable_blake3": "e" * 64,
            "calibration_executable_blake3": "e" * 64,
            "installations": 11,
            "installed_surfaces": sorted({
                "search-fixture", "embedded-structure", "embedded-sql",
                "local-structure-seed", "local-structure-migration",
                "local-structure-daemon", "local-sql-daemon", "indexed-sql",
                "join-sql", "group-commit", "physical-observation",
            }),
            "registered_pools": 1,
            "local_dispatches": 9,
            "stolen_dispatches": 0,
            "completed_jobs": 9,
            "numa_steal_status": "disabled",
        },
        "hardware": {
            "dedicated": True,
            "cpu": "test-cpu",
            "topology": "1 socket",
            "ram_bytes": 64 * 1024**3,
            "storage": "test-nvme",
            "filesystem": "ext4",
            "governor": "performance",
            "affinity": "0-31",
            "priority": "realtime",
            "background_services": "disabled",
            "virtualization": "none",
        },
        "cells": cells,
        "counters": {
            name: {
                "status": "measured",
                "value": 1,
                "unit": "count",
                "provider": "test-provider",
            }
            for name in (
                "allocations", "rss", "cpu_cycles", "cache_misses",
                "page_faults", "bytes_read", "bytes_written",
            )
        },
        "saturation": {
            "status": "measured",
            "levels": [1, 8, 32],
            "method": "executed-concurrency-sweep",
            "throughput_per_second": {
                name: {"1": 1.0, "8": 2.0, "32": 3.0}
                for name in {
                    "embedded-structure-point-get", "embedded-prepared-sql-primary-key",
                    "local-structure-point-get", "local-prepared-sql-primary-key",
                    "indexed-sql-bounded-read", "two-index-join-bounded-read", "bm25-top10",
                    "filtered-bm25-top10", "ann-top10-recall-095", "hybrid-top10",
                    "strict-group-commit",
                }
            },
        },
        "background_interference": {"status": "control"},
        "claims": [],
        "closure_declared": False,
        "physical_observation": {
            "page_count": 1,
            "physical_page_reads": 1,
            "wal_bytes": 1,
            "process_full_state_loads": 0,
            "process_full_catalog_loads": 0,
        },
    }


def interference_receipt() -> dict:
    payload = receipt()
    payload["background_mode"] = "interference"
    payload["execution_authority"]["installed_surfaces"].append(
        "background-maintenance"
    )
    payload["execution_authority"]["installed_surfaces"].sort()
    payload["execution_authority"]["installations"] += 1
    payload["background_interference"] = {
        "status": "measured",
        "operations": 1,
        "p99_ratio_by_cell": {name: 1.0 for name in payload["cells"]},
    }
    return payload


class G7ReceiptTests(unittest.TestCase):
    def test_valid_receipt(self) -> None:
        result = validate(receipt(), "a" * 40)
        self.assertEqual(result["status"], "passed")

    def test_build_source_tree_must_equal_the_expected_tree_not_commit(self) -> None:
        self.assertNotEqual(COMMIT, TREE)
        self.assertEqual(validate(receipt(), COMMIT)["status"], "passed")
        payload = receipt()
        payload["build"]["source_tree"] = "e" * 40
        with self.assertRaisesRegex(GateFailure, "source tree"):
            validate(payload, COMMIT)

    def test_source_tree_resolver_uses_the_commit_tree_object(self) -> None:
        commit = subprocess.run(
            ("git", "rev-parse", "HEAD"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        tree = subprocess.run(
            ("git", "rev-parse", "HEAD^{tree}"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        self.assertNotEqual(commit, tree)
        self.assertEqual(resolve_expected_tree(commit, repository=ROOT), tree)

    def test_dataset_requires_exact_closure_observations_and_warmup(self) -> None:
        for field, value in (
            ("observations", 999_999),
            ("observations", 1_000_001),
            ("warmup", 99_999),
            ("warmup", 100_001),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["dataset"][field] = value
                with self.assertRaisesRegex(GateFailure, "measurement counts"):
                    validate(payload, "a" * 40)

    def test_latency_targets_are_derived_from_the_profile_authority(self) -> None:
        profile = readiness_profile()
        profile["warm_targets_nanoseconds"]["bm25-top10"]["p99"] = 499_999
        payload = receipt()
        payload["cells"]["bm25-top10"]["p99"] = 500_000
        with self.assertRaisesRegex(GateFailure, "latency target"):
            validate(payload, "a" * 40, profile=profile)

    def test_ann_cell_requires_durable_read_view_and_worker_budget(self) -> None:
        payload = receipt()
        del payload["cells"]["ann-top10-recall-095"]["ann_read_view_open"]
        with self.assertRaisesRegex(GateFailure, "read-view open receipt"):
            validate(payload, "a" * 40)

        payload = receipt()
        payload["cells"]["ann-top10-recall-095"]["per_query_worker_limit"] = 0
        with self.assertRaisesRegex(GateFailure, "per-query worker limit"):
            validate(payload, "a" * 40)

    def test_ann_cell_rejects_any_post_open_storage_or_restore_work(self) -> None:
        for field, value in (
            ("post_open_hydration_performed", True),
            ("post_open_physical_page_reads", 1),
            ("post_open_restore_count", 1),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["ann-top10-recall-095"][field] = value
                with self.assertRaisesRegex(GateFailure, "after read-view open"):
                    validate(payload, "a" * 40)

    def test_ann_cell_rejects_fallback_full_fanout_and_incomplete_aggregation(self) -> None:
        for field, value in (
            ("full_fanout_budget_fallback", 1),
            ("full_fanout_requested", 1),
            ("selected_certified", 999_999),
            ("selected_partitions_max", 33),
            ("execution_waves_max", 7),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["ann-top10-recall-095"]["ann_routing_interval"][field] = value
                with self.assertRaisesRegex(GateFailure, "selected-certified"):
                    validate(payload, "a" * 40)

    def test_ann_adaptive_prefix_accepts_multiple_bounded_waves(self) -> None:
        payload = receipt()
        payload["cells"]["ann-top10-recall-095"]["ann_routing_interval"][
            "execution_waves_max"
        ] = 4
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_ann_routing_requires_finite_strict_omission_bound(self) -> None:
        for field, value in (
            ("minimum_next_partition_lower_bound", float("nan")),
            ("minimum_next_partition_lower_bound", float("inf")),
            ("minimum_next_partition_lower_bound", 0.20),
            ("maximum_kth_distance", float("nan")),
            ("maximum_kth_distance", float("inf")),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["ann-top10-recall-095"]["ann_routing_interval"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "ANN routing.*bound"):
                    validate(payload, "a" * 40)

    def test_ann_cell_rejects_claimed_hydration_without_measured_restore(self) -> None:
        payload = receipt()
        payload["cells"]["ann-top10-recall-095"]["ann_read_view_open"][
            "hydration_restore_count"
        ] = 0
        with self.assertRaisesRegex(GateFailure, "contradicts"):
            validate(payload, "a" * 40)

    def test_hybrid_evidence_is_valid_only_in_receipt_v4(self) -> None:
        payload = receipt()
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

        payload["schema"] = "hyphae-native-g7-receipt-v3"
        with self.assertRaisesRegex(GateFailure, "identity"):
            validate(payload, "a" * 40)

    def test_bm25_requires_one_prepared_lexical_read_view_authority(self) -> None:
        for mutation in ("missing-open", "extra-open", "foreign-root", "foreign-index"):
            with self.subTest(mutation=mutation):
                payload = receipt()
                cell = payload["cells"]["bm25-top10"]
                if mutation == "missing-open":
                    del cell["lexical_read_view_open"]
                elif mutation == "extra-open":
                    cell["lexical_read_view_open"]["unexpected"] = 0
                elif mutation == "foreign-root":
                    cell["lexical_read_view_open"]["root_identity"] = "a" * 64
                else:
                    cell["lexical_read_view_open"]["lexical_index_identity"] = "b" * 64
                with self.assertRaisesRegex(GateFailure, "BM25.*read-view"):
                    validate(payload, "a" * 40)

    def test_bm25_prepared_open_requires_exact_bounded_plan(self) -> None:
        mutations = (
            ("planned_terms", 2),
            ("retained_postings", 2),
            ("maximum_retained_postings", 11),
            ("maximum_retained_bytes", 1_048_577),
            ("observed_physical_entries", 1_000_001),
            ("observed_physical_bytes", 1_000_000_001),
            ("retained_memory_bytes", 1_048_577),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["bm25-top10"]["lexical_read_view_open"][field] = value
                with self.assertRaisesRegex(GateFailure, "BM25.*read-view"):
                    validate(payload, "a" * 40)

    def test_bm25_interval_requires_every_fresh_execution_and_zero_storage(self) -> None:
        mutations = (
            ("observations", 999_999),
            ("postings_evaluated", 999_999),
            ("execution_sequence_last", 1_099_999),
            ("receipt_physical_page_reads", 1),
            ("process_physical_page_reads", 1),
            ("full_state_loads", 1),
            ("full_catalog_loads", 1),
            ("lexical_execution", "cached-ranking-v1"),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["bm25-top10"]["lexical_read_view_query_interval"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "BM25.*interval"):
                    validate(payload, "a" * 40)

        payload = receipt()
        payload["cells"]["bm25-top10"]["lexical_read_view_query_interval"][
            "observations"
        ] = True
        with self.assertRaisesRegex(GateFailure, "BM25.*interval"):
            validate(payload, "a" * 40)

    def test_filtered_bm25_requires_exact_same_root_authority(self) -> None:
        for mutation in (
            "missing-open", "extra-open", "foreign-root", "foreign-csn",
            "foreign-lexical", "zero-filter-identity", "wrong-scope",
            "wrong-route", "wrong-correctness",
        ):
            with self.subTest(mutation=mutation):
                payload = receipt()
                cell = payload["cells"]["filtered-bm25-top10"]
                view = cell["filtered_lexical_read_view_open"]
                if mutation == "missing-open":
                    del cell["filtered_lexical_read_view_open"]
                elif mutation == "extra-open":
                    view["unexpected"] = 0
                elif mutation == "foreign-root":
                    view["root_identity"] = "a" * 64
                elif mutation == "foreign-csn":
                    view["snapshot_csn"] = 2
                elif mutation == "foreign-lexical":
                    view["lexical_index_identity"] = "b" * 64
                elif mutation == "zero-filter-identity":
                    view["structure_filter_identity"] = "0" * 64
                elif mutation == "wrong-scope":
                    view["lexical_plan_scope"] = "cached-final-ranking-v1"
                elif mutation == "wrong-route":
                    cell["route"] = "latest-snapshot-filter"
                else:
                    cell["correctness_scope"] = "best-effort"
                with self.assertRaisesRegex(GateFailure, "filtered BM25"):
                    validate(payload, "a" * 40)

    def test_filtered_bm25_open_requires_bounded_plan_and_admission(self) -> None:
        mutations = (
            ("retained_filter_records", 2),
            ("planned_filter_physical_entries", 2),
            ("observed_filter_physical_entries", 2),
            ("observed_filter_physical_bytes", 257),
            ("retained_filter_memory_bytes", 257),
            ("structure_filter_value_scope", "blob-or-inline-v1"),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["filtered-bm25-top10"][
                    "filtered_lexical_read_view_open"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "filtered BM25"):
                    validate(payload, "a" * 40)

        for phase, field, value in (
            ("filter_planning", "compute_threads", 2),
            ("filter_planning", "io_slots", 0),
            ("filter_hydration", "class", "bulk"),
            ("filter_hydration", "memory_bytes", 127),
            ("filter_hydration", "queue_ticket", True),
            ("filter_hydration", "unexpected", 0),
        ):
            with self.subTest(phase=phase, field=field):
                payload = receipt()
                payload["cells"]["filtered-bm25-top10"][
                    "filtered_lexical_read_view_open"
                ][phase][field] = value
                with self.assertRaisesRegex(GateFailure, "filtered BM25.*admission"):
                    validate(payload, "a" * 40)

    def test_filtered_bm25_interval_reexecutes_filter_before_rank(self) -> None:
        mutations = (
            ("observations", 999_999),
            ("execution_sequence_first", 100_002),
            ("execution_sequence_last", 1_099_999),
            ("postings_scored", 999_999),
            ("filter_records_evaluated", 999_999),
            ("filter_records_matched", 999_999),
            ("receipt_physical_page_reads", 1),
            ("process_physical_page_reads", 1),
            ("full_state_loads", 1),
            ("full_catalog_loads", 1),
            ("filter_execution", "cached-filter-result-v1"),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["filtered-bm25-top10"][
                    "filtered_lexical_read_view_query_interval"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "filtered BM25.*interval"):
                    validate(payload, "a" * 40)

        for field, value in (
            ("corpus_filter_density", 0.500_001),
            ("candidate_filter_selectivity", 0.999_999),
            ("candidate_filter_selectivity", True),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["filtered-bm25-top10"][field] = value
                with self.assertRaisesRegex(GateFailure, "filtered BM25"):
                    validate(payload, "a" * 40)

    def test_execution_authority_requires_exact_fields(self) -> None:
        for mutation in ("missing", "extra"):
            with self.subTest(mutation=mutation):
                payload = receipt()
                evidence = payload["execution_authority"]
                if mutation == "missing":
                    del evidence["registered_pools"]
                else:
                    evidence["unexpected"] = 0
                with self.assertRaisesRegex(GateFailure, "authority.*fields"):
                    validate(payload, "a" * 40)

    def test_execution_authority_binds_topology_and_exact_runner(self) -> None:
        for field, value in (
            ("topology_digest", "f" * 64),
            ("runner_executable_blake3", "f" * 64),
            ("calibration_executable_blake3", "f" * 64),
            ("runner_executable_blake3", "not-a-digest"),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["execution_authority"][field] = value
                with self.assertRaisesRegex(GateFailure, "authority identity"):
                    validate(payload, "a" * 40)

    def test_execution_authority_requires_canonical_complete_surfaces(self) -> None:
        mutations = ("missing", "invented", "duplicate", "wrong-count", "wrong-pools")
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                payload = receipt()
                evidence = payload["execution_authority"]
                if mutation == "missing":
                    evidence["installed_surfaces"].remove("local-sql-daemon")
                    evidence["installations"] -= 1
                elif mutation == "invented":
                    evidence["installed_surfaces"].append("second-execution-pool")
                    evidence["installed_surfaces"].sort()
                    evidence["installations"] += 1
                elif mutation == "duplicate":
                    evidence["installed_surfaces"].append("search-fixture")
                    evidence["installed_surfaces"].sort()
                    evidence["installations"] += 1
                elif mutation == "wrong-count":
                    evidence["installations"] += 1
                else:
                    evidence["registered_pools"] = 2
                with self.assertRaisesRegex(GateFailure, "surfaces or pools"):
                    validate(payload, "a" * 40)

    def test_execution_authority_reconciles_dispatch_and_numa_evidence(self) -> None:
        for field, value, message in (
            ("completed_jobs", 10, "counters"),
            ("local_dispatches", True, "counters"),
            ("numa_steal_status", "unknown", "NUMA"),
            ("stolen_dispatches", 1, "NUMA"),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["execution_authority"][field] = value
                if field == "stolen_dispatches":
                    payload["execution_authority"]["local_dispatches"] = 8
                with self.assertRaisesRegex(GateFailure, message):
                    validate(payload, "a" * 40)

        payload = receipt()
        evidence = payload["execution_authority"]
        evidence["numa_steal_status"] = "calibrated"
        evidence["local_dispatches"] = 7
        evidence["stolen_dispatches"] = 2
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_hybrid_view_must_share_ann_root_csn_and_view_identity(self) -> None:
        for field, value in (
            ("root_identity", "a" * 64),
            ("snapshot_csn", 2),
            ("ann_view_identity", "b" * 64),
            ("lexical_plan_scope", "final-result-cache-v1"),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["hybrid-top10"]["hybrid_read_view_open"][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*authority"):
                    validate(payload, "a" * 40)

    def test_hybrid_query_queue_wait_must_equal_ann_authority(self) -> None:
        for mutation, value in (
            ("missing", None),
            ("zero", 0),
            ("mismatch", 59_999),
            ("bool", True),
        ):
            with self.subTest(mutation=mutation):
                payload = receipt()
                cell = payload["cells"]["hybrid-top10"]
                if mutation == "missing":
                    del cell["query_queue_wait_millis"]
                else:
                    cell["query_queue_wait_millis"] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*queue wait"):
                    validate(payload, "a" * 40)

    def test_hybrid_view_rejects_unplanned_or_excess_open_work(self) -> None:
        payload = receipt()
        payload["cells"]["hybrid-top10"]["hybrid_read_view_open"]["unexpected"] = 0
        with self.assertRaisesRegex(GateFailure, "hybrid read-view open"):
            validate(payload, "a" * 40)

        for observed, planned in (
            ("observed_physical_entries", "planned_physical_entries"),
            ("observed_physical_bytes", "planned_physical_bytes"),
        ):
            with self.subTest(observed=observed):
                payload = receipt()
                view = payload["cells"]["hybrid-top10"]["hybrid_read_view_open"]
                view[observed] = view[planned] + 1
                with self.assertRaisesRegex(GateFailure, "physical plan"):
                    validate(payload, "a" * 40)

        payload = receipt()
        view = payload["cells"]["hybrid-top10"]["hybrid_read_view_open"]
        view["retained_memory_bytes"] = view["admitted_retained_memory_bytes"] + 1
        with self.assertRaisesRegex(GateFailure, "retained memory"):
            validate(payload, "a" * 40)

    def test_hybrid_interval_requires_exact_observations_and_zero_storage_work(self) -> None:
        for field, value in (
            ("observations", 999_999),
            ("hydrations", 1),
            ("physical_page_reads", 1),
            ("index_scoped_restores", 1),
            ("full_state_loads", 1),
            ("full_catalog_loads", 1),
            ("lexical_execution", "cached-ranking-v1"),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["hybrid-top10"][
                    "hybrid_read_view_query_interval"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*interval"):
                    validate(payload, "a" * 40)

        payload = receipt()
        payload["cells"]["hybrid-top10"]["hybrid_read_view_query_interval"][
            "hydrations"
        ] = False
        with self.assertRaisesRegex(GateFailure, "hybrid.*interval"):
            validate(payload, "a" * 40)

    def test_hybrid_interval_requires_atomic_peak_admission_for_every_query(self) -> None:
        for field, value in (
            ("peak_admission_executions", 999_999),
            ("peak_admission_class", "bulk"),
            ("peak_admission_compute_threads", 43),
            ("peak_admission_compute_threads", True),
            ("peak_admission_io_slots", 1),
            ("peak_admission_memory_bytes_min", 0),
            ("peak_admission_memory_bytes_min", 999_999),
            ("peak_admission_memory_bytes_max", 1_999_999),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["hybrid-top10"][
                    "hybrid_read_view_query_interval"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*peak admission"):
                    validate(payload, "a" * 40)

        for mutation in ("missing", "extra"):
            with self.subTest(mutation=mutation):
                payload = receipt()
                interval = payload["cells"]["hybrid-top10"][
                    "hybrid_read_view_query_interval"
                ]
                if mutation == "missing":
                    del interval["peak_admission_executions"]
                else:
                    interval["unexpected"] = 0
                with self.assertRaisesRegex(GateFailure, "hybrid.*fields"):
                    validate(payload, "a" * 40)

    def test_hybrid_interval_requires_result_retention_through_publication(self) -> None:
        for field, value in (
            ("result_retention_executions", 999_999),
            ("result_retention_class", "bulk"),
            ("result_retention_compute_threads", 1),
            ("result_retention_compute_threads", False),
            ("result_retention_io_slots", 1),
            ("result_retention_memory_bytes_min", 0),
            ("result_retention_memory_bytes_max", 999_999),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["hybrid-top10"][
                    "hybrid_read_view_query_interval"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*result retention"):
                    validate(payload, "a" * 40)

    def test_hybrid_interval_requires_compute_only_fusion_for_every_query(self) -> None:
        for field, value in (
            ("fusion_executions", 999_999),
            ("fusion_class", "bulk"),
            ("fusion_compute_threads", 2),
            ("fusion_compute_threads", True),
            ("fusion_io_slots", 1),
            ("fusion_memory_bytes", 1),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["hybrid-top10"][
                    "hybrid_read_view_query_interval"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*fusion"):
                    validate(payload, "a" * 40)

    def test_hybrid_routing_requires_selected_certification_for_every_observation(self) -> None:
        for field, value in (
            ("observations", 999_999),
            ("selected_certified", 999_999),
            ("full_fanout_requested", 1),
            ("full_fanout_budget_fallback", 1),
            ("single_generation_fallback", 1),
            ("next_partition_lower_bound_present", 999_999),
            ("selected_partitions_max", 33),
            ("execution_waves_max", 7),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["hybrid-top10"]["hybrid_ann_routing_interval"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*routing"):
                    validate(payload, "a" * 40)

    def test_hybrid_routing_requires_finite_strict_omission_bound(self) -> None:
        for field, value in (
            ("minimum_next_partition_lower_bound", float("nan")),
            ("minimum_next_partition_lower_bound", float("inf")),
            ("minimum_next_partition_lower_bound", 0.20),
            ("maximum_kth_distance", float("nan")),
            ("maximum_kth_distance", float("inf")),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["hybrid-top10"]["hybrid_ann_routing_interval"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*bound"):
                    validate(payload, "a" * 40)

    def test_hybrid_oracle_recomputes_rrf_order_explanations_and_digest(self) -> None:
        mutations = (
            ("rrf_constant", 61),
            ("result_digest", "0" * 64),
            ("oracle_digest", "0" * 64),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["hybrid-top10"]["hybrid_oracle"][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*oracle"):
                    validate(payload, "a" * 40)

        payload = receipt()
        results = payload["cells"]["hybrid-top10"]["hybrid_oracle"]["fused_results"]
        results[0]["fusion_score"] += 1
        with self.assertRaisesRegex(GateFailure, "hybrid.*oracle"):
            validate(payload, "a" * 40)

        payload = receipt()
        results = payload["cells"]["hybrid-top10"]["hybrid_oracle"]["fused_results"]
        results[0], results[1] = results[1], results[0]
        with self.assertRaisesRegex(GateFailure, "hybrid.*oracle"):
            validate(payload, "a" * 40)

    def test_hybrid_oracle_rejects_foreign_authority_and_noncanonical_inputs(self) -> None:
        for field, value in (("root_identity", "a" * 64), ("snapshot_csn", 2)):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["hybrid-top10"]["hybrid_oracle"][field] = value
                with self.assertRaisesRegex(GateFailure, "hybrid.*oracle authority"):
                    validate(payload, "a" * 40)

        payload = receipt()
        ranking = payload["cells"]["hybrid-top10"]["hybrid_oracle"][
            "vector_ranking"
        ]
        ranking[1] = ranking[0]
        with self.assertRaisesRegex(GateFailure, "hybrid.*ranking"):
            validate(payload, "a" * 40)

        payload = receipt()
        payload["cells"]["hybrid-top10"]["hybrid_oracle"]["lexical_ranking"][0] = (
            "0" * 32
        )
        with self.assertRaisesRegex(GateFailure, "hybrid.*ranking"):
            validate(payload, "a" * 40)

    def test_hybrid_oracle_rejects_bool_as_numeric_evidence(self) -> None:
        payload = receipt()
        payload["cells"]["hybrid-top10"]["hybrid_oracle"]["lexical_weight"] = True
        with self.assertRaisesRegex(GateFailure, "hybrid.*oracle authority"):
            validate(payload, "a" * 40)

        payload = receipt()
        payload["cells"]["hybrid-top10"]["hybrid_oracle"]["fused_results"][0][
            "final_rank"
        ] = True
        with self.assertRaisesRegex(GateFailure, "hybrid.*explanation"):
            validate(payload, "a" * 40)

    def test_initial_bulk_accepts_compute_only_governor_request(self) -> None:
        payload = receipt()
        self.assertEqual(payload["initial_ann_bulk"]["governor_execution"]["io_slots"], 0)
        result = validate(payload, "a" * 40)
        self.assertEqual(result["status"], "passed")

    def test_initial_bulk_rejects_governor_resource_mismatch(self) -> None:
        for field, value in (("compute_threads", 43), ("memory_bytes", 3_999_999_999)):
            with self.subTest(field=field):
                payload = receipt()
                payload["initial_ann_bulk"]["governor_execution"][field] = value
                with self.assertRaisesRegex(GateFailure, "governor execution"):
                    validate(payload, "a" * 40)

    def test_initial_bulk_rejects_negative_governor_resources(self) -> None:
        for field in ("compute_threads", "io_slots", "memory_bytes"):
            with self.subTest(field=field):
                payload = receipt()
                payload["initial_ann_bulk"]["governor_execution"][field] = -1
                with self.assertRaisesRegex(GateFailure, "governor execution"):
                    validate(payload, "a" * 40)

    def test_initial_bulk_rejects_invented_io_reservation(self) -> None:
        payload = receipt()
        payload["initial_ann_bulk"]["governor_execution"]["io_slots"] = 1
        with self.assertRaisesRegex(GateFailure, "governor execution"):
            validate(payload, "a" * 40)

    def test_hot_path_complete_state_loads_fail_closure(self) -> None:
        for counter in ("process_full_state_loads", "process_full_catalog_loads"):
            with self.subTest(counter=counter):
                payload = receipt()
                materialization_counter = counter.removeprefix("process_")
                payload["cells"]["embedded-structure-point-get"]["materialization"][
                    materialization_counter
                ] = 1
                with self.assertRaisesRegex(GateFailure, "hot path materialized"):
                    validate(payload, "a" * 40)

    def test_parallel_topology_requires_multiple_worker_batches(self) -> None:
        payload = receipt()
        payload["initial_ann_bulk"]["worker_batches"] = 1
        with self.assertRaisesRegex(GateFailure, "parallel construction"):
            validate(payload, "a" * 40)

    def test_linux_initial_bulk_requires_hard_affinity(self) -> None:
        payload = receipt()
        payload["initial_ann_bulk"]["hard_affinity"] = False
        with self.assertRaisesRegex(GateFailure, "parallel construction"):
            validate(payload, "a" * 40)

    def test_initial_bulk_rejects_unrepresentable_partition_count(self) -> None:
        payload = receipt()
        payload["initial_ann_bulk"]["planned_partitions"] = 112
        with self.assertRaisesRegex(GateFailure, "parallel construction"):
            validate(payload, "a" * 40)

    def test_large_topology_does_not_change_logical_partition_layout(self) -> None:
        payload = receipt()
        bulk = payload["initial_ann_bulk"]
        bulk["topology_workers"] = 256
        bulk["planned_workers"] = 64
        bulk["worker_batches"] = 64
        bulk["governor_execution"]["compute_threads"] = 64
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_valid_dedicated_darwin_receipt(self) -> None:
        payload = receipt()
        payload["platform"] = "darwin"
        payload["build"]["target"] = "aarch64-apple-darwin"
        payload["build"]["rustc"] = "rustc 1.96.0\nhost: aarch64-apple-darwin"
        payload["build"]["os"] = "macOS-test"
        result = validate(payload, "a" * 40)
        self.assertEqual(result["status"], "passed")

    def test_darwin_receipt_rejects_linux_target(self) -> None:
        payload = receipt()
        payload["platform"] = "darwin"
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_claims_fail_closed(self) -> None:
        payload = receipt()
        payload["claims"] = ["sub-millisecond"]
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_zero_latency_fails_closed(self) -> None:
        payload = copy.deepcopy(receipt())
        payload["cells"]["embedded-structure-point-get"]["p50"] = 0
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_missing_required_cell_fails_closed(self) -> None:
        payload = receipt()
        payload["cells"].pop("hybrid-top10")
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_unavailable_counter_fails_closure(self) -> None:
        payload = receipt()
        payload["counters"]["rss"] = {"status": "unavailable", "value": None}
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_empty_physical_observation_fails_closure(self) -> None:
        payload = receipt()
        payload["physical_observation"] = {}
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_latency_target_is_enforced(self) -> None:
        payload = copy.deepcopy(receipt())
        payload["cells"]["bm25-top10"]["p99"] = 500_001
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)

    def test_saturation_receipt_does_not_reapply_single_client_target(self) -> None:
        payload = copy.deepcopy(receipt())
        payload["concurrency"] = 32
        payload["cells"]["bm25-top10"]["p50"] = 10_000_000
        payload["cells"]["bm25-top10"]["p99"] = 20_000_000
        payload["cells"]["strict-group-commit"]["group_commit_evidence"] = (
            _strict_group_commit_evidence(concurrency=32)
        )
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_group_commit_research_target_is_advisory(self) -> None:
        payload = copy.deepcopy(receipt())
        cell = payload["cells"]["strict-group-commit"]
        advisory = _latency_summary(
            2_000_000,
            2_500_000,
            3_000_000,
            3_500_000,
            4_000_000,
        )
        cell.update(advisory)
        cell["group_commit_evidence"]["timings_nanoseconds"]["end_to_end"] = (
            advisory
        )
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_group_commit_accepts_exact_full_and_pilot_cohort_plans(self) -> None:
        for observations, expected_sizes, expected_positions, final_size in (
            (
                1_000_000,
                {"32": 31_250},
                {str(position): 31_250 for position in range(32)},
                32,
            ),
            (
                10_000,
                {"32": 312, "16": 1},
                {
                    str(position): 313 if position < 16 else 312
                    for position in range(32)
                },
                16,
            ),
        ):
            for concurrency in (1, 8, 32):
                with self.subTest(observations=observations, concurrency=concurrency):
                    cell = {
                        "status": "measured",
                        **_latency_summary(),
                        "throughput_per_second": 1.0,
                        "durability": "group-physical-sync",
                        "group_commit_evidence": _strict_group_commit_evidence(
                            observations,
                            concurrency,
                        ),
                    }
                    validate_strict_group_commit_cell(
                        cell,
                        observations,
                        concurrency,
                    )
                    evidence = cell["group_commit_evidence"]
                    self.assertEqual(evidence["cohort_size_histogram"], expected_sizes)
                    self.assertEqual(
                        evidence["cohort_position_histogram"],
                        expected_positions,
                    )
                    self.assertEqual(evidence["final_cohort_size"], final_size)

    def test_group_commit_requires_exact_evidence_fields(self) -> None:
        for mutation in ("missing", "extra"):
            with self.subTest(mutation=mutation):
                payload = receipt()
                evidence = payload["cells"]["strict-group-commit"][
                    "group_commit_evidence"
                ]
                if mutation == "missing":
                    del evidence["cohort_width"]
                else:
                    evidence["unexpected"] = 0
                with self.assertRaisesRegex(GateFailure, "group-commit.*fields"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_fixed_bounded_real_concurrency(self) -> None:
        for field, value in (
            ("producer_concurrency", 8),
            ("maximum_active_producers", 0),
            ("cohort_width", 31),
            ("submission_mode", "natural-timed-collection-v1"),
            ("scheduler_queue_capacity", 65),
            ("outstanding_limit", 31),
            ("maximum_outstanding", 33),
            ("maximum_outstanding", True),
        ):
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["cells"]["strict-group-commit"]["group_commit_evidence"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*configuration"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_exact_full_cohorts_and_terminal_remainder(self) -> None:
        mutations = (
            ("logical_commits", 999_999),
            ("cohort_count", 31_249),
            ("final_cohort_size", 16),
            ("cohort_size_histogram", {"32": 31_249, "16": 2}),
            (
                "cohort_position_histogram",
                {str(position): 31_250 for position in range(31)},
            ),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["strict-group-commit"]["group_commit_evidence"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*cohort"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_contiguous_distinct_commit_receipts(self) -> None:
        for field, value in (
            ("first_commit_csn", 4),
            ("last_commit_csn", 1_000_003),
            ("distinct_commit_csns", 999_999),
            ("distinct_commit_csns", False),
            ("commit_receipt_digest_algorithm", "sha256"),
            ("commit_receipt_digest", "0" * 63),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["strict-group-commit"]["group_commit_evidence"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*commit receipt"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_one_page_and_wal_sync_per_cohort(self) -> None:
        for field, value in (
            ("page_synchronizations", 31_249),
            ("wal_synchronizations", 31_251),
            ("cohort_execution_nanos_total", 62_499),
            ("page_synchronization_nanos_total", 0),
            ("wal_synchronization_nanos_total", True),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["strict-group-commit"]["group_commit_evidence"][
                    field
                ] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*synchronization"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_exact_timing_samples_and_end_to_end_stats(self) -> None:
        for mutation, field, value in (
            ("count", "timing_sample_count", 999_999),
            ("component", "p95", 0),
            ("top-level", "p99", 4),
            ("extra", "unexpected", {}),
            ("summary-missing", "maximum", None),
            ("summary-extra", "average", 1),
        ):
            with self.subTest(mutation=mutation):
                payload = receipt()
                cell = payload["cells"]["strict-group-commit"]
                evidence = cell["group_commit_evidence"]
                if mutation == "count":
                    evidence[field] = value
                elif mutation == "component":
                    evidence["timings_nanoseconds"]["queue_wait"][field] = value
                elif mutation == "top-level":
                    cell[field] = value
                elif mutation == "summary-missing":
                    del evidence["timings_nanoseconds"]["queue_wait"][field]
                elif mutation == "summary-extra":
                    evidence["timings_nanoseconds"]["queue_wait"][field] = value
                else:
                    evidence["timings_nanoseconds"][field] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*timing"):
                    validate(payload, COMMIT)

    def test_group_commit_requires_exact_reopen_equivalence(self) -> None:
        for field, value in (
            ("baseline_visible_csn", 3),
            ("reopened_visible_csn", 1_000_003),
            ("reopened_committed_transactions", 1_000_001),
            ("verified_logical_commits", 999_999),
            ("missing_keys", 1),
            ("mismatched_values", True),
            ("recovered_state_digest", "c" * 64),
            ("open_time_nanos", 0),
            ("verification_time_nanos", False),
        ):
            with self.subTest(field=field):
                payload = receipt()
                payload["cells"]["strict-group-commit"]["group_commit_evidence"][
                    "reopen"
                ][field] = value
                with self.assertRaisesRegex(GateFailure, "group-commit.*reopen"):
                    validate(payload, COMMIT)

        for mutation in ("missing", "extra"):
            with self.subTest(mutation=mutation):
                payload = receipt()
                reopen = payload["cells"]["strict-group-commit"][
                    "group_commit_evidence"
                ]["reopen"]
                if mutation == "missing":
                    del reopen["provider"]
                else:
                    reopen["unexpected"] = 0
                with self.assertRaisesRegex(GateFailure, "group-commit.*reopen.*fields"):
                    validate(payload, COMMIT)

    def test_interference_requires_control_comparison(self) -> None:
        payload = interference_receipt()
        payload["background_interference"].pop("p99_ratio_by_cell")
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)


if __name__ == "__main__":
    unittest.main()
