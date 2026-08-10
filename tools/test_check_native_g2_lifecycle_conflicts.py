#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUNTIME = ROOT / "crates/hyphae-native-runtime/src/lib.rs"


class NativeG2LifecycleConflictTests(unittest.TestCase):
    def test_relational_rows_validate_object_lifecycle_key(self) -> None:
        source = RUNTIME.read_text(encoding="utf-8")
        self.assertIn("fn catalog_object_lifecycle_write_key", source)
        self.assertIn("keys.push(catalog_object_lifecycle_write_key(object));", source)
        validation = source.split("fn mutation_validation_keys", 1)[1]
        self.assertIn("Opcode::InsertRow | Opcode::UpdateRow | Opcode::DeleteRow", validation)
        self.assertIn("catalog_object_lifecycle_write_key(object)", validation)


if __name__ == "__main__":
    unittest.main()
