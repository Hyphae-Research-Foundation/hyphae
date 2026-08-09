from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_goldens import (
    GateFailure,
    validate_completeness,
    validate_inventory,
)


ROOT = Path(__file__).resolve().parents[1]


class NativeGoldenInventoryTests(unittest.TestCase):
    def test_checked_in_inventory_resolves_exact_producers_tests_and_consumers(self) -> None:
        inventory = json.loads(
            (ROOT / "config/native-golden-inventory.json").read_text(encoding="utf-8")
        )

        result = validate_inventory(ROOT, inventory)

        self.assertEqual(result["status"], "passed")
        self.assertRegex(result["source_commit"], r"^[0-9a-f]{40}$")
        self.assertEqual(result["fixture_count"], 10)
        self.assertEqual(result["fixture_count"], len(result["fixtures"]))

    def test_checked_in_requirements_match_the_inventory_exactly(self) -> None:
        inventory = json.loads(
            (ROOT / "config/native-golden-inventory.json").read_text(encoding="utf-8")
        )
        requirements = json.loads(
            (ROOT / "config/native-golden-requirements.json").read_text(encoding="utf-8")
        )

        result = validate_completeness(requirements, inventory)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["required_count"], 10)
        self.assertEqual(result["inventoried_count"], 10)

    def test_missing_stale_and_duplicate_requirements_fail_closed(self) -> None:
        inventory = {
            "schema": "hyphae-native-golden-inventory-v1",
            "fixtures": [
                {"id": "one", "producer": "p", "test": "t", "consumer": "c"},
                {"id": "two", "producer": "p", "test": "t", "consumer": "c"},
            ],
        }
        with self.assertRaisesRegex(GateFailure, "missing required golden"):
            validate_completeness(
                {
                    "schema": "hyphae-native-golden-requirements-v1",
                    "required_ids": ["one", "two", "three"],
                },
                inventory,
            )
        with self.assertRaisesRegex(GateFailure, "not required"):
            validate_completeness(
                {
                    "schema": "hyphae-native-golden-requirements-v1",
                    "required_ids": ["one"],
                },
                inventory,
            )
        with self.assertRaisesRegex(GateFailure, "duplicate required golden"):
            validate_completeness(
                {
                    "schema": "hyphae-native-golden-requirements-v1",
                    "required_ids": ["one", "one"],
                },
                inventory,
            )

    def test_missing_test_and_duplicate_id_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "producer.rs").write_text("fn actual_test() {}\n", encoding="utf-8")
            (root / "consumer.rs").write_text("fn decode() {}\n", encoding="utf-8")
            inventory = {
                "schema": "hyphae-native-golden-inventory-v1",
                "fixtures": [
                    {
                        "id": "fixture",
                        "producer": "producer.rs",
                        "test": "missing_test",
                        "consumer": "consumer.rs",
                    }
                ],
            }
            with self.assertRaisesRegex(GateFailure, "test symbol"):
                validate_inventory(root, inventory)

            inventory["fixtures"][0]["test"] = "actual_test"
            inventory["fixtures"].append(dict(inventory["fixtures"][0]))
            with self.assertRaisesRegex(GateFailure, "duplicate fixture"):
                validate_inventory(root, inventory)

    def test_unknown_fields_and_paths_outside_root_fail_closed(self) -> None:
        inventory = {
            "schema": "hyphae-native-golden-inventory-v1",
            "fixtures": [
                {
                    "id": "fixture",
                    "producer": "../outside.rs",
                    "test": "test_name",
                    "consumer": "consumer.rs",
                }
            ],
        }
        with self.assertRaisesRegex(GateFailure, "escapes repository root"):
            validate_inventory(ROOT, inventory)

        inventory["fixtures"][0]["producer"] = "producer.rs"
        inventory["fixtures"][0]["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown fixture field"):
            validate_inventory(ROOT, inventory)


if __name__ == "__main__":
    unittest.main()
