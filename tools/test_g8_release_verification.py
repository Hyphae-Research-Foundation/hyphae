#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from collections import Counter
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
    expected_hyphae_identities,
    verify,
    verify_cyclonedx_hyphae_licenses,
    verify_spdx_hyphae_licenses,
)


COMMIT = "a" * 40
TAG = "v1.0.1"


def spdx(license_identifier: str = "Apache-2.0") -> dict:
    return {
        "packages": [
            {
                "name": "hyphae-native-runtime",
                "versionInfo": "1.0.1",
                "externalRefs": [
                    {
                        "referenceType": "purl",
                        "referenceLocator": "pkg:cargo/hyphae-native-runtime@1.0.1",
                    }
                ],
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


def cyclonedx(license_identifier: str = "Apache-2.0") -> dict:
    return {
        "components": [
            {
                "name": "hyphae-native-runtime",
                "version": "1.0.1",
                "purl": "pkg:cargo/hyphae-native-runtime@1.0.1",
                "licenses": [{"license": {"id": license_identifier}}],
            },
            {
                "name": "third-party-runtime",
                "licenses": [{"license": {"id": "GPL-3.0-only"}}],
            },
        ]
    }


class G8ReleaseVerificationTests(unittest.TestCase):
    def test_repository_release_authority_remains_79_artifacts_33_identities(
        self,
    ) -> None:
        identities = expected_hyphae_identities()
        self.assertEqual(sum(identities.values()), 79)
        self.assertEqual(len(identities), 33)
        self.assertNotIn(
            "hyphae-mcp-conformance-hosts",
            {name for name, _, _ in identities},
        )

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
                patch(
                    "g8_release_verification.expected_hyphae_identities",
                    return_value=Counter(
                        {
                            (
                                "hyphae-native-runtime",
                                "1.0.1",
                                "pkg:cargo/hyphae-native-runtime@1.0.1",
                            ): 1
                        }
                    ),
                ),
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
            self.assertEqual(result["software_license"], "Apache-2.0")
            self.assertEqual(
                result["license_authority"],
                "tracked-package-manifests-and-local-locks-v1",
            )
            self.assertEqual(result["first_party_artifact_count"], 1)
            self.assertEqual(result["first_party_identity_count"], 1)
            self.assertEqual(
                result["spdx_hyphae_components"], ["hyphae-native-runtime"]
            )
            self.assertEqual(
                result["cyclonedx_hyphae_components"], ["hyphae-native-runtime"]
            )
            self.assertIn("--tag-object", run.call_args_list[0].args)
            self.assertIn("--tag-target", run.call_args_list[0].args)

    def test_spdx_requires_declared_and_concluded_apache_for_hyphae_only(self) -> None:
        self.assertEqual(
            verify_spdx_hyphae_licenses(spdx()), ["hyphae-native-runtime"]
        )
        for field in ("licenseDeclared", "licenseConcluded"):
            with self.subTest(field=field):
                payload = spdx()
                payload["packages"][0][field] = "GPL-3.0-only"
                with self.assertRaisesRegex(RuntimeError, field):
                    verify_spdx_hyphae_licenses(payload)

    def test_cyclonedx_requires_apache_for_nested_hyphae_components_only(self) -> None:
        payload = cyclonedx()
        payload["components"][0]["components"] = [
            {
                "name": "hyphae-core",
                "licenses": [{"expression": "Apache-2.0"}],
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

    def test_verify_rejects_spdx_cyclonedx_identity_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for name in expected_archives(TAG):
                (directory / name).write_bytes(name.encode("ascii"))
            (directory / f"hyphae-{TAG}.spdx.json").write_text(
                json.dumps(spdx()), encoding="utf-8"
            )
            drifted = cyclonedx()
            drifted["components"][0]["version"] = "9.9.9"
            (directory / f"hyphae-{TAG}.cdx.json").write_text(
                json.dumps(drifted), encoding="utf-8"
            )
            for name in (f"hyphae-{TAG}.release-evidence.json", "SHA256SUMS"):
                (directory / name).write_text(name, encoding="ascii")
            with (
                patch("g8_release_verification.run"),
                patch(
                    "g8_release_verification.expected_hyphae_identities",
                    return_value=Counter(
                        {
                            (
                                "hyphae-native-runtime",
                                "1.0.1",
                                "pkg:cargo/hyphae-native-runtime@1.0.1",
                            ): 1
                        }
                    ),
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "identities differ"):
                    verify(directory, COMMIT, TAG, "workflow-identity")

    def test_verify_rejects_matching_but_truncated_sbom_pair(self) -> None:
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
                patch("g8_release_verification.run"),
                patch(
                    "g8_release_verification.expected_hyphae_identities",
                    return_value=Counter(
                        {
                            (
                                "hyphae-native-runtime",
                                "1.0.1",
                                "pkg:cargo/hyphae-native-runtime@1.0.1",
                            ): 1,
                            (
                                "hyphae-core",
                                "1.0.1",
                                "pkg:cargo/hyphae-core@1.0.1",
                            ): 1,
                        }
                    ),
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "omits or adds"):
                    verify(directory, COMMIT, TAG, "workflow-identity")

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
