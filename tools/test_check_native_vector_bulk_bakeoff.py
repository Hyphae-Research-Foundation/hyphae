#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import copy
import unittest

from tools.check_native_vector_bulk_bakeoff import GateFailure, validate


COMMIT = "a" * 40


def receipt() -> dict:
    return {
        "schema": "hyphae-native-vector-bulk-bakeoff-v1",
        "status": "diagnostic",
        "source_commit": COMMIT,
        "hardware_fingerprint": "b" * 64,
        "governor_calibration_cache_key": "c" * 64,
        "dataset": {
            "generator": "hyphae-partitioned-hnsw-bakeoff-v1",
            "digest": "d" * 64,
            "vectors": 256,
            "dimension": 8,
            "metric": "squared-l2",
            "corpus_construction_nanos": 100,
        },
        "build": {
            "requested_partitions": 8,
            "effective_partitions": 8,
            "serial_hnsw_nanos": 100,
            "serial_partitioned_nanos": 80,
            "parallel_partitioned_nanos": 20,
            "planned_compute_threads": 4,
            "planned_memory_bytes": 1024,
            "worker_batches": 4,
            "single_build_identity": "e" * 64,
            "partitioned_build_identity": "f" * 64,
            "deterministic_across_serial_and_parallel": True,
            "durable_publication": False,
        },
        "quality": {
            "queries": 16,
            "k": 10,
            "ef_search": 80,
            "selected_partitions": 4,
            "single_hnsw_recall_ppm": 980_000,
            "partitioned_hnsw_recall_ppm": 970_000,
            "selected_partition_recall_ppm": 960_000,
            "minimum_single_query_recall_ppm": 900_000,
            "minimum_partitioned_query_recall_ppm": 900_000,
            "minimum_selected_query_recall_ppm": 800_000,
            "single_query_batch_nanos": 500,
            "partitioned_query_batch_nanos": 400,
            "selected_query_batch_nanos": 200,
            "oracle": "partitioned-exact-flat-canonical-top-k-v1",
        },
        "missing_gate_evidence": [
            "peak-rss",
            "write-amplification",
            "checkpoint-restart",
            "durable-publication-and-reopen",
            "update-delete-consolidation",
            "accepted-corpus-matrix",
        ],
        "claims": [],
        "closure_declared": False,
    }


class NativeVectorBulkBakeoffTests(unittest.TestCase):
    def test_accepts_open_diagnostic_receipt(self) -> None:
        self.assertEqual(validate(receipt(), COMMIT)["status"], "passed")

    def test_rejects_source_or_closure_substitution(self) -> None:
        with self.assertRaisesRegex(GateFailure, "source commit"):
            validate(receipt(), "1" * 40)
        payload = receipt()
        payload["closure_declared"] = True
        with self.assertRaisesRegex(GateFailure, "open state"):
            validate(payload, COMMIT)

    def test_rejects_worker_and_recall_inconsistency(self) -> None:
        payload = receipt()
        payload["build"]["planned_compute_threads"] = 9
        with self.assertRaisesRegex(GateFailure, "worker bounds"):
            validate(payload, COMMIT)
        payload = copy.deepcopy(receipt())
        payload["quality"]["minimum_partitioned_query_recall_ppm"] = 980_000
        with self.assertRaisesRegex(GateFailure, "recall bounds"):
            validate(payload, COMMIT)

    def test_rejects_hidden_gate_evidence(self) -> None:
        payload = receipt()
        payload["missing_gate_evidence"].remove("peak-rss")
        with self.assertRaisesRegex(GateFailure, "missing-evidence"):
            validate(payload, COMMIT)


if __name__ == "__main__":
    unittest.main()
