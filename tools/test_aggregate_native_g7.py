#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import tempfile
import unittest
from pathlib import Path

from tools.aggregate_native_g7 import aggregate
from tools.test_check_native_g7_matrix import matrix


class G7AggregateTests(unittest.TestCase):
    def test_three_platform_exact_sha_aggregate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for platform in ("linux", "macos", "windows"):
                path = root / platform
                path.mkdir()
                (path / "native-g7-matrix.json").write_text(
                    json.dumps({**matrix(), "platform": platform})
                )
            result = aggregate(root, "a" * 40)
            self.assertEqual(set(result["platforms"]), {"linux", "macos", "windows"})


if __name__ == "__main__":
    unittest.main()
