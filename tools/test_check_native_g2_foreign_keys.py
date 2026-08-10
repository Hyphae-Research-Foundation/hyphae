#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SQL = ROOT / "crates/hyphae-native-runtime/src/sql.rs"
TEST = ROOT / "crates/hyphae-native-runtime/tests/sql_foreign_keys_g2.rs"


class NativeG2ForeignKeyContractTests(unittest.TestCase):
    def test_constraints_are_persistent_and_authoritative(self) -> None:
        sql = SQL.read_text(encoding="utf-8")
        self.assertIn("ForeignKeyConstraintViolation", sql)
        self.assertIn("validate_foreign_keys(transaction", sql)
        self.assertIn("validate_parent_not_referenced", sql)

    def test_races_and_fail_closed_clauses_are_covered(self) -> None:
        test = TEST.read_text(encoding="utf-8")
        self.assertIn("concurrent_parent_delete_wins", test)
        self.assertIn("concurrent_child_commit_wins", test)
        self.assertIn("group_commit_rejects_second", test)
        self.assertIn("ON DELETE CASCADE", test)
        self.assertIn("Err(SqlError::TypeMismatch)", test)


if __name__ == "__main__":
    unittest.main()
