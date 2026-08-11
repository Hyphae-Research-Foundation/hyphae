#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import json
import plistlib
import tempfile
import unittest
from pathlib import Path

from tools.prepare_native_g7_macos_template import prepare


class NativeG7MacosTemplateTests(unittest.TestCase):
    def test_rewrites_exact_counter_setting(self) -> None:
        settings = {
            "allEventsAndFormulas": [],
            "selectedCountingMode": {
                "analysisMode": "bottleneck",
                "countingMode": "bottlenecks",
            },
            "selectedCountingModeDisplayName": "CPU Bottlenecks",
        }
        archive = {
            "$archiver": "NSKeyedArchiver",
            "$objects": ["$null", json.dumps(settings).encode("utf-8")],
            "$top": {},
            "$version": 100000,
        }
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.tracetemplate"
            output = Path(directory) / "output.tracetemplate"
            source.write_bytes(plistlib.dumps(archive, fmt=plistlib.FMT_BINARY))
            prepare(source, output)
            objects = plistlib.loads(output.read_bytes())["$objects"]
        result = json.loads(objects[1].decode("utf-8"))
        self.assertEqual(
            result["selectedCountingMode"],
            {"analysisMode": "bottleneck", "countingMode": "l1d_miss_sampling"},
        )
        self.assertEqual(result["selectedCountingModeDisplayName"], "L1D Miss Sampling")

    def test_rejects_ambiguous_settings(self) -> None:
        archive = {
            "$objects": [
                '$null',
                b'{"allEventsAndFormulas":[]}',
                b'{"allEventsAndFormulas":[]}',
            ]
        }
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.tracetemplate"
            output = Path(directory) / "output.tracetemplate"
            source.write_bytes(plistlib.dumps(archive, fmt=plistlib.FMT_BINARY))
            with self.assertRaisesRegex(ValueError, "one counter setting"):
                prepare(source, output)


if __name__ == "__main__":
    unittest.main()
