from __future__ import annotations

import json
import unittest
from pathlib import Path

from tools.check_native_conformance import (
    GateFailure,
    run_profile,
    validate_profile,
    validate_receipt,
    validate_receipt_set,
)


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
    def test_runner_executes_only_current_platform_and_records_failures(self) -> None:
        profile = {
            "schema": "hyphae-native-conformance-profile-audit-v1",
            "status": "passed",
            "surface_count": 3,
            "platform_counts": {"linux": 2, "macos": 2, "windows": 1},
            "surfaces": [
                {
                    "id": "green",
                    "command": "cargo test green --locked",
                    "required_platforms": ["linux", "macos"],
                },
                {
                    "id": "red",
                    "command": "cargo test red --locked",
                    "required_platforms": ["linux"],
                },
                {
                    "id": "windows-only",
                    "command": "cargo test windows --locked",
                    "required_platforms": ["windows"],
                },
            ],
        }
        calls: list[list[str]] = []

        def execute(command: list[str]) -> int:
            calls.append(command)
            return 1 if "red" in command else 0

        result = run_profile(profile, "linux", execute)

        self.assertEqual(result["status"], "failed")
        self.assertEqual([row["id"] for row in result["results"]], ["green", "red"])
        self.assertEqual([row["status"] for row in result["results"]], ["passed", "failed"])
        self.assertEqual(
            calls,
            [
                ["cargo", "test", "green", "--locked"],
                ["cargo", "test", "red", "--locked"],
            ],
        )

        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("Run native conformance profile", workflow)
        self.assertIn("native-conformance-${{ matrix.native-platform }}.json", workflow)
        self.assertIn("run-native-conformance: false", workflow)

    def test_runner_rejects_unknown_platform_and_unvalidated_profile(self) -> None:
        with self.assertRaisesRegex(GateFailure, "unknown platform"):
            run_profile({"status": "passed", "surfaces": []}, "plan9", lambda _: 0)
        with self.assertRaisesRegex(GateFailure, "validated profile"):
            run_profile({"status": "failed", "surfaces": []}, "linux", lambda _: 0)
    def test_receipt_must_cover_exact_platform_surfaces_and_consistent_counts(self) -> None:
        profile = {
            "schema": "hyphae-native-conformance-profile-audit-v1",
            "status": "passed",
            "surfaces": [
                {"id": "one", "required_platforms": ["linux", "macos"]},
                {"id": "two", "required_platforms": ["linux"]},
            ],
        }
        receipt = {
            "schema": "hyphae-native-conformance-receipt-v1",
            "source_commit": "a" * 40,
            "platform": "linux",
            "status": "passed",
            "required_count": 2,
            "passed_count": 2,
            "results": [
                {"id": "one", "command": "cargo test one --locked", "status": "passed", "exit_code": 0},
                {"id": "two", "command": "cargo test two --locked", "status": "passed", "exit_code": 0},
            ],
        }
        validate_receipt(profile, receipt)

        receipt["results"].pop()
        with self.assertRaisesRegex(GateFailure, "coverage"):
            validate_receipt(profile, receipt)
        receipt["results"].append(
            {"id": "two", "command": "cargo test two --locked", "status": "failed", "exit_code": 1}
        )
        with self.assertRaisesRegex(GateFailure, "summary"):
            validate_receipt(profile, receipt)

    def test_receipt_rejects_unknown_fields_duplicate_results_and_inconsistent_exit(self) -> None:
        profile = {
            "schema": "hyphae-native-conformance-profile-audit-v1",
            "status": "passed",
            "surfaces": [{"id": "one", "required_platforms": ["linux"]}],
        }
        receipt = {
            "schema": "hyphae-native-conformance-receipt-v1",
            "source_commit": "a" * 40,
            "platform": "linux",
            "status": "passed",
            "required_count": 1,
            "passed_count": 1,
            "results": [
                {"id": "one", "command": "cargo test one --locked", "status": "passed", "exit_code": 1}
            ],
        }
        with self.assertRaisesRegex(GateFailure, "exit code"):
            validate_receipt(profile, receipt)
        receipt["results"][0]["exit_code"] = 0
        receipt["extra"] = True
        with self.assertRaisesRegex(GateFailure, "unknown receipt field"):
            validate_receipt(profile, receipt)
        receipt.pop("extra")
        receipt["results"].append(dict(receipt["results"][0]))
        receipt["required_count"] = 2
        receipt["passed_count"] = 2
        with self.assertRaisesRegex(GateFailure, "duplicate result"):
            validate_receipt(profile, receipt)
    def test_receipt_set_requires_every_platform_exactly_once(self) -> None:
        profile = {
            "schema": "hyphae-native-conformance-profile-audit-v1",
            "status": "passed",
            "surfaces": [
                {"id": "portable", "required_platforms": ["linux", "macos", "windows"]},
                {"id": "unix", "required_platforms": ["linux", "macos"]},
            ],
        }

        def receipt(platform: str, ids: list[str]) -> dict[str, object]:
            return {
                "schema": "hyphae-native-conformance-receipt-v1",
            "source_commit": "a" * 40,
                "platform": platform,
                "status": "passed",
                "required_count": len(ids),
                "passed_count": len(ids),
                "results": [
                    {"id": item, "command": f"cargo test {item} --locked", "status": "passed", "exit_code": 0}
                    for item in ids
                ],
            }

        receipts = [
            receipt("linux", ["portable", "unix"]),
            receipt("macos", ["portable", "unix"]),
            receipt("windows", ["portable"]),
        ]
        result = validate_receipt_set(profile, receipts)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["platform_count"], 3)
        self.assertEqual(result["passed_surfaces"], 5)

        with self.assertRaisesRegex(GateFailure, "missing platform receipt"):
            validate_receipt_set(profile, receipts[:-1])
        with self.assertRaisesRegex(GateFailure, "duplicate platform receipt"):
            validate_receipt_set(profile, receipts + [receipts[0]])

    def test_receipt_set_fails_when_one_platform_receipt_fails(self) -> None:
        profile = {
            "schema": "hyphae-native-conformance-profile-audit-v1",
            "status": "passed",
            "surfaces": [{"id": "portable", "required_platforms": ["linux", "macos"]}],
        }
        green = {
            "schema": "hyphae-native-conformance-receipt-v1",
            "source_commit": "a" * 40,
            "platform": "linux",
            "status": "passed",
            "required_count": 1,
            "passed_count": 1,
            "results": [{"id": "portable", "command": "cargo test portable --locked", "status": "passed", "exit_code": 0}],
        }
        red = {
            "schema": "hyphae-native-conformance-receipt-v1",
            "source_commit": "a" * 40,
            "platform": "macos",
            "status": "failed",
            "required_count": 1,
            "passed_count": 0,
            "results": [{"id": "portable", "command": "cargo test portable --locked", "status": "failed", "exit_code": 1}],
        }
        result = validate_receipt_set(profile, [green, red])
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["passed_platforms"], 1)

        workflow = (ROOT / ".github/workflows/native-conformance.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("pull_request:", workflow)
        self.assertIn("head_sha: sha", workflow)
        self.assertIn("steps.ci.outputs.run_id", workflow)
        self.assertIn("attempt <= 80", workflow)
        self.assertIn("item.conclusion === 'success'", workflow)
        self.assertIn("--aggregate", workflow)
        for platform in ("linux", "macos", "windows"):
            self.assertIn(f"native-conformance-{platform}.json", workflow)
        self.assertIn("native-conformance-aggregate.json", workflow)


if __name__ == "__main__":
    unittest.main()
