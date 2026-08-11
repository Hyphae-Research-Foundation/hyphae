#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging"))

from g8_release_verification import (  # noqa: E402
    expected_archives,
    verify,
    verify_cyclonedx_hyphae_licenses,
    verify_spdx_hyphae_licenses,
)


COMMIT = "a" * 40
TAG = "v1.0.1"


def spdx(license_identifier: str = "AGPL-3.0-only") -> dict:
    return {
        "packages": [
            {
                "name": "hyphae-native-runtime",
                "licenseDeclared": license_identifier,
                "licenseConcluded": license_identifier,
            },
            {
                "name": "third-party-runtime",
                "licenseDeclared": "GPL-3.0-only",
                "licenseConcluded": "GPL-3.0-only",
            },
        ]
    }


def cyclonedx(license_identifier: str = "AGPL-3.0-only") -> dict:
    return {
        "components": [
            {
                "name": "hyphae-native-runtime",
                "licenses": [{"license": {"id": license_identifier}}],
            },
            {
                "name": "third-party-runtime",
                "licenses": [{"license": {"id": "GPL-3.0-only"}}],
            },
        ]
    }


class G8ReleaseVerificationTests(unittest.TestCase):
    def test_expected_archives_rejects_noncanonical_tag(self) -> None:
        with self.assertRaises(ValueError):
            expected_archives("1.0")

    def test_verify_checks_every_signature_and_attestation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for name in expected_archives(TAG):
                (directory / name).write_bytes(name.encode("ascii"))
            (directory / f"hyphae-{TAG}.spdx.json").write_text(
                json.dumps(spdx()), encoding="utf-8"
            )
            (directory / f"hyphae-{TAG}.cdx.json").write_text(
                json.dumps(cyclonedx()), encoding="utf-8"
            )
            for name in (f"hyphae-{TAG}.release-evidence.json", "SHA256SUMS"):
                (directory / name).write_text(name, encoding="ascii")

            with (
                patch("g8_release_verification.run") as run,
                patch("g8_release_verification.verify_blob") as blob,
                patch("g8_release_verification.verify_attestation") as attestation,
            ):
                result = verify(
                    directory,
                    COMMIT,
                    TAG,
                    "workflow-identity",
                    "b" * 40,
                    COMMIT,
                )

            self.assertEqual(result["archive_count"], 4)
            self.assertEqual(result["signature_verifications"], 8)
            self.assertEqual(result["attestation_verifications"], 12)
            self.assertEqual(blob.call_count, 8)
            self.assertEqual(attestation.call_count, 12)
            self.assertEqual(result["software_license"], "AGPL-3.0-only")
            self.assertEqual(
                result["spdx_hyphae_components"], ["hyphae-native-runtime"]
            )
            self.assertEqual(
                result["cyclonedx_hyphae_components"], ["hyphae-native-runtime"]
            )
            self.assertIn("--tag-object", run.call_args_list[0].args)
            self.assertIn("--tag-target", run.call_args_list[0].args)

    def test_spdx_requires_declared_and_concluded_agpl_for_hyphae_only(self) -> None:
        self.assertEqual(
            verify_spdx_hyphae_licenses(spdx()), ["hyphae-native-runtime"]
        )
        for field in ("licenseDeclared", "licenseConcluded"):
            with self.subTest(field=field):
                payload = spdx()
                payload["packages"][0][field] = "GPL-3.0-only"
                with self.assertRaisesRegex(RuntimeError, field):
                    verify_spdx_hyphae_licenses(payload)

    def test_cyclonedx_requires_agpl_for_nested_hyphae_components_only(self) -> None:
        payload = cyclonedx()
        payload["components"][0]["components"] = [
            {
                "name": "hyphae-core",
                "licenses": [{"expression": "AGPL-3.0-only"}],
            }
        ]
        self.assertEqual(
            verify_cyclonedx_hyphae_licenses(payload),
            ["hyphae-core", "hyphae-native-runtime"],
        )
        payload["components"][0]["licenses"] = [
            {"license": {"id": "GPL-3.0-only"}}
        ]
        with self.assertRaisesRegex(RuntimeError, "hyphae-native-runtime"):
            verify_cyclonedx_hyphae_licenses(payload)

    def test_sbom_license_validation_fails_when_hyphae_is_absent(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "no Hyphae"):
            verify_spdx_hyphae_licenses({"packages": spdx()["packages"][1:]})
        with self.assertRaisesRegex(RuntimeError, "no Hyphae"):
            verify_cyclonedx_hyphae_licenses(
                {"components": cyclonedx()["components"][1:]}
            )

    def test_verify_rejects_partial_tag_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(ValueError, "provided together"):
                verify(
                    Path(temporary),
                    COMMIT,
                    TAG,
                    "workflow-identity",
                    tag_object="b" * 40,
                )

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
