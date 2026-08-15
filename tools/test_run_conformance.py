#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Boundary tests for the shared format-2 `/v1` conformance runner."""

from __future__ import annotations

import unittest

from tools.run_conformance import client_cases


class V1ConformanceBoundaryTests(unittest.TestCase):
    def test_native_mcp_is_not_run_as_a_legacy_v1_client(self) -> None:
        self.assertEqual(
            [case.name for case in client_cases()],
            ["rust", "typescript", "python", "cli"],
        )


if __name__ == "__main__":
    unittest.main()
