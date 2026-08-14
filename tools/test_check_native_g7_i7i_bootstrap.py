#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the ephemeral AWS i7i G7 bootstrap contract."""

from __future__ import annotations

import unittest
from pathlib import Path

from tools.check_native_g7_i7i_bootstrap import (
    I7iBootstrapError,
    validate_i7i_bootstrap,
)


BOOTSTRAP = Path("tools/bootstrap_native_g7_i7i.sh")


class I7iBootstrapTests(unittest.TestCase):
    def test_repository_bootstrap_establishes_perf_before_runner_registration(self) -> None:
        audit = validate_i7i_bootstrap(BOOTSTRAP.read_text(encoding="utf-8"))
        self.assertEqual(audit["status"], "passed")
        self.assertEqual(len(audit["bootstrap_sha256"]), 64)
        self.assertEqual(audit["perf_event_paranoid"], -1)
        self.assertIs(audit["perf_canary_before_registration"], True)

    def test_rejects_missing_or_relaxed_perf_authority(self) -> None:
        source = BOOTSTRAP.read_text(encoding="utf-8")
        for old, new in (
            ("kernel.perf_event_paranoid = -1", "kernel.perf_event_paranoid = 4"),
            (
                "sysctl -w kernel.perf_event_paranoid=-1",
                "true # sysctl omitted",
            ),
            (
                'test "$(sysctl -n kernel.perf_event_paranoid)" = "-1"',
                "true # verification omitted",
            ),
            (
                "sudo -u ubuntu perf stat --no-big-num",
                "true # perf canary omitted",
            ),
            (
                'if set(measured) != expected or measured["cycles"] <= 0:',
                "if False:",
            ),
        ):
            with self.subTest(old=old):
                with self.assertRaises(I7iBootstrapError):
                    validate_i7i_bootstrap(source.replace(old, new, 1))

    def test_rejects_perf_canary_after_runner_registration(self) -> None:
        source = BOOTSTRAP.read_text(encoding="utf-8")
        canary = "sudo -u ubuntu perf stat --no-big-num"
        registration = "/opt/actions-runner/config.sh"
        reordered = source.replace(canary, "__CANARY__", 1).replace(
            registration, canary, 1
        ).replace("__CANARY__", registration, 1)
        with self.assertRaisesRegex(I7iBootstrapError, "before registration"):
            validate_i7i_bootstrap(reordered)


if __name__ == "__main__":
    unittest.main()
