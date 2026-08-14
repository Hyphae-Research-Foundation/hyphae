#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.inject_native_g2_evidence import GateFailure, inject

ROOT = Path(__file__).resolve().parents[1]


class InjectNativeG2EvidenceTests(unittest.TestCase):
    def test_exact_audit_is_injected_content_bound(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            audit = root / "audit.json"
            audit.write_text(json.dumps({
                "schema": "hyphae-native-g2-receipt-audit-v1",
                "status": "passed",
                "source_commit": "a" * 40,
                "requirement": "prepared-plans-and-explain",
                "test_count": 22,
                "suite_count": 2,
                "corpus_sha256": "b" * 64,
                "scope": "bounded-correctness",
                "production_scale": False,
            }) + "\n")
            baseline = json.loads((ROOT / "config/native-g2-readiness-evidence.json").read_text())
            result = inject(
                root,
                baseline,
                "prepared-plans-and-explain",
                Path("audit.json"),
                "hosted",
                "a" * 40,
            )
            row = result["evidence"]["prepared-plans-and-explain"]
            self.assertEqual(row["artifact_sha256"], hashlib.sha256(audit.read_bytes()).hexdigest())

    def test_wrong_commit_requirement_or_nonpassed_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            baseline = json.loads((ROOT / "config/native-g2-readiness-evidence.json").read_text())
            for field, value, message in (
                ("source_commit", "c" * 40, "commit"),
                ("requirement", "tpcc-acid", "requirement"),
                ("status", "failed", "passed"),
            ):
                payload = {
                    "schema": "hyphae-native-g2-receipt-audit-v1",
                    "status": "passed",
                    "source_commit": "a" * 40,
                    "requirement": "prepared-plans-and-explain",
                    "test_count": 22,
                    "suite_count": 2,
                    "corpus_sha256": "b" * 64,
                    "scope": "bounded-correctness",
                    "production_scale": False,
                }
                payload[field] = value
                (root / "audit.json").write_text(json.dumps(payload))
                with self.assertRaisesRegex(GateFailure, message):
                    inject(root, baseline, "prepared-plans-and-explain", Path("audit.json"), "hosted", "a" * 40)


if __name__ == "__main__":
    unittest.main()
