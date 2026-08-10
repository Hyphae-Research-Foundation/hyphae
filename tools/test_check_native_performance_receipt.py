#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import copy
import unittest

from tools.check_native_performance_receipt import (
    GateFailure,
    profile_digest,
    suite_profile_digest,
    validate_progress,
    validate_receipt,
    validate_suite,
)


COMMIT = "a" * 40
TREE = "b" * 40
DATASET = "c" * 64
HARDWARE = "d" * 64
BINARY = "e" * 64
PARAMETERS = "f" * 64


def counter(unit: str, value: int = 1) -> dict:
    return {
        "status": "measured",
        "value": value,
        "unit": unit,
        "provider": "test-provider",
        "reason": None,
    }


def receipt() -> dict:
    components = {
        "admission": 10,
        "queueing": 20,
        "parse_bind_plan": 30,
        "engine_execution": 100,
        "cross_engine_fusion": 0,
        "wal_append": 0,
        "physical_synchronization": 0,
        "transport": 0,
        "result_proof_encoding": 30,
        "unattributed": 10,
    }
    return {
        "schema": "hyphae-native-performance-receipt-v1",
        "status": "passed",
        "evidence_class": "diagnostic-baseline",
        "source": {
            "commit": COMMIT,
            "tree": TREE,
            "binary_sha256": BINARY,
            "profile_sha256": profile_digest(),
            "clean": False,
        },
        "environment": {
            "platform": "linux",
            "target": "x86_64-unknown-linux-gnu",
            "os": "Linux-test",
            "compiler": "rustc test",
            "build_profile": "release",
            "hardware_fingerprint": HARDWARE,
            "dedicated": False,
            "virtualization": "kvm",
            "topology": "1 socket, 4 physical cores, 8 hardware threads",
            "affinity": "0-7",
        },
        "workload": {
            "class": "foreground-point",
            "engines": ["structures"],
            "operation": "embedded-structure-point-get",
            "parameters_sha256": PARAMETERS,
        },
        "dataset": {
            "source_commit": COMMIT,
            "generator": "native-performance-test-v1",
            "digest": DATASET,
            "records": 2_048,
            "bytes": 131_072,
        },
        "measurement": {
            "observations": 1_000_000,
            "warmup": 100_000,
            "concurrency": 1,
            "state": "warm",
            "background_mode": "control",
            "elapsed_nanos": 200,
            "clock_totals_nanos": components,
        },
        "counters": {
            "cpu_time": counter("nanoseconds"),
            "cpu_cycles": counter("cycles"),
            "instructions": counter("count"),
            "cache_misses": counter("count"),
            "context_switches": counter("count"),
            "page_faults": counter("count"),
            "allocations": counter("count"),
            "peak_rss": counter("bytes"),
            "bytes_read": counter("bytes"),
            "bytes_written": counter("bytes"),
        },
        "correctness": {
            "status": "passed",
            "oracle": "native-structure-model-v1",
            "result_digest": "1" * 64,
        },
        "claims": [],
        "closure_declared": False,
    }


def progress() -> dict:
    return {
        "schema": "hyphae-native-performance-progress-v1",
        "source_commit": COMMIT,
        "source_tree": TREE,
        "dataset_digest": DATASET,
        "operation": "ann-bulk-build",
        "stage": "candidate-construction",
        "sequence": 2,
        "completed_units": 512,
        "total_units": 1_024,
        "unit": "vectors",
        "elapsed_nanos": 10_000,
        "status": "running",
        "checkpoint_digest": None,
    }


def suite(*receipts: dict) -> dict:
    first = receipts[0]
    return {
        "schema": "hyphae-native-performance-suite-v1",
        "status": "passed",
        "suite_profile_sha256": suite_profile_digest(),
        "source_commit": first["source"]["commit"],
        "source_tree": first["source"]["tree"],
        "binary_sha256": first["source"]["binary_sha256"],
        "clean": first["source"]["clean"],
        "hardware_fingerprint": first["environment"]["hardware_fingerprint"],
        "receipts": list(receipts),
        "claims": [],
        "closure_declared": False,
    }


class NativePerformanceReceiptTests(unittest.TestCase):
    def test_accepts_diagnostic_baseline(self) -> None:
        audit = validate_receipt(receipt(), COMMIT)
        self.assertEqual(audit["status"], "passed")
        self.assertEqual(audit["evidence_class"], "diagnostic-baseline")

    def test_rejects_non_exhaustive_clock_decomposition(self) -> None:
        payload = receipt()
        payload["measurement"]["clock_totals_nanos"]["unattributed"] = 9
        with self.assertRaisesRegex(GateFailure, "clock decomposition"):
            validate_receipt(payload, COMMIT)

    def test_rejects_unknown_clock_component(self) -> None:
        payload = receipt()
        payload["measurement"]["clock_totals_nanos"]["other"] = 0
        with self.assertRaisesRegex(GateFailure, "clock components"):
            validate_receipt(payload, COMMIT)

    def test_rejects_unsupported_counter_encoded_as_zero(self) -> None:
        payload = receipt()
        payload["counters"]["cpu_cycles"] = {
            "status": "unsupported",
            "value": 0,
            "unit": "cycles",
            "provider": "none",
            "reason": "hardware counters unavailable",
        }
        with self.assertRaisesRegex(GateFailure, "unsupported counter"):
            validate_receipt(payload, COMMIT)

    def test_diagnostic_baseline_accepts_explicitly_unsupported_counter(self) -> None:
        payload = receipt()
        payload["counters"]["cpu_cycles"] = {
            "status": "unsupported",
            "value": None,
            "unit": "cycles",
            "provider": "none",
            "reason": "hardware counters unavailable",
        }
        self.assertEqual(validate_receipt(payload, COMMIT)["status"], "passed")

    def test_qualification_requires_dedicated_physical_host_and_counters(self) -> None:
        payload = receipt()
        payload["evidence_class"] = "qualification-candidate"
        with self.assertRaisesRegex(GateFailure, "qualification environment"):
            validate_receipt(payload, COMMIT)

        payload["environment"]["dedicated"] = True
        payload["environment"]["virtualization"] = "none"
        payload["source"]["clean"] = True
        payload["counters"]["cpu_cycles"] = {
            "status": "unsupported",
            "value": None,
            "unit": "cycles",
            "provider": "none",
            "reason": "hardware counters unavailable",
        }
        with self.assertRaisesRegex(GateFailure, "qualification counters"):
            validate_receipt(payload, COMMIT)

    def test_rejects_seed_from_another_source(self) -> None:
        payload = receipt()
        payload["dataset"]["source_commit"] = "2" * 40
        with self.assertRaisesRegex(GateFailure, "dataset source"):
            validate_receipt(payload, COMMIT)

    def test_qualification_rejects_dirty_source(self) -> None:
        payload = receipt()
        payload["evidence_class"] = "qualification-candidate"
        payload["environment"]["dedicated"] = True
        payload["environment"]["virtualization"] = "none"
        with self.assertRaisesRegex(GateFailure, "qualification source"):
            validate_receipt(payload, COMMIT)

    def test_rejects_wrong_profile_digest(self) -> None:
        payload = receipt()
        payload["source"]["profile_sha256"] = "2" * 64
        with self.assertRaisesRegex(GateFailure, "profile"):
            validate_receipt(payload, COMMIT)

    def test_accepts_complete_homogeneous_suite(self) -> None:
        audit = validate_suite(suite(receipt()), COMMIT)
        self.assertEqual(audit["cells"], 1)
        self.assertEqual(audit["status"], "passed")

    def test_rejects_missing_or_duplicate_suite_cells(self) -> None:
        missing = suite(receipt())
        missing["receipts"][0]["workload"]["operation"] = "different-operation"
        with self.assertRaisesRegex(GateFailure, "1 missing, 1 extra"):
            validate_suite(missing, COMMIT)

        duplicate = suite(receipt(), copy.deepcopy(receipt()))
        with self.assertRaisesRegex(GateFailure, "duplicate cells"):
            validate_suite(duplicate, COMMIT)

    def test_rejects_cross_hardware_suite(self) -> None:
        payload = suite(receipt())
        payload["hardware_fingerprint"] = "2" * 64
        with self.assertRaisesRegex(GateFailure, "hardware identity"):
            validate_suite(payload, COMMIT)

    def test_rejects_changed_dataset_across_suite_cells(self) -> None:
        second = copy.deepcopy(receipt())
        second["measurement"]["concurrency"] = 8
        second["dataset"]["digest"] = "2" * 64
        payload = suite(receipt(), second)
        with self.assertRaisesRegex(GateFailure, "dataset identity"):
            validate_suite(payload, COMMIT)

    def test_accepts_monotonic_progress(self) -> None:
        audit = validate_progress(progress(), COMMIT)
        self.assertEqual(audit["completed_units"], 512)
        self.assertEqual(audit["status"], "running")

    def test_rejects_progress_past_total(self) -> None:
        payload = progress()
        payload["completed_units"] = 1_025
        with self.assertRaisesRegex(GateFailure, "progress bounds"):
            validate_progress(payload, COMMIT)

    def test_completed_progress_requires_all_units_and_checkpoint(self) -> None:
        payload = progress()
        payload["status"] = "completed"
        with self.assertRaisesRegex(GateFailure, "completed progress"):
            validate_progress(payload, COMMIT)

        payload["completed_units"] = payload["total_units"]
        payload["checkpoint_digest"] = "3" * 64
        self.assertEqual(validate_progress(payload, COMMIT)["status"], "completed")

    def test_rejects_progress_regression(self) -> None:
        previous = progress()
        current = copy.deepcopy(previous)
        current["sequence"] += 1
        current["completed_units"] -= 1
        with self.assertRaisesRegex(GateFailure, "not monotonic"):
            validate_progress(current, COMMIT, previous)


if __name__ == "__main__":
    unittest.main()
