#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class NativeG2ClosureAuthorityTests(unittest.TestCase):
    def test_inventory_records_bounded_benchmark_scope(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        rows = {row["id"]: row for row in inventory["requirements"]}
        matrix = json.loads(
            (ROOT / "crates/hyphae-native-runtime/tests/corpus/g2-tpch-matrix.json").read_text()
        )
        unsupported = sum(row["status"] == "unsupported" for row in matrix["queries"])
        self.assertEqual(unsupported, 21)
        self.assertEqual(rows["tpch-correctness"]["status"], "implemented-unhosted")
        self.assertTrue(
            any(
                "21 unsupported" in claim
                for claim in rows["tpch-correctness"]["out_of_scope_non_claims"]
            )
        )

    def test_bounded_contract_retains_explicit_non_claims(self) -> None:
        inventory = json.loads((ROOT / "config/native-g2-inventory.json").read_text())
        text = json.dumps(inventory)
        for gap in (
            "non-rename schema evolution",
            "serializable/write-skew",
            "recursive CTEs",
            "21 unsupported canonical queries",
            "canonical full-column TPC-C schema",
        ):
            self.assertIn(gap, text)

    def test_readiness_profile_requires_all_bounded_rows_hosted(self) -> None:
        profile = json.loads((ROOT / "config/native-g2-readiness-profile.json").read_text())
        self.assertEqual(len(profile["requirements"]), 8)
        self.assertTrue(all(row["required_evidence"] == "hosted" for row in profile["requirements"]))
        self.assertNotIn("optional", json.dumps(profile).lower())
        gate = (ROOT / "docs/gates/native-local-phase-1.md").read_text()
        self.assertIn("Bounded relational engine", gate)
        self.assertIn("does not claim universal SQL compatibility", gate)


if __name__ == "__main__":
    unittest.main()
