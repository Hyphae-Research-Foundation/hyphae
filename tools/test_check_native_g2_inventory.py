#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

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

    def test_all_rows_are_implemented_unhosted_but_remain_open(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        self.assertEqual(
            {row["status"] for row in inventory["requirements"]},
            {"implemented-unhosted"},
        )
        for row in inventory["requirements"]:
            self.assertTrue(any("hosted exact-SHA" in gap for gap in row["gaps"]))

    def test_benchmark_non_claims_remain_explicit(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        text = json.dumps(inventory)
        for gap in (
            "21 unsupported canonical queries",
            "canonical full-column TPC-C schema",
            "serializable/write-skew",
            "recursive CTEs",
        ):
            self.assertIn(gap, text)


if __name__ == "__main__":
    unittest.main()
