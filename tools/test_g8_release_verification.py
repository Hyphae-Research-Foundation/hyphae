#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging"))

from g8_release_verification import expected_archives, verify  # noqa: E402


COMMIT = "a" * 40
TAG = "v1.0.0"


class G8ReleaseVerificationTests(unittest.TestCase):
    def test_expected_archives_rejects_noncanonical_tag(self) -> None:
        with self.assertRaises(ValueError):
            expected_archives("1.0")

    def test_verify_checks_every_signature_and_attestation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for name in expected_archives(TAG):
                (directory / name).write_bytes(name.encode("ascii"))
            for name in (
                f"hyphae-{TAG}.spdx.json",
                f"hyphae-{TAG}.cdx.json",
                f"hyphae-{TAG}.release-evidence.json",
                "SHA256SUMS",
            ):
                (directory / name).write_text(name, encoding="ascii")

            with (
                patch("g8_release_verification.run"),
                patch("g8_release_verification.verify_blob") as blob,
                patch("g8_release_verification.verify_attestation") as attestation,
            ):
                result = verify(directory, COMMIT, TAG, "workflow-identity")

            self.assertEqual(result["archive_count"], 4)
            self.assertEqual(result["signature_verifications"], 8)
            self.assertEqual(result["attestation_verifications"], 12)
            self.assertEqual(blob.call_count, 8)
            self.assertEqual(attestation.call_count, 12)

    def test_verify_rejects_incomplete_target_set(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            archive = sorted(expected_archives(TAG))[0]
            (directory / archive).write_bytes(b"archive")
            with patch("g8_release_verification.run"):
                with self.assertRaises(RuntimeError):
                    verify(directory, COMMIT, TAG, "workflow-identity")


if __name__ == "__main__":
    unittest.main()
