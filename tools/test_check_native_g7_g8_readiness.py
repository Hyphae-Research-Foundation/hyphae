#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g7_g8_readiness import GateFailure, validate


ROOT = Path(__file__).resolve().parents[1]


class NativeG7G8ReadinessTests(unittest.TestCase):
    def test_checked_in_authority_is_open_and_claim_free(self) -> None:
        result = validate(ROOT, "a" * 40)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["g7"]["status"], "open")
        self.assertEqual(result["g8"]["status"], "open")
        self.assertEqual(result["claims"], [])
        self.assertFalse(result["closure_declared"])

    def test_g8_closed_row_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in ("native-g7-readiness-profile.json", "native-g8-readiness-profile.json", "native-g8-suite-manifest.json"):
                source = ROOT / "config" / name
                (root / "config" / name).write_bytes(source.read_bytes())
            (root / ".github/workflows/native-g8-closure.yml").write_bytes(
                (ROOT / ".github/workflows/native-g8-closure.yml").read_bytes()
            )
            path = root / "config/native-g8-suite-manifest.json"
            payload = json.loads(path.read_text())
            payload["requirements"][0]["status"] = "passed"
            path.write_text(json.dumps(payload))
            with self.assertRaisesRegex(GateFailure, "not completely implemented"):
                validate(root, "a" * 40)

    def test_g8_closure_must_enforce_g7_predecessor(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in (
                "native-g7-readiness-profile.json",
                "native-g8-readiness-profile.json",
                "native-g8-suite-manifest.json",
            ):
                (root / "config" / name).write_bytes((ROOT / "config" / name).read_bytes())
            workflow = (ROOT / ".github/workflows/native-g8-closure.yml").read_text()
            (root / ".github/workflows/native-g8-closure.yml").write_text(
                workflow.replace("native-g7-aggregate.json", "missing-g7.json")
            )
            with self.assertRaisesRegex(GateFailure, "G7 predecessor"):
                validate(root, "a" * 40)

    def test_g8_closure_must_revalidate_raw_g7_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in (
                "native-g7-readiness-profile.json",
                "native-g8-readiness-profile.json",
                "native-g8-suite-manifest.json",
            ):
                (root / "config" / name).write_bytes((ROOT / "config" / name).read_bytes())
            workflow = (ROOT / ".github/workflows/native-g8-closure.yml").read_text()
            (root / ".github/workflows/native-g8-closure.yml").write_text(
                workflow.replace("--receipts", "--unchecked-receipts")
            )
            with self.assertRaisesRegex(GateFailure, "G7 predecessor"):
                validate(root, "a" * 40)


if __name__ == "__main__":
    unittest.main()
