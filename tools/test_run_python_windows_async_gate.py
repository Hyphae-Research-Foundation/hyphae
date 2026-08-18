#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_python_windows_async_gate import CASES
from tools.run_python_windows_async_gate import (
    WindowsAsyncRunFailure,
    run,
    run_hosted_test,
    sanitized_environment,
    wheel_version,
)


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


class WindowsAsyncGateRunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.wheel = root / "hyphae_sdk-1.1.0-py3-none-any.whl"
        self.wheel.write_bytes(b"wheel")
        self.transcript = root / "transcript.json"

    def test_wheel_name_and_sanitized_child_environment_are_closed(self) -> None:
        self.assertEqual(wheel_version(self.wheel.name), "1.1.0")
        with self.assertRaisesRegex(WindowsAsyncRunFailure, "filename"):
            wheel_version("hyphae_sdk-latest.whl")
        with patch.dict(
            os.environ,
            {
                "PYTHONPATH": "source-tree",
                "OWNER_API_KEY": "sensitive",
                "ACCESS_TOKEN": "sensitive",
                "SAFE_VALUE": "retained",
            },
            clear=True,
        ):
            environment = sanitized_environment(self.wheel, self.transcript)
        self.assertNotIn("PYTHONPATH", environment)
        self.assertNotIn("OWNER_API_KEY", environment)
        self.assertNotIn("ACCESS_TOKEN", environment)
        self.assertEqual(environment["SAFE_VALUE"], "retained")
        self.assertEqual(environment["PYTHONNOUSERSITE"], "1")

    def test_child_failure_and_missing_transcript_fail_without_diagnostic_echo(
        self,
    ) -> None:
        failed = subprocess.CompletedProcess(
            [], 1, b"private stdout", b"private stderr"
        )
        with patch(
            "tools.run_python_windows_async_gate.subprocess.run", return_value=failed
        ):
            with self.assertRaisesRegex(
                WindowsAsyncRunFailure, "tests failed"
            ) as raised:
                run_hosted_test(Path("python.exe"), self.wheel, self.transcript)
        self.assertNotIn("private", str(raised.exception))

        passed = subprocess.CompletedProcess([], 0, b"", b"")
        with patch(
            "tools.run_python_windows_async_gate.subprocess.run", return_value=passed
        ):
            with self.assertRaisesRegex(WindowsAsyncRunFailure, "did not produce"):
                run_hosted_test(Path("python.exe"), self.wheel, self.transcript)

    def test_hosted_child_retains_only_canonical_transcript(self) -> None:
        expected = transcript(self.wheel)

        def complete(
            *args: object, **kwargs: object
        ) -> subprocess.CompletedProcess[bytes]:
            del args, kwargs
            self.transcript.write_text(json.dumps(expected) + "\n", encoding="utf-8")
            return subprocess.CompletedProcess([], 0, b"", b"")

        with patch(
            "tools.run_python_windows_async_gate.subprocess.run", side_effect=complete
        ):
            encoded, value = run_hosted_test(
                Path("python.exe"),
                self.wheel,
                self.transcript,
            )
        self.assertEqual(json.loads(encoded), expected)
        self.assertEqual(value["cases"], expected["cases"])

    @unittest.skipIf(os.name == "nt", "non-Windows guard")
    def test_non_windows_run_fails_before_source_or_child_execution(self) -> None:
        with patch("tools.run_python_windows_async_gate.source_identity") as source:
            with self.assertRaisesRegex(WindowsAsyncRunFailure, "requires Windows"):
                run(self.wheel, Path("receipt.json"), self.transcript)
        source.assert_not_called()


if __name__ == "__main__":
    unittest.main()
