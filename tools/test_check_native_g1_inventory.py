#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class NativeG1InventoryTests(unittest.TestCase):
    def test_inventory_matches_readiness_and_stays_honest(self) -> None:
        inventory = json.loads((ROOT / "config/native-g1-inventory.json").read_text())
        profile = json.loads((ROOT / "config/native-g1-readiness-profile.json").read_text())
        self.assertEqual(inventory["schema"], "hyphae-native-g1-inventory-v1")
        self.assertEqual(
            [row["id"] for row in inventory["requirements"]],
            [row["id"] for row in profile["requirements"]],
        )
        self.assertEqual(len(inventory["requirements"]), 7)
        allowed = {"implemented-unhosted", "partial", "missing"}
        for row in inventory["requirements"]:
            self.assertIn(row["status"], allowed)
            self.assertTrue(row["commands"])
            for command in row["commands"]:
                self.assertIn("--locked", command)
            if row["status"] != "implemented-unhosted":
                self.assertTrue(row.get("gap"))

    def test_native_runtime_dependency_closure_excludes_redb(self) -> None:
        manifest = (ROOT / "crates/hyphae-native-runtime/Cargo.toml").read_text()
        self.assertNotIn("redb", manifest.lower())
        self.assertIn("hyphae-native-pages.workspace = true", manifest)
        self.assertIn("hyphae-native-wal.workspace = true", manifest)
        self.assertIn("hyphae-native-catalog.workspace = true", manifest)


if __name__ == "__main__":
    unittest.main()
