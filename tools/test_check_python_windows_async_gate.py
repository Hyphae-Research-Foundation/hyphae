#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_python_windows_async_gate import (
    CASES,
    SCHEMA_PATH,
    WindowsAsyncGateFailure,
    sha256,
    validate_receipt,
    validate_schema_contract,
)


COMMIT = "a" * 40
TREE = "b" * 40


def transcript(wheel: Path) -> dict[str, object]:
    return {
        "schema": "hyphae-python-windows-async-transcript-v1",
        "status": "passed",
        "platform": "windows",
        "python_version": "3.11.15",
        "distribution": {"filename": wheel.name, "version": "1.1.0"},
        "transport": "named-pipe",
        "cases": {
            name: {"elapsed_millis": 40, "error": error, "recovery": recovery}
            for name, (error, recovery) in CASES.items()
        },
    }


def receipt(wheel: Path, transcript_path: Path) -> dict[str, object]:
    return {
        "schema": "hyphae-python-windows-async-receipt-v1",
        "status": "passed",
        "source_commit": COMMIT,
        "source_tree": TREE,
        "platform": "windows",
        "python_version": "3.11.15",
        "distribution": {"filename": wheel.name, "sha256": sha256(wheel)},
        "transport": "named-pipe",
        "cases": {
            name: {"elapsed_millis": 40, "error": error, "recovery": recovery}
            for name, (error, recovery) in CASES.items()
        },
        "transcript_sha256": sha256(transcript_path),
    }


class WindowsAsyncGateContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.wheel = Path(self.directory.name) / "hyphae_sdk-1.1.0-py3-none-any.whl"
        self.wheel.write_bytes(b"exact wheel")
        self.transcript = Path(self.directory.name) / "transcript.json"
        self.transcript.write_text(
            json.dumps(transcript(self.wheel), sort_keys=True) + "\n",
            encoding="utf-8",
        )

    def test_schema_and_exact_receipt_pass(self) -> None:
        validate_schema_contract(json.loads(SCHEMA_PATH.read_text(encoding="utf-8")))
        result = validate_receipt(
            receipt(self.wheel, self.transcript),
            expected_source_commit=COMMIT,
            expected_source_tree=TREE,
            expected_wheel=self.wheel,
            expected_transcript=self.transcript,
        )
        self.assertEqual(result["source_tree"], TREE)
        self.assertEqual(result["wheel_sha256"], sha256(self.wheel))

    def test_source_wheel_and_case_drift_fail_closed(self) -> None:
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "source commit"):
            validate_receipt(
                receipt(self.wheel, self.transcript),
                expected_source_commit="d" * 40,
                expected_source_tree=TREE,
                expected_wheel=self.wheel,
                expected_transcript=self.transcript,
            )
        changed_wheel = Path(self.directory.name) / self.wheel.name
        changed_wheel.write_bytes(b"changed wheel")
        stale = receipt(self.wheel, self.transcript)
        changed_wheel.write_bytes(b"second change")
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "expected wheel"):
            validate_receipt(
                stale,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                expected_wheel=changed_wheel,
                expected_transcript=self.transcript,
            )
        changed_transcript = Path(self.directory.name) / "changed-transcript.json"
        changed_transcript.write_text(
            json.dumps(transcript(changed_wheel), sort_keys=True) + "\n",
            encoding="utf-8",
        )
        malformed = receipt(changed_wheel, changed_transcript)
        cases = malformed["cases"]
        assert isinstance(cases, dict)
        cases["welcome_deadline_reconnect"] = {
            "elapsed_millis": 1000,
            "error": "deadline_exceeded",
            "recovery": "reconnected",
        }
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "one second"):
            validate_receipt(
                malformed,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                expected_wheel=changed_wheel,
                expected_transcript=changed_transcript,
            )

    def test_unknown_fields_and_sensitive_material_fail_closed(self) -> None:
        malformed = receipt(self.wheel, self.transcript)
        malformed["claim"] = True
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "fields"):
            validate_receipt(
                malformed,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                expected_wheel=self.wheel,
                expected_transcript=self.transcript,
            )
        malformed = copy.deepcopy(receipt(self.wheel, self.transcript))
        malformed["endpoint"] = r"\\.\pipe\private"
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "forbidden"):
            validate_receipt(
                malformed,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                expected_wheel=self.wheel,
                expected_transcript=self.transcript,
            )

    def test_transcript_digest_and_schema_limit_drift_fail_closed(self) -> None:
        stale = receipt(self.wheel, self.transcript)
        value = transcript(self.wheel)
        value["status"] = "changed"
        self.transcript.write_text(json.dumps(value) + "\n", encoding="utf-8")
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "transcript"):
            validate_receipt(
                stale,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                expected_wheel=self.wheel,
                expected_transcript=self.transcript,
            )
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        schema["$defs"]["case"]["properties"]["elapsed_millis"]["exclusiveMaximum"] = (
            1001
        )
        with self.assertRaisesRegex(WindowsAsyncGateFailure, "Schema policy"):
            validate_schema_contract(schema)


if __name__ == "__main__":
    unittest.main()
