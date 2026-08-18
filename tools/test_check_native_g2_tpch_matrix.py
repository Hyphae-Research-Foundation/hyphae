#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MATRIX = ROOT / "crates/hyphae-native-runtime/tests/corpus/g2-tpch-matrix.json"


class NativeG2TpchMatrixTests(unittest.TestCase):
    def test_all_22_queries_are_accounted_for_exactly_once(self) -> None:
        matrix = json.loads(MATRIX.read_text())
        self.assertEqual(matrix["schema"], "hyphae-native-g2-tpch-matrix-v1")
        queries = matrix["queries"]
        self.assertEqual([row["query"] for row in queries], list(range(1, 23)))
        self.assertEqual(len({row["query"] for row in queries}), 22)

    def test_admitted_and_unsupported_rows_have_exact_explanations(self) -> None:
        matrix = json.loads(MATRIX.read_text())
        admitted = [row for row in matrix["queries"] if row["status"] == "admitted-derived"]
        unsupported = [row for row in matrix["queries"] if row["status"] == "unsupported"]
        self.assertEqual([row["query"] for row in admitted], [3])
        self.assertEqual(admitted[0]["test"], "tpch_correctness_g2")
        self.assertEqual(len(unsupported), 21)
        self.assertTrue(all(row.get("reason") for row in unsupported))

    def test_no_query_is_claimed_canonical_or_production_scale(self) -> None:
        matrix = MATRIX.read_text()
        self.assertNotIn('"status": "passed"', matrix)
        self.assertNotIn("production-scale", matrix)
        self.assertEqual(
            len(json.loads(matrix)["derived_shapes"]),
            4,
        )


if __name__ == "__main__":
    unittest.main()
