#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Mutation tests for the publishable Python SDK contract."""

from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path

from tools.check_python_package import (
    PythonPackageValidationError,
    ROOT,
    validate,
)


class PythonPackageContractTests(unittest.TestCase):
    def fixture(self) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        shutil.copytree(ROOT / "sdks/python", root / "sdks/python")
        (root / ".github/workflows").mkdir(parents=True)
        shutil.copy2(
            ROOT / ".github/workflows/python-publish.yml",
            root / ".github/workflows/python-publish.yml",
        )
        return directory

    def replace(self, root: Path, before: str, after: str) -> None:
        path = root / "sdks/python/pyproject.toml"
        text = path.read_text(encoding="utf-8")
        self.assertIn(before, text)
        path.write_text(text.replace(before, after, 1), encoding="utf-8")

    def test_checked_in_package_is_publishable(self) -> None:
        self.assertEqual(
            validate(),
            {"name": "hyphae-sdk", "status": "passed", "version": "1.1.0"},
        )

    def test_distribution_name_cannot_collide_with_unrelated_project(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, 'name = "hyphae-sdk"', 'name = "hyphae"')
            with self.assertRaisesRegex(PythonPackageValidationError, "hyphae-sdk"):
                validate(root)

    def test_runtime_dependency_cannot_be_added_silently(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, "dependencies = []", 'dependencies = ["requests"]')
            with self.assertRaisesRegex(PythonPackageValidationError, "standard-library"):
                validate(root)

    def test_typed_marker_cannot_be_omitted(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, 'hyphae_sdk = ["py.typed"]', "hyphae_sdk = []")
            with self.assertRaisesRegex(PythonPackageValidationError, "typed-distribution"):
                validate(root)

    def test_publisher_cannot_fall_back_to_a_stored_token(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".github/workflows/python-publish.yml"
            path.write_text(
                path.read_text(encoding="utf-8") + "\n# PYPI_TOKEN\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PythonPackageValidationError, "OIDC"):
                validate(root)


if __name__ == "__main__":
    unittest.main()
