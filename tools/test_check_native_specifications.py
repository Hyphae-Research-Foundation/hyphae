#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_specifications import GateFailure, REQUIRED_SPECS, validate_profile

ROOT = Path(__file__).resolve().parent.parent
PROFILE = ROOT / "config/native-specification-profile.json"


class NativeSpecificationTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads(PROFILE.read_text(encoding="utf-8"))

    def test_checked_in_profile_binds_exact_g0_specification_set(self) -> None:
        summary = validate_profile(ROOT, self.profile())
        self.assertEqual(summary["document_count"], len(REQUIRED_SPECS) + 1)
        self.assertTrue(all(len(value) == 64 for value in summary["documents"].values()))

    def test_missing_duplicate_and_unknown_specifications_fail_closed(self) -> None:
        profile = self.profile()
        profile["specifications"].pop()
        with self.assertRaisesRegex(GateFailure, "exact G0 contract set"):
            validate_profile(ROOT, profile)
        profile = self.profile()
        profile["specifications"].append(profile["specifications"][0])
        with self.assertRaisesRegex(GateFailure, "unique array"):
            validate_profile(ROOT, profile)
        profile = self.profile()
        profile["unexpected"] = True
        with self.assertRaisesRegex(GateFailure, "unsupported"):
            validate_profile(ROOT, profile)

    def test_missing_or_unaccepted_documents_fail_closed(self) -> None:
        profile = self.profile()
        profile["architecture"] = "docs/missing.md"
        with self.assertRaisesRegex(GateFailure, "missing"):
            validate_profile(ROOT, profile)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            architecture = root / "architecture.md"
            architecture.write_text("# Architecture\n\nDraft.\n", encoding="utf-8")
            profile = {
                "schema": "hyphae-native-specification-profile-v1",
                "architecture": "architecture.md",
                "specifications": [f"{name}" for name in sorted(REQUIRED_SPECS)],
            }
            for name in REQUIRED_SPECS:
                (root / name).write_text(f"# {name} v1\n", encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "not explicitly accepted"):
                validate_profile(root, profile)


if __name__ == "__main__":
    unittest.main()
