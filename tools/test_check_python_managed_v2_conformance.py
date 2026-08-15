#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for hosted Python managed Native v2 conformance receipts."""

from __future__ import annotations

import copy
import json
import unittest

from tools.check_python_managed_v2_conformance import (
    ConformanceFailure,
    SCHEMA_PATH,
    validate_receipt,
    validate_receipt_set,
    validate_schema_contract,
)


COMMIT = "a" * 40
TREE = "b" * 40
WHEEL = "c" * 64
BINARY = "d" * 64
TRANSCRIPT = "e" * 64
READS = [
    "security_assignment_list",
    "security_audit_read",
    "security_key_list",
    "security_principal_list",
    "security_role_list",
    "security_status",
]
WRITES = [
    "security_assignment_revoke",
    "security_built_in_assignment_create",
    "security_custom_assignment_create",
    "security_custom_role_create",
    "security_principal_create",
    "security_principal_set_enabled",
]


def receipt(platform: str) -> dict[str, object]:
    transports = (
        ["http-v2", "named-pipe"]
        if platform == "windows"
        else ["af-unix", "http-v2"]
    )
    return {
        "schema": "hyphae-python-managed-v2-conformance-receipt-v1",
        "status": "passed",
        "source_commit": COMMIT,
        "source_tree": TREE,
        "platform": platform,
        "python_version": "3.11.15",
        "distribution": {
            "filename": "hyphae_sdk-1.1.0-py3-none-any.whl",
            "sha256": WHEEL,
        },
        "binary": {
            "filename": "hyphae.exe" if platform == "windows" else "hyphae",
            "sha256": BINARY,
        },
        "fixture_binary": {
            "filename": (
                "hyphae-v2-fixture.exe"
                if platform == "windows"
                else "hyphae-v2-fixture"
            ),
            "sha256": "f" * 64,
        },
        "protocol": {"major": 1, "minor": 2},
        "transports": transports,
        "operations": {"reads": READS, "writes": WRITES},
        "cases": {
            "conflict": True,
            "next_operation_revocation": True,
            "pagination": True,
            "readback": True,
            "redaction": True,
            "replay": True,
            "stale_cursor": True,
        },
        "transcript_sha256": TRANSCRIPT,
    }


class PythonManagedV2ConformanceTests(unittest.TestCase):
    def test_checked_json_schema_matches_the_executable_contract(self) -> None:
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        validate_schema_contract(schema)
        schema["properties"]["protocol"]["properties"]["minor"]["const"] = 1
        with self.assertRaisesRegex(ConformanceFailure, "Schema policy"):
            validate_schema_contract(schema)

    def test_exact_three_platform_receipts_aggregate(self) -> None:
        rows = [receipt("linux"), receipt("macos"), receipt("windows")]
        rows[1]["python_version"] = "3.11.14"
        rows[2]["python_version"] = "3.11.13"
        result = validate_receipt_set(
            rows,
            expected_source_commit=COMMIT,
            expected_source_tree=TREE,
            receipt_sha256s=["1" * 64, "2" * 64, "3" * 64],
        )
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["platform_count"], 3)
        self.assertEqual(result["source_commit"], COMMIT)
        self.assertEqual(result["python_series"], "3.11")
        self.assertEqual(result["transcript_sha256"], TRANSCRIPT)
        self.assertEqual([lane["platform"] for lane in result["lanes"]], ["linux", "macos", "windows"])

    def test_missing_duplicate_and_wrong_transport_lanes_fail(self) -> None:
        with self.assertRaisesRegex(ConformanceFailure, "missing platform"):
            validate_receipt_set(
                [receipt("linux"), receipt("macos")],
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=["1" * 64, "2" * 64],
            )
        with self.assertRaisesRegex(ConformanceFailure, "duplicate platform"):
            validate_receipt_set(
                [receipt("linux"), receipt("linux"), receipt("windows")],
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=["1" * 64, "2" * 64, "3" * 64],
            )
        malformed = receipt("windows")
        malformed["transports"] = ["af-unix", "http-v2"]
        with self.assertRaisesRegex(ConformanceFailure, "transport"):
            validate_receipt(malformed)

    def test_source_tree_wheel_and_operation_drift_fail(self) -> None:
        rows = [receipt("linux"), receipt("macos"), receipt("windows")]
        rows[1]["source_tree"] = "f" * 40
        with self.assertRaisesRegex(ConformanceFailure, "source commit and tree"):
            validate_receipt_set(
                rows,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=["1" * 64, "2" * 64, "3" * 64],
            )

        rows = [receipt("linux"), receipt("macos"), receipt("windows")]
        distribution = rows[2]["distribution"]
        assert isinstance(distribution, dict)
        distribution["sha256"] = "f" * 64
        with self.assertRaisesRegex(ConformanceFailure, "wheel"):
            validate_receipt_set(
                rows,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=["1" * 64, "2" * 64, "3" * 64],
            )

        malformed = receipt("macos")
        malformed["python_version"] = "3.14.6"
        with self.assertRaisesRegex(ConformanceFailure, "Python version"):
            validate_receipt(malformed)

        rows = [receipt("linux"), receipt("macos"), receipt("windows")]
        rows[1]["transcript_sha256"] = "f" * 64
        with self.assertRaisesRegex(ConformanceFailure, "canonical transcript"):
            validate_receipt_set(
                rows,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=["1" * 64, "2" * 64, "3" * 64],
            )

        malformed = receipt("linux")
        operations = malformed["operations"]
        assert isinstance(operations, dict)
        operations["reads"] = READS[:-1]
        with self.assertRaisesRegex(ConformanceFailure, "operation inventory"):
            validate_receipt(malformed)

    def test_aggregate_binds_expected_checkout_and_receipt_bytes(self) -> None:
        rows = [receipt("linux"), receipt("macos"), receipt("windows")]
        digests = ["1" * 64, "2" * 64, "3" * 64]
        with self.assertRaisesRegex(ConformanceFailure, "expected source"):
            validate_receipt_set(
                rows,
                expected_source_commit="9" * 40,
                expected_source_tree=TREE,
                receipt_sha256s=digests,
            )
        with self.assertRaisesRegex(ConformanceFailure, "digest inventory"):
            validate_receipt_set(
                rows,
                expected_source_commit=COMMIT,
                expected_source_tree=TREE,
                receipt_sha256s=digests[:-1],
            )

        malformed = receipt("linux")
        malformed.pop("fixture_binary")
        with self.assertRaisesRegex(ConformanceFailure, "receipt fields"):
            validate_receipt(malformed)

    def test_case_and_unknown_field_drift_fail(self) -> None:
        malformed = receipt("linux")
        cases = malformed["cases"]
        assert isinstance(cases, dict)
        cases["pagination"] = False
        with self.assertRaisesRegex(ConformanceFailure, "cases"):
            validate_receipt(malformed)

        malformed = receipt("linux")
        malformed["escape_hatch"] = True
        with self.assertRaisesRegex(ConformanceFailure, "fields"):
            validate_receipt(malformed)

    def test_secret_vocabulary_is_rejected_recursively(self) -> None:
        for key in ("api_key", "secret", "serialized", "verifier", "verifier_digest"):
            with self.subTest(key=key):
                malformed = copy.deepcopy(receipt("linux"))
                malformed["binary"] = {
                    "filename": "hyphae",
                    "sha256": BINARY,
                    key: "redacted",
                }
                with self.assertRaisesRegex(ConformanceFailure, "forbidden"):
                    validate_receipt(malformed)


if __name__ == "__main__":
    unittest.main()
