#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Failure-path tests for Python distribution inspection."""

from __future__ import annotations

import tempfile
import unittest
import zipfile
from pathlib import Path

from tools.check_python_distributions import (
    DistributionValidationError,
    safe_names,
    validate,
)


class PythonDistributionContractTests(unittest.TestCase):
    def test_path_traversal_is_rejected(self) -> None:
        with self.assertRaisesRegex(DistributionValidationError, "unsafe member"):
            safe_names(["../outside"])

    def test_bytecode_is_rejected(self) -> None:
        with self.assertRaisesRegex(DistributionValidationError, "bytecode"):
            safe_names(["hyphae_sdk/__pycache__/client.pyc"])

    def test_extra_distribution_file_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "unexpected.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(DistributionValidationError, "one wheel and one sdist"):
                validate(root, "1.2.0")

    def test_missing_wheel_metadata_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            wheel = root / "hyphae_sdk-1.2.0-py3-none-any.whl"
            with zipfile.ZipFile(wheel, mode="w") as archive:
                archive.writestr("hyphae_sdk/__init__.py", "")
            (root / "hyphae_sdk-1.2.0.tar.gz").write_bytes(b"not reached")
            with self.assertRaisesRegex(DistributionValidationError, "METADATA"):
                validate(root, "1.2.0")


if __name__ == "__main__":
    unittest.main()
