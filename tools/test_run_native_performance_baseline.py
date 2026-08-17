#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

from tools.check_native_performance_receipt import validate_receipt, validate_suite
from tools.run_native_performance_baseline import (
    assemble_receipt,
    assemble_suite,
    measured_counter,
    unsupported_counter,
)


COMMIT = "a" * 40


def sample() -> dict:
    return {
        "schema": "hyphae-native-performance-sample-v1",
        "source_commit": COMMIT,
        "workload": {
            "class": "foreground-point",
            "engines": ["structures"],
            "operation": "embedded-structure-point-get",
            "parameters": {"key_bytes": 4, "value_bytes": 64},
        },
        "dataset": {
            "generator": "test-generator",
            "digest": "b" * 64,
            "records": 2_048,
            "bytes": 139_264,
        },
        "measurement": {
            "observations": 10_000,
            "warmup": 1_000,
            "concurrency": 1,
            "state": "warm",
            "background_mode": "control",
            "elapsed_nanos": 100,
            "engine_execution_nanos": 90,
        },
        "correctness": {
            "status": "passed",
            "oracle": "test-oracle",
            "result_digest": "c" * 64,
        },
    }


def counters() -> dict[str, dict]:
    units = {
        "cpu_time": "nanoseconds",
        "cpu_cycles": "cycles",
        "instructions": "count",
        "cache_misses": "count",
        "context_switches": "count",
        "page_faults": "count",
        "allocations": "count",
        "peak_rss": "bytes",
        "bytes_read": "bytes",
        "bytes_written": "bytes",
    }
    return {
        name: (
            measured_counter(unit, 1, "test-provider")
            if name in {"cpu_time", "peak_rss"}
            else unsupported_counter(unit, "not attached in test")
        )
        for name, unit in units.items()
    }


class NativePerformanceBaselineTests(unittest.TestCase):
    def test_assembles_auditable_diagnostic_receipt(self) -> None:
        receipt = assemble_receipt(
            sample(),
            counters(),
            "d" * 40,
            False,
            {
                "target": "x86_64-unknown-linux-gnu",
                "compiler": "rustc test",
                "binary_sha256": "e" * 64,
            },
        )
        self.assertEqual(
            receipt["measurement"]["clock_totals_nanos"]["unattributed"],
            10,
        )
        self.assertEqual(validate_receipt(receipt, COMMIT)["status"], "passed")
        self.assertEqual(validate_suite(assemble_suite(receipt), COMMIT)["cells"], 1)

    def test_rejects_component_time_larger_than_elapsed(self) -> None:
        payload = sample()
        payload["measurement"]["engine_execution_nanos"] = 101
        with self.assertRaisesRegex(RuntimeError, "clocks"):
            assemble_receipt(
                payload,
                counters(),
                "d" * 40,
                False,
                {
                    "target": "x86_64-unknown-linux-gnu",
                    "compiler": "rustc test",
                    "binary_sha256": "e" * 64,
                },
            )


if __name__ == "__main__":
    unittest.main()
