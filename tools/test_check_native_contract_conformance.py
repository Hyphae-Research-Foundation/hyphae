#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import json
import unittest
from pathlib import Path

from tools.check_native_contract_conformance import GateFailure, SURFACES, validate_inventory

ROOT = Path(__file__).resolve().parent.parent
INVENTORY = ROOT / "config/native-contract-conformance.json"


class NativeContractConformanceTests(unittest.TestCase):
    def inventory(self) -> dict:
        return json.loads(INVENTORY.read_text(encoding="utf-8"))

    def test_checked_in_inventory_covers_exact_g0_contract_surfaces(self) -> None:
        surfaces = validate_inventory(ROOT, self.inventory())
        self.assertEqual({entry["id"] for entry in surfaces}, SURFACES)
        self.assertGreaterEqual(sum(len(entry["tests"]) for entry in surfaces), 10)

    def test_missing_surface_contract_or_test_fails_closed(self) -> None:
        inventory = self.inventory()
        inventory["surfaces"].pop()
        with self.assertRaisesRegex(GateFailure, "exact required surfaces"):
            validate_inventory(ROOT, inventory)
        inventory = self.inventory()
        inventory["surfaces"][0]["contract"] = "docs/native/missing.md"
        with self.assertRaisesRegex(GateFailure, "missing contract"):
            validate_inventory(ROOT, inventory)
        inventory = self.inventory()
        inventory["surfaces"][0]["tests"] = []
        with self.assertRaisesRegex(GateFailure, "tests required"):
            validate_inventory(ROOT, inventory)

    def test_duplicate_or_unlocked_commands_fail_closed(self) -> None:
        inventory = self.inventory()
        duplicate = inventory["surfaces"][0]["tests"][0]
        inventory["surfaces"][1]["tests"][0] = duplicate
        with self.assertRaisesRegex(GateFailure, "unique"):
            validate_inventory(ROOT, inventory)
        inventory = self.inventory()
        inventory["surfaces"][0]["tests"][0] = "cargo test -p hyphae-native-runtime"
        with self.assertRaisesRegex(GateFailure, "--locked"):
            validate_inventory(ROOT, inventory)


if __name__ == "__main__":
    unittest.main()
