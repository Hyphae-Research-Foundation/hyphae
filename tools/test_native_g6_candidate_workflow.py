#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools.aggregate_native_g6_candidate import validate_platform_artifacts
from tools.check_native_g6_foundation import PLATFORMS, REQUIREMENTS
from tools.run_native_g6_candidate import host_command
from tools.test_check_native_g6_conformance import receipt as conformance_receipt
from tools.test_native_g6_evidence_support import checked_raw, digests


class NativeG6CandidateWorkflowTests(unittest.TestCase):
    def test_windows_command_shims_are_executable(self) -> None:
        with mock.patch("tools.run_native_g6_candidate.os.name", "nt"):
            self.assertEqual(host_command(["npm", "test"]), ["npm.cmd", "test"])
            self.assertEqual(host_command(["node", "--test"]), ["node.exe", "--test"])
            self.assertEqual(host_command(["cargo", "test"]), ["cargo", "test"])

    def fixture(self, root: Path) -> dict[str, str]:
        manifest_sha256 = digests(checked_raw())
        for platform in PLATFORMS:
            platform_root = root / platform
            (platform_root / "receipts").mkdir(parents=True)
            (platform_root / "audits").mkdir()
            rows = []
            conformance = platform_root / "native-g6-conformance-receipt.json"
            conformance.write_text(json.dumps(conformance_receipt(platform)), encoding="utf-8")
            conformance_audit = platform_root / "native-g6-conformance-audit.json"
            conformance_audit.write_text(json.dumps(conformance_receipt(platform)), encoding="utf-8")
            for requirement in REQUIREMENTS:
                receipt = platform_root / "receipts" / f"{requirement}.json"
                audit = platform_root / "audits" / f"{requirement}.json"
                receipt.write_text(json.dumps({"requirement": requirement}), encoding="utf-8")
                audit.write_text(json.dumps({
                    "requirement": requirement,
                    "platform": platform,
                    "source_commit": "a" * 40,
                    "manifest_sha256": manifest_sha256,
                }), encoding="utf-8")
                rows.append(
                    {
                        "id": requirement,
                        "implementation_status": "implemented-unhosted",
                        "uncovered_acceptance": [],
                        "receipt_sha256": hashlib.sha256(receipt.read_bytes()).hexdigest(),
                        "audit_sha256": hashlib.sha256(audit.read_bytes()).hexdigest(),
                    }
                )
            summary = {
                "schema": "hyphae-native-g6-platform-candidate-v1",
                "gate": "G6",
                "status": "passed",
                "evidence_class": "supporting-not-closure",
                "source_commit": "a" * 40,
                "platform": platform,
                "host": platform,
                "manifest_sha256": manifest_sha256,
                "conformance_receipt_sha256": hashlib.sha256(conformance.read_bytes()).hexdigest(),
                "conformance_audit_sha256": hashlib.sha256(conformance_audit.read_bytes()).hexdigest(),
                "requirements": len(REQUIREMENTS),
                "requirement_receipts": rows,
                "claims": [],
                "closure_declared": False,
            }
            (platform_root / "platform-summary.json").write_text(
                json.dumps(summary), encoding="utf-8"
            )
        return manifest_sha256

    def test_platform_summaries_bind_every_receipt_and_remain_open(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_sha256 = self.fixture(root)
            validate_platform_artifacts(root, "a" * 40, manifest_sha256)

    def test_missing_platform_receipt_and_summary_claim_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_sha256 = self.fixture(root)
            (root / "windows/audits/native-local-daemon.json").unlink()
            with self.assertRaises((FileNotFoundError, ValueError)):
                validate_platform_artifacts(root, "a" * 40, manifest_sha256)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_sha256 = self.fixture(root)
            path = root / "linux/platform-summary.json"
            summary = json.loads(path.read_text(encoding="utf-8"))
            summary["claims"] = ["G6 complete"]
            path.write_text(json.dumps(summary), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "platform summary"):
                validate_platform_artifacts(root, "a" * 40, manifest_sha256)

    def test_duplicate_platform_artifact_identity_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_sha256 = self.fixture(root)
            path = root / "windows/audits/shared-contracts-and-errors.json"
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["platform"] = "linux"
            path.write_text(json.dumps(payload), encoding="utf-8")
            summary_path = root / "windows/platform-summary.json"
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            summary["requirement_receipts"][0]["audit_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            summary_path.write_text(json.dumps(summary), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "matrix cell"):
                validate_platform_artifacts(root, "a" * 40, manifest_sha256)

    def test_platform_summary_must_bind_real_conformance_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_sha256 = self.fixture(root)
            (root / "windows/native-g6-conformance-receipt.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "conformance evidence binding"):
                validate_platform_artifacts(root, "a" * 40, manifest_sha256)


if __name__ == "__main__":
    unittest.main()
