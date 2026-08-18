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

    def test_release_readiness_fetches_only_verified_historical_tags(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        release_readiness = workflow.split("\n  release-readiness:\n", 1)[1]
        expected_tags = """\
          v0.1.0 3505ee8fb8a2184c5248109325aa2ad463ba1421 76b0cfdad90cf9e75d949a945c94a3badf0c6b59
          v0.2.0 72f55eb2f6023c36834a133d65411861dabcb01d 170380453a2ca6322a4c8bc50417318daee1c011
          v0.2.1 469be57b547661755a31d2fc838136f3c051dff4 08028e8dac077846c638f067ce74fbcf6fb75501
          v1.0.0 d5e9e142dfa49e688ce4f21880fcbc91d9b5016d 839ea6e2a806ed919610952cb17fd1dd61195d76
          v1.0.1 4a81737036c8122d23db632947666eaa03dfe61d 84161cf067141b60f4847b965ef77c5b749749c0
          v1.1.0 80b2f094c17ada6adc3bb879e20c3662bd93f4e4 e88f2ea2c3455a393e3ac0cd69e25486cc26888e
"""
        self.assertIn(
            "ref: ${{ github.event.pull_request.head.sha || github.sha }}",
            release_readiness,
        )
        self.assertIn("fetch-depth: 1", release_readiness)
        self.assertIn("fetch-tags: false", release_readiness)
        self.assertIn(
            'git fetch --no-tags --depth=1 origin "${tag_ref}:${tag_ref}"',
            release_readiness,
        )
        self.assertIn('git cat-file -t "$tag_ref"', release_readiness)
        self.assertIn('git rev-parse "${tag_ref}^{commit}"', release_readiness)
        self.assertIn(expected_tags, release_readiness)
        self.assertIn(
            'git fetch --unshallow --no-tags origin "$CANDIDATE_COMMIT"',
            release_readiness,
        )
        self.assertIn(
            "git rev-list --count fcf2f918e1539cfb7d67fd52abf0c7d57169ec18",
            release_readiness,
        )
        self.assertNotIn("fetch-depth: 0", release_readiness)
        self.assertNotIn("fetch-tags: true", release_readiness)
        self.assertNotIn("git fetch --tags", release_readiness)

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
            calls[-3][-2:], ("tools/check_relicensing_transition.py", "--refresh")
        )
        self.assertEqual(calls[-2][-1], "tools/check_relicensing_preflight.py")
        self.assertEqual(calls[-1][-1], "tools/check_relicensing_transition.py")


if __name__ == "__main__":
    unittest.main()
