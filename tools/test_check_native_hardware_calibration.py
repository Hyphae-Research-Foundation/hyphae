#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the Native hardware calibration semantic checker."""

from __future__ import annotations

import copy
import unittest

from tools.check_native_hardware_calibration import (
    CalibrationValidationError,
    validate_receipt,
)


DIGEST = "1" * 64


def valid_receipt() -> dict:
    return {
        "schema": "hyphae-native-hardware-calibration-v1",
        "mode": "quick",
        "status": "stable",
        "accepted_for_scheduling": True,
        "cache_status": "disabled",
        "elapsed_ms": 7_500,
        "identity": {
            "hardware_fingerprint": DIGEST,
            "kernel_release": "test-kernel",
            "filesystem": "testfs",
            "compiler_identity": "rustc test",
            "hyphae_build_identity": "hyphae/test",
            "executable_blake3": DIGEST,
            "cache_key": DIGEST,
        },
        "policy": {
            "minimum_duration_ms": 5_000,
            "maximum_duration_ms": 15_000,
            "warmup_batches": 2,
            "samples_per_measurement": 15,
            "target_sample_duration_ms": 15,
            "maximum_relative_mad_ppm": 75_000,
            "maximum_relative_range_ppm": 500_000,
        },
        "feature_detection": {
            "instruction_sets": ["sse2"],
            "differential_tests_passed": True,
        },
        "measurements": [
            {
                "primitive": "vector-dot",
                "variant": "portable-iterator-f64",
                "input_size": 384,
                "input_unit": "dimensions",
                "bytes_per_operation": 3_072,
                "operations_per_sample": 10_000,
                "maximum_operations_per_sample": 4_294_967_296,
                "sample_count": 15,
                "statistics": {
                    "unit": "picoseconds_per_operation",
                    "minimum": 900,
                    "median": 1_000,
                    "maximum": 1_100,
                    "median_absolute_deviation": 20,
                    "relative_mad_ppm": 20_000,
                    "relative_range_ppm": 200_000,
                    "median_bytes_per_second": 3_072_000_000_000,
                },
                "correctness": {
                    "status": "passed",
                    "result_digest_blake3": DIGEST,
                    "reference_digest_blake3": DIGEST,
                },
                "status": "stable",
            }
        ],
        "selected_kernels": [
            {
                "primitive": "vector-dot",
                "input_size": 384,
                "input_unit": "dimensions",
                "variant": "portable-iterator-f64",
                "reason": "candidate passed correctness and variance policy",
            }
        ],
        "thread_scaling": {
            "binding": "unbound",
            "physical_core_boundary": 1,
            "logical_processor_boundary": 1,
            "measured_thread_counts": [],
            "status": "unavailable",
            "physical_peak_threads": None,
            "physical_peak_bytes_per_second": None,
            "smt_peak_threads": None,
            "smt_peak_bytes_per_second": None,
            "smt_to_physical_throughput_ppm": None,
            "smt_recommended": False,
            "recommended_worker_count": None,
            "recommendation": "no scaling cells in semantic fixture",
        },
        "io_scaling": {
            "binding": "buffered-sync-workers",
            "measured_queue_depths": [],
            "status": "unavailable",
            "peak_queue_depth": None,
            "peak_bytes_per_second": None,
            "recommended_io_slots": None,
            "recommendation": "no I/O scaling cells in semantic fixture",
        },
        "coverage": {
            "measured": ["vector-dot"],
            "unsupported": [
                {"primitive": "thread-scaling", "reason": "not implemented"},
                {
                    "primitive": "numa-local-remote-memory",
                    "reason": "single-node fixture",
                },
            ],
        },
        "claims": [],
    }


class CalibrationCheckerTests(unittest.TestCase):
    def test_accepts_semantically_consistent_receipt(self) -> None:
        validate_receipt(valid_receipt())

    def test_accepts_unstable_durability_diagnostic_for_scheduling(self) -> None:
        receipt = valid_receipt()
        diagnostic = copy.deepcopy(receipt["measurements"][0])
        diagnostic.update(
            {
                "primitive": "native-wal-group-flush",
                "variant": "native-eight-record-group-sync-data",
                "input_size": 8,
                "input_unit": "records",
                "bytes_per_operation": 65_536,
                "operations_per_sample": 1,
                "maximum_operations_per_sample": 1,
                "status": "unstable",
            }
        )
        diagnostic["statistics"].update(
            {
                "minimum": 100_000,
                "median": 200_000,
                "maximum": 900_000,
                "median_absolute_deviation": 100_000,
                "relative_mad_ppm": 500_000,
                "relative_range_ppm": 4_000_000,
                "median_bytes_per_second": 327_680_000,
            }
        )
        receipt["measurements"].append(diagnostic)
        receipt["coverage"]["measured"].append("native-wal-group-flush")
        receipt["coverage"]["measured"].sort()

        validate_receipt(receipt)

    def test_rejects_unstable_scheduler_input(self) -> None:
        receipt = valid_receipt()
        scheduler_input = copy.deepcopy(receipt["measurements"][0])
        scheduler_input.update(
            {
                "primitive": "queue-depth-random-read",
                "variant": "persistent-sync-workers-buffered-4k",
                "input_size": 16,
                "input_unit": "outstanding-reads",
                "status": "unstable",
            }
        )
        scheduler_input["statistics"].update(
            {
                "minimum": 100,
                "median": 200,
                "maximum": 900,
                "median_absolute_deviation": 100,
                "relative_mad_ppm": 500_000,
                "relative_range_ppm": 4_000_000,
            }
        )
        receipt["measurements"].append(scheduler_input)
        receipt["coverage"]["measured"].append("queue-depth-random-read")
        receipt["coverage"]["measured"].sort()

        with self.assertRaisesRegex(CalibrationValidationError, "scheduler variance"):
            validate_receipt(receipt)

    def test_rejects_selection_when_receipt_is_unstable(self) -> None:
        receipt = valid_receipt()
        measurement = receipt["measurements"][0]
        measurement["primitive"] = "thread-scaling-memory-scan"
        receipt["coverage"]["measured"] = ["thread-scaling-memory-scan"]
        measurement["statistics"]["relative_range_ppm"] = 600_000
        measurement["status"] = "unstable"
        receipt["status"] = "unstable"
        receipt["accepted_for_scheduling"] = False
        with self.assertRaisesRegex(CalibrationValidationError, "selected_kernels"):
            validate_receipt(receipt)

    def test_rejects_false_correctness_status(self) -> None:
        receipt = valid_receipt()
        receipt["measurements"][0]["correctness"]["reference_digest_blake3"] = "2" * 64
        with self.assertRaisesRegex(CalibrationValidationError, "correctness status"):
            validate_receipt(receipt)

    def test_rejects_sample_count_not_bound_to_policy(self) -> None:
        receipt = valid_receipt()
        receipt["measurements"][0]["sample_count"] = 14
        with self.assertRaisesRegex(CalibrationValidationError, "differs from policy"):
            validate_receipt(receipt)

    def test_rejects_inner_batch_above_recorded_cap(self) -> None:
        receipt = valid_receipt()
        receipt["measurements"][0]["maximum_operations_per_sample"] = 9_999
        with self.assertRaisesRegex(CalibrationValidationError, "hard limit"):
            validate_receipt(receipt)

    def test_rejects_out_of_window_receipt_claiming_stable(self) -> None:
        receipt = valid_receipt()
        receipt["elapsed_ms"] = 20_000
        with self.assertRaisesRegex(CalibrationValidationError, "accepted_for_scheduling"):
            validate_receipt(receipt)

    def test_rejects_performance_claims(self) -> None:
        receipt = copy.deepcopy(valid_receipt())
        receipt["claims"] = ["microsecond-first"]
        with self.assertRaisesRegex(CalibrationValidationError, "cannot carry"):
            validate_receipt(receipt)

    def test_rejects_measured_unsupported_overlap(self) -> None:
        receipt = valid_receipt()
        receipt["coverage"]["unsupported"].append(
            {"primitive": "vector-dot", "reason": "contradictory fixture"}
        )
        with self.assertRaisesRegex(CalibrationValidationError, "measured and unsupported"):
            validate_receipt(receipt)

    def test_rejects_thread_scaling_recommendation_without_curve(self) -> None:
        receipt = valid_receipt()
        receipt["thread_scaling"]["status"] = "stable"
        receipt["thread_scaling"]["recommended_worker_count"] = 1
        with self.assertRaisesRegex(CalibrationValidationError, "status must be unavailable"):
            validate_receipt(receipt)

    def test_rejects_io_scaling_recommendation_without_curve(self) -> None:
        receipt = valid_receipt()
        receipt["io_scaling"]["status"] = "stable"
        receipt["io_scaling"]["recommended_io_slots"] = 4
        with self.assertRaisesRegex(CalibrationValidationError, "status must be unavailable"):
            validate_receipt(receipt)

    def test_accepts_complete_directed_numa_matrix(self) -> None:
        receipt = valid_receipt()
        template = receipt["measurements"][0]
        cells = []
        for variant in (
            "linux-first-touch-node-0-read-node-0-cpu-0",
            "linux-first-touch-node-0-read-node-1-cpu-48",
            "linux-first-touch-node-1-read-node-0-cpu-0",
            "linux-first-touch-node-1-read-node-1-cpu-48",
        ):
            cell = copy.deepcopy(template)
            cell.update(
                {
                    "primitive": "numa-memory-read",
                    "variant": variant,
                    "input_size": 8 * 1024 * 1024,
                    "input_unit": "working-set-bytes",
                    "bytes_per_operation": 8 * 1024 * 1024,
                }
            )
            cells.append(cell)
            receipt["selected_kernels"].append(
                {
                    "primitive": cell["primitive"],
                    "input_size": cell["input_size"],
                    "input_unit": cell["input_unit"],
                    "variant": cell["variant"],
                    "reason": "candidate passed correctness and variance policy",
                }
            )
        receipt["measurements"].extend(cells)
        receipt["coverage"]["measured"] = ["numa-memory-read", "vector-dot"]
        receipt["coverage"]["unsupported"] = [
            item
            for item in receipt["coverage"]["unsupported"]
            if item["primitive"] != "numa-local-remote-memory"
        ]
        with self.assertRaisesRegex(CalibrationValidationError, "page-residency"):
            validate_receipt(receipt)
        receipt["measurements"][-1]["statistics"]["unit"] = "nanoseconds"
        with self.assertRaisesRegex(CalibrationValidationError, "unit is not canonical"):
            validate_receipt(receipt)

    def test_rejects_incomplete_directed_numa_matrix(self) -> None:
        receipt = valid_receipt()
        template = receipt["measurements"][0]
        for source, reader, cpu in ((0, 0, 0), (0, 1, 48), (1, 1, 48)):
            cell = copy.deepcopy(template)
            cell.update(
                {
                    "primitive": "numa-memory-read",
                    "variant": (
                        f"linux-first-touch-node-{source}-read-node-{reader}-cpu-{cpu}"
                    ),
                    "input_size": 8 * 1024 * 1024,
                    "input_unit": "working-set-bytes",
                    "bytes_per_operation": 8 * 1024 * 1024,
                }
            )
            receipt["measurements"].append(cell)
            receipt["selected_kernels"].append(
                {
                    "primitive": cell["primitive"],
                    "input_size": cell["input_size"],
                    "input_unit": cell["input_unit"],
                    "variant": cell["variant"],
                    "reason": "candidate passed correctness and variance policy",
                }
            )
        receipt["coverage"]["measured"] = ["numa-memory-read", "vector-dot"]
        receipt["coverage"]["unsupported"] = []
        with self.assertRaisesRegex(CalibrationValidationError, "complete directed"):
            validate_receipt(receipt)


if __name__ == "__main__":
    unittest.main()
