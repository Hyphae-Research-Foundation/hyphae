#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.assemble_native_g0_evidence import (
    INJECTIONS,
    assemble,
    validate_receipt_commit_identity,
)
from tools.check_native_g0_readiness import GateFailure, evaluate_readiness

ROOT = Path(__file__).resolve().parent.parent
PROFILE = json.loads((ROOT / "config/native-g0-readiness-profile.json").read_text())


class NativeG0EvidenceAssemblyTests(unittest.TestCase):
    def write_artifacts(self, root: Path, *, clean_room: str = "passed") -> None:
        for requirement, _, artifact in INJECTIONS:
            status = clean_room if requirement == "clean-room-porting-ledger-review" else "passed"
            (root / artifact).write_text(
                json.dumps(
                    {
                        "schema": f"test-{requirement}",
                        "status": status,
                        "source_commit": "a" * 40,
                    }
                )
                + "\n",
                encoding="utf-8",
            )

    def test_exact_eight_passed_artifacts_close_g0(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            evidence = assemble(root, PROFILE, {})
            readiness = evaluate_readiness(PROFILE, evidence)
            self.assertEqual(len(evidence), 8)
            self.assertEqual(readiness["status"], "passed")
            self.assertEqual(readiness["passed"], 8)

    def test_missing_or_blocked_artifact_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            (root / "native-quality-aggregate.json").unlink()
            with self.assertRaisesRegex(GateFailure, "missing"):
                assemble(root, PROFILE, {})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root, clean_room="blocked")
            with self.assertRaisesRegex(GateFailure, "does not report passed"):
                assemble(root, PROFILE, {})

    def test_receipt_commit_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            receipt = root / INJECTIONS[0][2]
            payload = json.loads(receipt.read_text())
            payload["source_commit"] = "b" * 40
            receipt.write_text(json.dumps(payload) + "\n")
            with self.assertRaisesRegex(GateFailure, "source commit mismatch"):
                validate_receipt_commit_identity(root, "a" * 40)
            payload.pop("source_commit")
            receipt.write_text(json.dumps(payload) + "\n")
            with self.assertRaisesRegex(GateFailure, "source commit mismatch"):
                validate_receipt_commit_identity(root, "a" * 40)
            payload["source_commit"] = "a" * 40
            receipt.write_text(json.dumps(payload) + "\n")
            validate_receipt_commit_identity(root, "a" * 40)

    def test_injection_contract_matches_profile_exactly(self) -> None:
        profile_ids = {entry["id"] for entry in PROFILE["requirements"]}
        injection_ids = {entry[0] for entry in INJECTIONS}
        self.assertEqual(injection_ids, profile_ids)
        self.assertEqual(len(INJECTIONS), len(injection_ids))


if __name__ == "__main__":
    unittest.main()
