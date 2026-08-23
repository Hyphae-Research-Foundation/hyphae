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
    validate_metadata,
)


def metadata_bytes(*extra_lines: str) -> bytes:
    lines = [
        "Metadata-Version: 2.4",
        "Name: hyphae-sdk",
        "Version: 2.0.0",
        "Requires-Python: >=3.11",
        "License-Expression: Apache-2.0",
        'Provides-Extra: providers',
        'Provides-Extra: langchain',
        'Provides-Extra: llamaindex',
        *extra_lines,
    ]
    return ("\n".join(lines) + "\n").encode("utf-8")


class PythonDistributionContractTests(unittest.TestCase):
    def test_frozen_extras_are_the_only_admitted_requirements(self) -> None:
        validate_metadata(
            metadata_bytes(
                'Requires-Dist: blake3>=0.4; extra == "providers"',
                'Requires-Dist: langchain-core>=0.3; extra == "langchain"',
            ),
            "2.0.0",
        )
        with self.assertRaisesRegex(
            DistributionValidationError, "runtime dependencies"
        ):
            validate_metadata(
                metadata_bytes("Requires-Dist: requests>=2"), "2.0.0"
            )
        with self.assertRaisesRegex(
            DistributionValidationError, "runtime dependencies"
        ):
            validate_metadata(
                metadata_bytes('Requires-Dist: requests>=2; extra == "providers"'),
                "2.0.0",
            )
        with self.assertRaisesRegex(DistributionValidationError, "extras differ"):
            validate_metadata(
                metadata_bytes('Provides-Extra: surprise'), "2.0.0"
            )

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
