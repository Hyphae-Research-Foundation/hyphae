#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.run_native_g7 import parse_macos_counter_export


class NativeG7ControllerTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
