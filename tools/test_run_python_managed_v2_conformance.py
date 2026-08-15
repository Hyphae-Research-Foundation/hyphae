#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Unit tests for the real-daemon Python managed conformance harness."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.run_python_managed_v2_conformance import (
    LiveConformanceFailure,
    assert_safe_output,
    bounded_diagnostic,
    local_endpoint,
    require_daemon_running,
    require_clean_status,
    resolve_executable,
    validate_transcript,
)


class PythonManagedV2RunnerTests(unittest.TestCase):
    def test_posix_endpoint_is_short_and_windows_is_a_local_namespace(self) -> None:
        endpoint, path = local_endpoint("linux")
        self.assertIsNotNone(path)
        self.assertLess(len(endpoint.encode()), 100)
        windows, windows_path = local_endpoint("windows")
        self.assertIsNone(windows_path)
        self.assertTrue(windows.startswith("hyphae-python-"))
        self.assertNotIn("\\\\", windows)

    def test_output_rejects_exact_and_credential_shaped_material(self) -> None:
        with self.assertRaisesRegex(LiveConformanceFailure, "exceeded its bound"):
            assert_safe_output(b"x" * (64 * 1024 + 1), ())
        with self.assertRaisesRegex(LiveConformanceFailure, "credential"):
            assert_safe_output(b"prefix exact-value suffix", (b"exact-value",))
        with self.assertRaisesRegex(LiveConformanceFailure, "credential-shaped"):
            assert_safe_output(b"hyp1_not-a-real-key", ())
        self.assertEqual(bounded_diagnostic(b" bounded error\n"), "bounded error")

    def test_source_receipt_rejects_a_dirty_worktree(self) -> None:
        require_clean_status(b"")
        with self.assertRaisesRegex(LiveConformanceFailure, "clean exact-commit"):
            require_clean_status(b"?? conformance/v2/untracked.py\n")

    def test_daemon_must_live_until_controlled_shutdown(self) -> None:
        require_daemon_running(None)
        with self.assertRaisesRegex(LiveConformanceFailure, "controlled shutdown"):
            require_daemon_running(1)

    def test_transcript_requires_every_case_and_operation(self) -> None:
        valid = {
            "schema": "hyphae-python-managed-v2-transcript-v1",
            "status": "passed",
            "protocol": {"major": 1, "minor": 2},
            "operations": {
                "reads": [
                    "security_assignment_list",
                    "security_audit_read",
                    "security_key_list",
                    "security_principal_list",
                    "security_role_list",
                    "security_status",
                ],
                "writes": [
                    "security_assignment_revoke",
                    "security_built_in_assignment_create",
                    "security_custom_assignment_create",
                    "security_custom_role_create",
                    "security_principal_create",
                    "security_principal_set_enabled",
                ],
            },
            "cases": {
                "conflict": True,
                "next_operation_revocation": True,
                "pagination": True,
                "readback": True,
                "redaction": True,
                "replay": True,
                "stale_cursor": True,
            },
        }
        self.assertEqual(validate_transcript(valid), valid)
        valid["cases"]["replay"] = False
        with self.assertRaisesRegex(LiveConformanceFailure, "contract"):
            validate_transcript(valid)

    def test_missing_executable_fails_before_daemon_start(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(LiveConformanceFailure, "executable is missing"):
                resolve_executable(Path(directory) / "hyphae", "linux")


if __name__ == "__main__":
    unittest.main()
