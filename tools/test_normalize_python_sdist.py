#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for deterministic Python source-distribution normalization."""

from __future__ import annotations

import hashlib
import io
import tarfile
import tempfile
import unittest
from pathlib import Path

from tools.normalize_python_sdist import SdistNormalizationError, normalize


EPOCH = 1_700_000_000


def write_sdist(path: Path, mtime: int) -> None:
    with tarfile.open(path, mode="w:gz") as archive:
        member = tarfile.TarInfo("hyphae_sdk-1.2.0/PKG-INFO")
        encoded = b"Name: hyphae-sdk\nVersion: 1.2.0\n"
        member.size = len(encoded)
        member.mtime = mtime
        member.uid = mtime % 100
        archive.addfile(member, io.BytesIO(encoded))


class SdistNormalizationTests(unittest.TestCase):
    def test_different_build_metadata_normalizes_to_identical_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "first.tar.gz"
            second = Path(directory) / "second.tar.gz"
            write_sdist(first, 11)
            write_sdist(second, 19)
            normalize(first, EPOCH)
            normalize(second, EPOCH)
            self.assertEqual(
                hashlib.sha256(first.read_bytes()).digest(),
                hashlib.sha256(second.read_bytes()).digest(),
            )

    def test_path_traversal_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unsafe.tar.gz"
            with tarfile.open(path, mode="w:gz") as archive:
                member = tarfile.TarInfo("../outside")
                archive.addfile(member, io.BytesIO())
            with self.assertRaisesRegex(SdistNormalizationError, "unsafe member"):
                normalize(path, EPOCH)

    def test_pre_zip_epoch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unused.tar.gz"
            with self.assertRaisesRegex(SdistNormalizationError, "representable"):
                normalize(path, 0)


if __name__ == "__main__":
    unittest.main()
