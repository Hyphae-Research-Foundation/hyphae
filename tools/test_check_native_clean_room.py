#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_clean_room import GateFailure, validate_profile

ROOT = Path(__file__).resolve().parent.parent


class NativeCleanRoomTests(unittest.TestCase):
    def profile(self) -> dict:
        return {
            "schema": "hyphae-native-clean-room-profile-v1",
            "ledger": "docs/porting/ledger.md",
            "historical_inputs": [
                {
                    "source": "terrizoaguimor/hyphae-v2",
                    "revision": "3d06318fffb15a151520a35bd8b4f5b49954d6c5",
                    "decision": "Exclude",
                },
                {
                    "source": "local historical hyphae tree",
                    "revision": "268290e561c309ea24ac12392a6984670c8abccc",
                    "decision": "Rewrite",
                },
                {
                    "source": "local celiums-hyphae tree",
                    "revision": "174ebea2aa0b9df4a4bb4ee59d30c74bf76cb8e7",
                    "decision": "Rewrite",
                },
                {
                    "source": "celiumsai/hyphae-network",
                    "revision": "b6b630ca44dc549c42a7f921249b1cb210e13337",
                    "decision": "Defer",
                },
            ],
            "human_reviewers": [
                {
                    "github_login": "celiumsai",
                    "reviewed_commit": "a" * 40,
                    "scope": "G0 clean-room ledger and no accepted source ports",
                    "decision": "approved",
                }
            ],
        }

    def test_checked_in_ledger_profile_requires_exact_historical_bindings(self) -> None:
        summary = validate_profile(ROOT, self.profile())
        self.assertEqual(summary["historical_inputs"], 4)
        self.assertEqual(summary["human_reviewers"], 1)
        self.assertRegex(summary["ledger_sha256"], r"^[0-9a-f]{64}$")

    def test_missing_human_review_fails_closed(self) -> None:
        profile = self.profile()
        profile["human_reviewers"] = []
        with self.assertRaisesRegex(GateFailure, "human clean-room review"):
            validate_profile(ROOT, profile)

    def test_nonimmutable_or_unknown_inputs_fail_closed(self) -> None:
        profile = self.profile()
        profile["historical_inputs"][0]["revision"] = "main"
        with self.assertRaisesRegex(GateFailure, "immutable revision"):
            validate_profile(ROOT, profile)
        profile = self.profile()
        profile["historical_inputs"][0]["source"] = "unknown/source"
        with self.assertRaisesRegex(GateFailure, "does not bind"):
            validate_profile(ROOT, profile)

    def test_accepted_ports_must_remain_explicitly_none(self) -> None:
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ledger = root / "ledger.md"
            ledger.write_text("## Accepted ports\n\nOne.\n", encoding="utf-8")
            profile["ledger"] = "ledger.md"
            with self.assertRaisesRegex(GateFailure, "accepted ports"):
                validate_profile(root, profile)


if __name__ == "__main__":
    unittest.main()
