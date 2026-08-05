#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class NativeG2InventoryTests(unittest.TestCase):
    def test_inventory_matches_profile_and_never_credits_partial_work(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        profile = json.loads((ROOT / "config/native-g2-readiness-profile.json").read_text())
        self.assertEqual(
            [row["id"] for row in inventory["requirements"]],
            [row["id"] for row in profile["requirements"]],
        )
        allowed = {"implemented-unhosted", "partial", "missing"}
        self.assertEqual(len(inventory["requirements"]), 8)
        for row in inventory["requirements"]:
            self.assertIn(row["status"], allowed)
            self.assertTrue(row["gaps"])

    def test_benchmark_and_conformance_gaps_are_explicitly_missing(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        rows = {row["id"]: row for row in inventory["requirements"]}
        for identifier in (
            "sqllogictest-conformance",
            "metamorphic-sql-equivalence",
            "tpch-correctness",
            "tpcc-acid",
        ):
            self.assertEqual(rows[identifier]["status"], "missing")

    def test_ctes_windows_and_constraint_gaps_are_not_omitted(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        text = json.dumps(inventory)
        for gap in ("CTEs", "window functions", "CHECK constraints", "FOREIGN KEY"):
            self.assertIn(gap, text)


if __name__ == "__main__":
    unittest.main()
