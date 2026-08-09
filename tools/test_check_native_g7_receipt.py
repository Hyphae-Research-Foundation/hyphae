#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

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
    }
    return {
        "schema": "hyphae-native-g7-receipt-v2",
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
            "product_search_seed": "strict-committed",
            "commit_cell": "group-physical-sync",
        },
        "proofs_included": False,
        "correctness": {
            "cell_assertions": "passed",
            "ann_recall_floor": 0.95,
            "cross_engine_visibility": "integrated-product-search",
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
        "cells": {name: cell for name in {
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

    def test_interference_requires_control_comparison(self) -> None:
        payload = interference_receipt()
        payload["background_interference"].pop("p99_ratio_by_cell")
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)


if __name__ == "__main__":
    unittest.main()
