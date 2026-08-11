#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.aggregate_native_g7 import aggregate
from tools.check_native_g7_matrix import validate_closure_bundle
from tools.test_check_native_g7_matrix import matrix


class G7AggregateTests(unittest.TestCase):
    def test_g7_linux_aggregate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for platform in ("linux",):
                path = root / platform
                path.mkdir()
                (path / "native-g7-matrix.json").write_text(
                    json.dumps({**matrix(), "platform": platform})
                )
            result = aggregate(root, "a" * 40)
            self.assertEqual(set(result["platforms"]), {"linux"})
            self.assertEqual(
                validate_closure_bundle(result, root, "a" * 40)["status"],
                "passed",
            )

    def test_g7_darwin_aggregate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "darwin"
            path.mkdir()
            payload = matrix()
            payload["platform"] = "darwin"
            for receipt in payload["receipts"]:
                receipt["platform"] = "darwin"
                receipt["build"]["target"] = "aarch64-apple-darwin"
            (path / "native-g7-matrix.json").write_text(json.dumps(payload))
            result = aggregate(root, "a" * 40)
            self.assertEqual(set(result["platforms"]), {"darwin"})
            self.assertEqual(
                validate_closure_bundle(result, root, "a" * 40)["status"],
                "passed",
            )


if __name__ == "__main__":
    unittest.main()
