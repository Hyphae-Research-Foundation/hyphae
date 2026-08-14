#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Unit tests for exact-source PyPI distribution receipts."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.python_distribution_receipt import (
    PythonReceiptError,
    build_receipt,
    verify_registry,
)


COMMIT = "a" * 40


class PythonDistributionReceiptTests(unittest.TestCase):
    def fixture(self) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        (root / "hyphae_sdk-1.2.0-py3-none-any.whl").write_bytes(b"wheel")
        (root / "hyphae_sdk-1.2.0.tar.gz").write_bytes(b"sdist")
        return directory

    def test_build_and_verify_exact_registry_release(self) -> None:
        with self.fixture() as directory:
            receipt = build_receipt(Path(directory), "1.2.0", "v1.2.0", COMMIT)
        release = {
            "info": {"name": "hyphae-sdk", "version": "1.2.0"},
            "urls": [
                {"filename": entry["filename"], "digests": {"sha256": entry["sha256"]}}
                for entry in receipt["files"]
            ],
        }
        verified = verify_registry(receipt, release, "pypi", ("3.11", "3.14"))
        self.assertEqual(verified["status"], "published")
        self.assertEqual(verified["source_commit"], COMMIT)
        self.assertEqual(verified["verified_python_versions"], ["3.11", "3.14"])

    def test_tag_version_mismatch_is_rejected(self) -> None:
        with self.fixture() as directory:
            with self.assertRaisesRegex(PythonReceiptError, "version.*tag"):
                build_receipt(Path(directory), "1.2.0", "v1.2.1", COMMIT)

    def test_registry_digest_mismatch_is_rejected(self) -> None:
        with self.fixture() as directory:
            receipt = build_receipt(Path(directory), "1.2.0", "v1.2.0", COMMIT)
        release = {
            "info": {"name": "hyphae-sdk", "version": "1.2.0"},
            "urls": [{"filename": receipt["files"][0]["filename"], "digests": {"sha256": "0" * 64}}],
        }
        with self.assertRaisesRegex(PythonReceiptError, "SHA-256"):
            verify_registry(receipt, release, "testpypi")

    def test_malformed_receipt_cannot_compare_as_an_empty_release(self) -> None:
        with self.fixture() as directory:
            receipt = build_receipt(Path(directory), "1.2.0", "v1.2.0", COMMIT)
        receipt["files"] = []
        release = {
            "info": {"name": "hyphae-sdk", "version": "1.2.0"},
            "urls": [],
        }
        with self.assertRaisesRegex(PythonReceiptError, "exactly two"):
            verify_registry(receipt, release, "pypi")


if __name__ == "__main__":
    unittest.main()
