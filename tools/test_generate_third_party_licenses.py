#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

from tools.generate_third_party_licenses import OUTPUT, POLICY


class ThirdPartyLicenseBundleTests(unittest.TestCase):
    def test_bundle_has_policy_and_archive_authority(self) -> None:
        self.assertEqual(POLICY.as_posix(), "config/native-dependency-policy.json")
        self.assertEqual(OUTPUT.as_posix(), "THIRD_PARTY_LICENSES.txt")


if __name__ == "__main__":
    unittest.main()
