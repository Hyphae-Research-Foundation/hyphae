#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import unittest

from tools.check_native_g1_latency_receipts import GateFailure, validate_receipts


class NativeG1LatencyReceiptTests(unittest.TestCase):
    def embedded(self) -> dict:
        return {
            "schema": "hyphae.native.microsecond-smoke.v16",
            "status": "observation-not-gate",
            "commit": "a" * 40,
            "rustc": "rustc 1.96.0",
            "target": "x86_64-linux",
            "profile": "release",
            "observations_per_operation": 1_000_000,
            "warmup_per_operation": 100_000,
            "operations": {
                "embedded_structure_get_64b": {"p50_nanos": 100, "p99_nanos": 200, "throughput_per_second": 10.0},
                "embedded_prepared_sql_pk_materialized_scaled_snapshot": {"p50_nanos": 110, "p99_nanos": 220, "throughput_per_second": 9.0},
                "buffered_inverted_btree_bm25_match_top1_rare_term": {"p50_nanos": 120, "p99_nanos": 240, "throughput_per_second": 8.0},
            },
        }

    def protocol(self) -> dict:
        names = [
            "persistent_ping_round_trip_32b",
            "persistent_transaction_sql_stage_round_trip",
            "persistent_transaction_structure_stage_round_trip",
            "persistent_transaction_search_stage_round_trip",
            "persistent_transaction_memory_commit_round_trip",
            "persistent_transaction_strict_commit_round_trip",
        ]
        return {
            "schema": "hyphae.native.local-all-engine-transaction-smoke.v1",
            "status": "observation-not-regression-gate",
            "implementation_commit": "a" * 40,
            "harness_commit": "a" * 40,
            "target": "x86_64-linux",
            "profile": "release",
            "concurrency": 1,
            "warm_state": True,
            "staged_operations_per_transaction": 3,
            "operations": {
                name: {
                    "p50_nanos": 100,
                    "p95_nanos": 150,
                    "p99_nanos": 200,
                    "p999_nanos": 250,
                    "maximum_nanos": 300,
                    "throughput_per_second": 10.0,
                }
                for name in names
            },
        }

    def test_complete_bounded_observations_pass(self) -> None:
        result = validate_receipts(self.embedded(), self.protocol(), "a" * 40)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["scope"], "bounded-observation")
        self.assertFalse(result["production_scale"])

    def test_commit_mismatch_fails_closed(self) -> None:
        protocol = self.protocol()
        protocol["harness_commit"] = "b" * 40
        with self.assertRaisesRegex(GateFailure, "commit"):
            validate_receipts(self.embedded(), protocol, "a" * 40)

    def test_missing_operation_or_nonfinite_metric_fails_closed(self) -> None:
        protocol = self.protocol()
        protocol["operations"].pop("persistent_transaction_search_stage_round_trip")
        with self.assertRaisesRegex(GateFailure, "operation set"):
            validate_receipts(self.embedded(), protocol, "a" * 40)
        embedded = self.embedded()
        embedded["operations"]["embedded_structure_get_64b"]["throughput_per_second"] = float("inf")
        with self.assertRaisesRegex(GateFailure, "finite"):
            validate_receipts(embedded, self.protocol(), "a" * 40)

    def test_scope_cannot_claim_g7_production_scale(self) -> None:
        embedded = self.embedded()
        embedded["status"] = "passed-production"
        with self.assertRaisesRegex(GateFailure, "bounded observation"):
            validate_receipts(embedded, self.protocol(), "a" * 40)


if __name__ == "__main__":
    unittest.main()
