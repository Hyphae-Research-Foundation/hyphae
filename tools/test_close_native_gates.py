#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.close_native_gates import GateFailure, close


class NativeGateClosureTests(unittest.TestCase):
    def test_closes_complete_exact_gate_aggregates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for gate, required in (("G2", 8), ("G3", 11)):
                path = root / f"{gate}.json"
                path.write_text(json.dumps({"gate": gate, "status": "passed", "required": required, "passed": required}))
                paths.append(path)
            result = close({"schema": "hyphae-native-gate-status-v1", "gates": [{"id": "G2"}, {"id": "G3"}]}, "a" * 40, *paths)
            self.assertEqual([row["status"] for row in result["gates"]], ["closed", "closed"])

    def test_rejects_incomplete_aggregate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            g2 = root / "g2.json"
            g3 = root / "g3.json"
            g2.write_text(json.dumps({"gate": "G2", "status": "passed", "required": 8, "passed": 7}))
            g3.write_text(json.dumps({"gate": "G3", "status": "passed", "required": 11, "passed": 11}))
            with self.assertRaises(GateFailure):
                close({"schema": "hyphae-native-gate-status-v1", "gates": [{"id": "G2"}, {"id": "G3"}]}, "a" * 40, g2, g3)


if __name__ == "__main__":
    unittest.main()
