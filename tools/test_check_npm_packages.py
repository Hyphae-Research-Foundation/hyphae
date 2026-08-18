#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

from tools.check_npm_packages import PROJECTS, REQUIRED


class NpmPackageTests(unittest.TestCase):
    def test_publishable_projects_require_legal_bundle(self) -> None:
        self.assertEqual(PROJECTS, ("sdks/typescript", "integrations/javascript"))
        self.assertIn("package/THIRD_PARTY_NOTICES.md", REQUIRED)
        self.assertIn("package/LICENSE", REQUIRED)


if __name__ == "__main__":
    unittest.main()
