#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g6_foundation import GateFailure
from tools.inject_native_g6_evidence import inject
from tools.test_native_g6_evidence_support import COMMIT, checked_raw, digests, payloads


class NativeG6EvidenceInjectionTests(unittest.TestCase):
    def baseline(self) -> dict:
        return payloads(checked_raw())["evidence"]

    def test_requirement_and_predecessor_audits_are_digest_bound(self) -> None:
        manifest_sha256 = digests(checked_raw())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            requirement = {
                "schema": "hyphae-native-g6-receipt-audit-v1", "gate": "G6", "status": "passed",
                "source_commit": COMMIT, "requirement": "shared-contracts-and-errors",
                "platform": "linux",
                "manifest_sha256": manifest_sha256, "claims": [], "closure_declared": False,
            }
            requirement_path = root / "requirement.json"
            requirement_path.write_text(json.dumps(requirement), encoding="utf-8")
            evidence = inject(root, self.baseline(), "requirement", Path("requirement.json"), COMMIT, manifest_sha256, "shared-contracts-and-errors", "linux")
            self.assertEqual(evidence["evidence"]["shared-contracts-and-errors"]["linux"]["artifact_sha256"], hashlib.sha256(requirement_path.read_bytes()).hexdigest())
            predecessor = {
                "schema": "hyphae-native-g6-manifest-audit-v1", "gate": "G6", "status": "passed",
                "source_commit": COMMIT, "manifest_sha256": manifest_sha256, "predecessor_count": 6,
                "claims": [], "closure_declared": False,
            }
            predecessor_path = root / "predecessor.json"
            predecessor_path.write_text(json.dumps(predecessor), encoding="utf-8")
            evidence = inject(root, evidence, "predecessor", Path("predecessor.json"), COMMIT, manifest_sha256)
            self.assertEqual(evidence["predecessor"]["level"], "retained")
            self.assertEqual((evidence["claims"], evidence["closure_declared"]), ([], False))

    def test_duplicate_escape_and_claim_fail_closed(self) -> None:
        manifest_sha256 = digests(checked_raw())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = {
                "schema": "hyphae-native-g6-receipt-audit-v1", "gate": "G6", "status": "passed",
                "source_commit": COMMIT, "requirement": "shared-contracts-and-errors",
                "platform": "linux",
                "manifest_sha256": manifest_sha256, "claims": [], "closure_declared": False,
            }
            path = root / "audit.json"
            path.write_text(json.dumps(payload), encoding="utf-8")
            evidence = inject(root, self.baseline(), "requirement", Path("audit.json"), COMMIT, manifest_sha256, "shared-contracts-and-errors", "linux")
            with self.assertRaisesRegex(GateFailure, "duplicate"):
                inject(root, evidence, "requirement", Path("audit.json"), COMMIT, manifest_sha256, "shared-contracts-and-errors", "linux")
            with self.assertRaisesRegex(GateFailure, "reference"):
                inject(root, self.baseline(), "requirement", Path("../audit.json"), COMMIT, manifest_sha256, "shared-contracts-and-errors", "linux")
            payload["claims"] = ["G6 complete"]
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "claim"):
                inject(root, self.baseline(), "requirement", Path("audit.json"), COMMIT, manifest_sha256, "shared-contracts-and-errors", "linux")


if __name__ == "__main__":
    unittest.main()
