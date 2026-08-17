#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import subprocess
import unittest
from contextlib import redirect_stderr
from io import StringIO
from unittest.mock import patch

from tools.relicensing_checks import ROOT, main


class RelicensingCheckOrchestratorTests(unittest.TestCase):
    def test_stale_receipt_fails_without_refresh_by_default(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(arguments, **_kwargs):
            calls.append(tuple(arguments))
            returncode = int(
                arguments[-1] == "tools/check_relicensing_transition.py"
            )
            return subprocess.CompletedProcess(arguments, returncode)

        with patch("tools.relicensing_checks.subprocess.run", side_effect=run), patch(
            "tools.relicensing_checks.repository_state", return_value="stable"
        ):
            self.assertEqual(main([]), 1)
        self.assertEqual(calls[-1][-1], "tools/check_relicensing_transition.py")
        self.assertFalse(any("--refresh" in call for call in calls))

    def test_readonly_mode_rejects_checker_mutation(self) -> None:
        with patch(
            "tools.relicensing_checks.subprocess.run",
            side_effect=lambda arguments, **_kwargs: subprocess.CompletedProcess(
                arguments, 0
            ),
        ), patch(
            "tools.relicensing_checks.repository_state",
            side_effect=("before", "after"),
        ), redirect_stderr(StringIO()):
            self.assertEqual(main(["--readonly"]), 1)

    def test_ci_uses_explicit_readonly_mode(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("python tools/relicensing_checks.py --readonly", workflow)
        self.assertNotIn("python tools/relicensing_checks.py --refresh", workflow)

    def test_explicit_refresh_is_last_mutating_step_and_validation_follows(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(arguments, **_kwargs):
            calls.append(tuple(arguments))
            return subprocess.CompletedProcess(arguments, 0)

        with patch("tools.relicensing_checks.subprocess.run", side_effect=run), patch(
            "tools.relicensing_checks.repository_state", return_value="stable"
        ):
            self.assertEqual(main(["--refresh"]), 0)
        self.assertEqual(
            calls[-2][-2:], ("tools/check_relicensing_transition.py", "--refresh")
        )
        self.assertEqual(calls[-1][-1], "tools/check_relicensing_transition.py")


if __name__ == "__main__":
    unittest.main()
