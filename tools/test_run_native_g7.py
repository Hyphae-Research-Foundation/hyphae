#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.run_native_g7 import (
    parse_macos_counter_export,
    validate_completed_ann_progress,
    write_matrix_progress,
)


class NativeG7ControllerTests(unittest.TestCase):
    SOURCE_COMMIT = "1" * 40

    @classmethod
    def completed_progress(cls) -> dict[str, object]:
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


if __name__ == "__main__":
    unittest.main()
