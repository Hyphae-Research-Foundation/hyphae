#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import copy
import unittest

from tools.check_native_g7_receipt import GateFailure, validate


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
    return {
        "schema": "hyphae-native-g7-receipt-v3",
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
            "source_tree": "d" * 40,
        },
        "dataset": {
            "observations": 1_000_000,
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
            "input_identity": "1" * 64,
            "aggregate_identity": "2" * 64,
            "planned_vectors": 1_000_000,
            "planned_partitions": 48,
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
        "cells": {name: dict(cell) for name in {
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
        }},
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
        payload["initial_ann_bulk"]["planned_partitions"] = 222
        payload["initial_ann_bulk"]["topology_workers"] = 222
        with self.assertRaisesRegex(GateFailure, "parallel construction"):
            validate(payload, "a" * 40)

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
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_group_commit_research_target_is_advisory(self) -> None:
        payload = copy.deepcopy(receipt())
        payload["cells"]["strict-group-commit"]["p50"] = 2_000_000
        payload["cells"]["strict-group-commit"]["p99"] = 3_000_000
        self.assertEqual(validate(payload, "a" * 40)["status"], "passed")

    def test_interference_requires_control_comparison(self) -> None:
        payload = interference_receipt()
        payload["background_interference"].pop("p99_ratio_by_cell")
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)


if __name__ == "__main__":
    unittest.main()
