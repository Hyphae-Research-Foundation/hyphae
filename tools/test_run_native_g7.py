#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_performance_receipt import validate_progress
from tools.run_native_g7 import (
    ProgressWatchdog,
    parse_macos_counter_export,
    validate_completed_ann_progress,
    validate_initial_ann_bulk_evidence,
    write_matrix_progress,
)


class NativeG7ControllerTests(unittest.TestCase):
    SOURCE_COMMIT = "1" * 40

    @classmethod
    def initial_ann_bulk(cls) -> dict[str, object]:
        return {
            "schema": "hyphae-native-g7-initial-ann-bulk-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "dataset_digest": "3" * 64,
            "builder": "partitioned-hnsw-v1",
            "input_identity": "4" * 64,
            "aggregate_identity": "5" * 64,
            "planned_vectors": 1_000_000,
            "planned_partitions": 48,
            "planned_workers": 44,
            "planned_memory_bytes": 4_000_000_000,
            "worker_batches": 48,
            "total_time_nanos": 1,
            "hardware_profile_fingerprint": "6" * 64,
            "governor_policy_schema": "hyphae-native-governor-policy-v1",
            "governor_mode": "mixed",
            "calibration_cache_key": "test-calibration",
            "topology_digest": "7" * 64,
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
        }

    @classmethod
    def completed_progress(cls) -> dict[str, object]:
        details = cls.initial_ann_bulk()
        details["eta"] = {
            "status": "completed",
            "estimated_remaining_nanos": 0,
        }
        return {
            "schema": "hyphae-native-performance-progress-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "source_tree": "2" * 40,
            "dataset_digest": "3" * 64,
            "operation": "ann-bulk-build",
            "stage": "ann-published",
            "sequence": 4,
            "completed_units": 1_000_000,
            "total_units": 1_000_000,
            "unit": "vectors",
            "elapsed_nanos": 10,
            "status": "completed",
            "checkpoint_digest": "4" * 64,
            "details": details,
        }

    def test_parses_macos_counter_rows_and_references(self) -> None:
        document = """<?xml version="1.0"?>
<trace-query-result><node><schema name="MetricTable"/>
<row><string id="1">Cycles</string><fixed-decimal id="2">120.0</fixed-decimal></row>
<row><string id="3">L1D Cache Load Misses</string><fixed-decimal id="4">7.0</fixed-decimal></row>
<row><string id="5">L1D Cache Store Misses</string><fixed-decimal id="6">3.0</fixed-decimal></row>
<row><string ref="1"/><fixed-decimal ref="2"/></row>
</node></trace-query-result>
"""
        with tempfile.TemporaryDirectory() as directory:
            export = Path(directory) / "metrics.xml"
            export.write_text(document, encoding="utf-8")
            counters = parse_macos_counter_export(export)
        self.assertEqual(counters, {"cpu_cycles": 240, "cache_misses": 10})

    def test_rejects_export_without_cycles(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            export = Path(directory) / "metrics.xml"
            export.write_text("<trace-query-result/>", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "did not contain"):
                parse_macos_counter_export(export)

    def test_accepts_durably_published_ann_progress(self) -> None:
        validate_completed_ann_progress(self.completed_progress(), self.SOURCE_COMMIT)

    def test_rejects_progress_that_stops_before_publication(self) -> None:
        progress = self.completed_progress()
        progress["stage"] = "ann-publication"
        progress["status"] = "running"
        progress["checkpoint_digest"] = None
        with self.assertRaisesRegex(RuntimeError, "durable publication"):
            validate_completed_ann_progress(progress, self.SOURCE_COMMIT)

    def test_rejects_parallel_bulk_without_multiple_worker_batches(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["worker_batches"] = 1
        with self.assertRaisesRegex(RuntimeError, "parallel worker batches"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_accepts_compute_only_initial_bulk_governor_request(self) -> None:
        evidence = self.initial_ann_bulk()
        self.assertEqual(evidence["governor_execution"]["io_slots"], 0)
        validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_initial_bulk_governor_resource_mismatch(self) -> None:
        for field, value in (("compute_threads", 43), ("memory_bytes", 3_999_999_999)):
            with self.subTest(field=field):
                evidence = self.initial_ann_bulk()
                evidence["governor_execution"][field] = value
                with self.assertRaisesRegex(RuntimeError, "governor execution"):
                    validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_negative_initial_bulk_governor_resources(self) -> None:
        for field in ("compute_threads", "io_slots", "memory_bytes"):
            with self.subTest(field=field):
                evidence = self.initial_ann_bulk()
                evidence["governor_execution"][field] = -1
                with self.assertRaisesRegex(RuntimeError, "governor execution"):
                    validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_invented_initial_bulk_io_reservation(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["governor_execution"]["io_slots"] = 1
        with self.assertRaisesRegex(RuntimeError, "governor execution"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_bulk_above_durable_partition_limit(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["planned_partitions"] = 222
        evidence["topology_workers"] = 222
        with self.assertRaisesRegex(RuntimeError, "partition limit"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_progress_details_for_another_dataset(self) -> None:
        progress = self.completed_progress()
        progress["details"]["dataset_digest"] = "8" * 64
        with self.assertRaisesRegex(RuntimeError, "another dataset"):
            validate_completed_ann_progress(progress, self.SOURCE_COMMIT)

    def test_stage_progress_preserves_vector_identity(self) -> None:
        previous = self.completed_progress()
        previous.update({
            "stage": "ann-private-build",
            "sequence": 1,
            "completed_units": 0,
            "status": "running",
            "checkpoint_digest": None,
            "details": {
                "builder": "partitioned-hnsw-v1",
                "eta": {
                    "status": "pending",
                    "estimated_remaining_nanos": None,
                },
            },
        })
        current = dict(previous)
        current.update({
            "stage": "ann-child-build",
            "sequence": 2,
            "completed_units": 250_000,
            "elapsed_nanos": 20,
            "details": {
                "builder": "partitioned-hnsw-v1",
                "stage_completed": 1,
                "stage_total": 4,
                "eta": {
                    "status": "estimated",
                    "estimated_remaining_nanos": 60,
                },
            },
        })
        validate_progress(current, self.SOURCE_COMMIT, previous)

    def test_matrix_progress_is_atomic_diagnostic_and_exact_sha(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "matrix.progress.json"
            completed = [{
                "state": "warm",
                "background_mode": "control",
                "concurrency": 1,
            }]
            current = {
                "state": "warm",
                "background_mode": "control",
                "concurrency": 8,
            }
            write_matrix_progress(
                path,
                self.SOURCE_COMMIT,
                "linux",
                completed,
                6,
                current,
                "running",
                1,
            )
            progress = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(progress["source_commit"], self.SOURCE_COMMIT)
            self.assertEqual(progress["completed_count"], 1)
            self.assertEqual(progress["total_cells"], 6)
            self.assertEqual(progress["current_cell"], current)
            self.assertEqual(progress["status"], "running")
            self.assertFalse(list(path.parent.glob("*.tmp")))

    def test_progress_watchdog_fails_after_configured_stall(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=10.0)
            watchdog.observe(14.9)
            with self.assertRaisesRegex(RuntimeError, "stalled for 5s"):
                watchdog.observe(15.0)

    def test_progress_watchdog_stall_reports_last_stage_progress_and_eta(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=5.0)
            path.write_text(json.dumps({
                "sequence": 7,
                "status": "running",
                "stage": "ann-child-build",
                "completed_units": 250_000,
                "total_units": 1_000_000,
                "details": {
                    "eta": {
                        "status": "estimated",
                        "estimated_remaining_nanos": 123,
                    },
                },
            }))
            watchdog.observe(10.0)
            with self.assertRaises(RuntimeError) as context:
                watchdog.observe(15.0)
            message = str(context.exception)
            self.assertIn("stage='ann-child-build'", message)
            self.assertIn("completed=250000/1000000", message)
            self.assertIn(
                'eta={"estimated_remaining_nanos":123,"status":"estimated"}',
                message,
            )

    def test_progress_watchdog_tracks_sequence_and_stops_after_completion(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=10.0)
            path.write_text(json.dumps({"sequence": 1, "status": "running"}))
            watchdog.observe(14.0)
            path.write_text(json.dumps({"sequence": 2, "status": "completed"}))
            watchdog.observe(18.0)
            watchdog.observe(100.0)


if __name__ == "__main__":
    unittest.main()
