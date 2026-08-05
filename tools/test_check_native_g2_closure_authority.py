#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class NativeG2ClosureAuthorityTests(unittest.TestCase):
    def test_inventory_cannot_mark_benchmark_rows_ready_with_unsupported_matrix(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        rows = {row["id"]: row for row in inventory["requirements"]}
        matrix = json.loads(
            (ROOT / "crates/hyphae-native-runtime/tests/corpus/g2-tpch-matrix.json").read_text()
        )
        unsupported = sum(row["status"] == "unsupported" for row in matrix["queries"])
        self.assertEqual(unsupported, 21)
        self.assertNotEqual(rows["tpch-correctness"]["status"], "implemented-unhosted")

    def test_complete_g2_contract_retains_hard_engine_gaps(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        text = json.dumps(inventory)
        for gap in (
            "ALTER TABLE",
            "DROP TABLE",
            "CHECK constraints",
            "FOREIGN KEY constraints",
            "complete isolation litmus corpus",
            "complete join semantics and algorithms",
            "canonical full-column TPC-C schema",
        ):
            self.assertIn(gap, text)

    def test_readiness_profile_has_no_bounded_escape_hatch(self) -> None:
        profile = json.loads((ROOT / "config/native-g2-readiness-profile.json").read_text())
        self.assertEqual(len(profile["requirements"]), 8)
        self.assertTrue(all(row["required_evidence"] == "hosted" for row in profile["requirements"]))
        self.assertNotIn("optional", json.dumps(profile).lower())


if __name__ == "__main__":
    unittest.main()
