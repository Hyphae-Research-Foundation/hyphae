#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import copy
import json
import unittest
from pathlib import Path

from tools.check_native_g1_crash_receipt import GateFailure, validate_receipt

ROOT = Path(__file__).resolve().parents[1]


class NativeG1CrashReceiptTests(unittest.TestCase):
    def receipt(self) -> dict:
        return {
            "schema": "hyphae.native.process-crash-matrix.v3",
            "status": "process-crash-not-power-loss",
            "source_commit": "a" * 40,
            "environment": "github-actions-ubuntu",
            "target": "x86_64-linux",
            "durability": "strict",
            "all_engine_csn": 1,
            "commit_boundaries": [
                {
                    "boundary": name,
                    "expected_state": state,
                    "recovered_csn": csn,
                    "recovered_blob_count": blobs,
                    "termination": "signal-9",
                }
                for name, state, csn, blobs in [
                    ("blob-staged", "prior-empty", None, 0),
                    ("blob-promoted", "prior-empty", None, 1),
                    ("page-appended", "prior-empty", None, 1),
                    ("page-synchronized", "prior-empty", None, 1),
                    ("wal-appended", "complete-csn-1", 1, 1),
                    ("wal-synchronized", "complete-csn-1", 1, 1),
                    ("root-published", "complete-csn-1", 1, 1),
                ]
            ],
            "checkpoint_boundaries": [
                {
                    "boundary": name,
                    "manifest_count": manifests,
                    "checkpoint_count": checkpoints,
                    "unanchored_manifest_suffix": suffix,
                    "recovered_temporary_manifests": 1 if name == "manifest-staged" else 0,
                    "termination": "signal-9",
                }
                for name, manifests, checkpoints, suffix in [
                    ("manifest-staged", 0, 0, 0),
                    ("manifest-published", 1, 0, 1),
                    ("wal-appended", 1, 1, 0),
                    ("wal-synchronized", 1, 1, 0),
                ]
            ],
            "snapshot_pin_boundaries": [
                {
                    "boundary": "record-synchronized",
                    "expected_pin": "absent",
                    "recovered_pin_count": 0,
                    "pin_directory_files": 0,
                    "retained_page_generations": 1,
                    "termination": "signal-9",
                },
                {
                    "boundary": "record-published",
                    "expected_pin": "present",
                    "recovered_pin_count": 1,
                    "pin_directory_files": 1,
                    "retained_page_generations": 1,
                    "termination": "signal-9",
                },
            ],
        }

    def test_exact_complete_matrix_passes(self) -> None:
        result = validate_receipt(self.receipt(), "a" * 40)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["commit_boundaries"], 7)
        self.assertEqual(result["checkpoint_boundaries"], 4)

    def test_missing_or_duplicate_boundary_fails_closed(self) -> None:
        receipt = self.receipt()
        receipt["commit_boundaries"].pop()
        with self.assertRaisesRegex(GateFailure, "commit boundary set"):
            validate_receipt(receipt, "a" * 40)
        receipt = self.receipt()
        receipt["checkpoint_boundaries"][1] = copy.deepcopy(
            receipt["checkpoint_boundaries"][0]
        )
        with self.assertRaisesRegex(GateFailure, "checkpoint boundary set"):
            validate_receipt(receipt, "a" * 40)

    def test_mixed_or_partial_csn_recovery_fails_closed(self) -> None:
        receipt = self.receipt()
        receipt["commit_boundaries"][0]["recovered_csn"] = 1
        with self.assertRaisesRegex(GateFailure, "prior state"):
            validate_receipt(receipt, "a" * 40)
        receipt = self.receipt()
        receipt["commit_boundaries"][4]["recovered_blob_count"] = 0
        with self.assertRaisesRegex(GateFailure, "complete state"):
            validate_receipt(receipt, "a" * 40)

    def test_wrong_commit_or_non_kill_termination_fails_closed(self) -> None:
        with self.assertRaisesRegex(GateFailure, "source commit"):
            validate_receipt(self.receipt(), "b" * 40)
        receipt = self.receipt()
        receipt["checkpoint_boundaries"][0]["termination"] = "exit-code-0"
        with self.assertRaisesRegex(GateFailure, "hard-killed"):
            validate_receipt(receipt, "a" * 40)


if __name__ == "__main__":
    unittest.main()
