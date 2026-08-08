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
    }
    return {
        "schema": "hyphae-native-g7-receipt-v1",
        "gate": "G7",
        "status": "passed",
        "evidence_class": "supporting-not-closure",
        "source_commit": "a" * 40,
        "platform": "linux",
        "state": "warm",
        "concurrency": 1,
        "dataset": {"observations": 100_000},
        "cells": {name: cell for name in {
            "embedded-structure-point-get",
            "embedded-prepared-sql-primary-key",
            "local-structure-point-get",
            "local-prepared-sql-primary-key",
            "indexed-sql-bounded-read",
            "bm25-top10",
            "filtered-bm25-top10",
            "ann-top10-recall-095",
            "hybrid-top10",
            "strict-group-commit",
        }},
        "counters": {
            name: {"status": "unavailable", "value": None}
            for name in (
                "allocations", "rss", "cpu_cycles", "cache_misses",
                "page_faults", "bytes_read", "bytes_written",
            )
        },
        "saturation": {"status": "measured"},
        "background_interference": {"status": "measured"},
        "claims": [],
        "closure_declared": False,
        "physical_observation": {},
    }


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

    def test_unavailable_counter_cannot_claim_a_value(self) -> None:
        payload = receipt()
        payload["counters"]["rss"]["value"] = 1
        with self.assertRaises(GateFailure):
            validate(payload, "a" * 40)


if __name__ == "__main__":
    unittest.main()
