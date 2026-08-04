from __future__ import annotations

import json
import unittest
from pathlib import Path

from tools.check_native_conformance import GateFailure, validate_profile


ROOT = Path(__file__).resolve().parents[1]


class NativeConformanceProfileTests(unittest.TestCase):
    def test_checked_in_profile_has_exact_unique_surfaces(self) -> None:
        profile = json.loads(
            (ROOT / "config/native-conformance-profile.json").read_text(encoding="utf-8")
        )

        result = validate_profile(profile)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["surface_count"], 7)
        self.assertEqual(result["platform_counts"], {"linux": 7, "macos": 7, "windows": 1})

    def test_duplicate_surfaces_platforms_and_unknown_fields_fail_closed(self) -> None:
        surface = {
            "id": "frame",
            "command": "cargo test frame --locked",
            "required_platforms": ["linux"],
        }
        with self.assertRaisesRegex(GateFailure, "duplicate surface"):
            validate_profile(
                {
                    "schema": "hyphae-native-conformance-profile-v1",
                    "surfaces": [surface, dict(surface)],
                }
            )
        duplicated_platform = dict(surface)
        duplicated_platform["required_platforms"] = ["linux", "linux"]
        with self.assertRaisesRegex(GateFailure, "duplicate platform"):
            validate_profile(
                {
                    "schema": "hyphae-native-conformance-profile-v1",
                    "surfaces": [duplicated_platform],
                }
            )
        unknown = dict(surface)
        unknown["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown surface field"):
            validate_profile(
                {
                    "schema": "hyphae-native-conformance-profile-v1",
                    "surfaces": [unknown],
                }
            )

    def test_commands_are_locked_tests_and_platforms_are_known(self) -> None:
        with self.assertRaisesRegex(GateFailure, "locked cargo test"):
            validate_profile(
                {
                    "schema": "hyphae-native-conformance-profile-v1",
                    "surfaces": [
                        {
                            "id": "bad",
                            "command": "cargo run demo",
                            "required_platforms": ["linux"],
                        }
                    ],
                }
            )
        with self.assertRaisesRegex(GateFailure, "unknown platform"):
            validate_profile(
                {
                    "schema": "hyphae-native-conformance-profile-v1",
                    "surfaces": [
                        {
                            "id": "bad",
                            "command": "cargo test demo --locked",
                            "required_platforms": ["plan9"],
                        }
                    ],
                }
            )


if __name__ == "__main__":
    unittest.main()
