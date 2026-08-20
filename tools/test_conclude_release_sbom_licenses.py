#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging"))

from conclude_release_sbom_licenses import (  # noqa: E402
    SOFTWARE_LICENSE,
    SYFT_VERSION,
    conclude_document,
    conclude_file,
)


def write(path: Path, value: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")


def rust_artifact() -> dict:
    return {
        "id": "rust-id",
        "name": "hyphae-core",
        "version": "1.0.1",
        "type": "rust-crate",
        "foundBy": "rust-cargo-lock-cataloger",
        "locations": [{"path": "/Cargo.lock"}],
        "licenses": [],
        "purl": "pkg:cargo/hyphae-core@1.0.1",
        "metadata": {"source": "", "checksum": ""},
    }


def linked_npm_artifact() -> dict:
    return {
        "id": "npm-link-id",
        "name": "@hyphae_/hyphae",
        "version": "UNKNOWN",
        "type": "npm",
        "foundBy": "javascript-lock-cataloger",
        "locations": [{"path": "/integrations/javascript/package-lock.json"}],
        "licenses": [],
        "purl": "pkg:npm/%40hyphae_/hyphae",
        "metadata": {"resolved": "../../sdks/typescript"},
        "cpes": [
            {
                "cpe": "cpe:2.3:a:\\@hyphae_\\/hyphae:\\@hyphae_\\/hyphae:*:*:*:*:*:*:*:*",
                "source": "syft-generated",
            }
        ],
    }


def private_mcp_host_artifact() -> dict:
    return {
        "id": "private-mcp-host-id",
        "name": "hyphae-mcp-conformance-hosts",
        "version": "1.0.0",
        "type": "npm",
        "foundBy": "javascript-lock-cataloger",
        "locations": [{"path": "/conformance/mcp/hosts/package-lock.json"}],
        "licenses": [],
        "purl": "pkg:npm/hyphae-mcp-conformance-hosts@1.0.0",
    }


def syft_document(*artifacts: dict) -> dict:
    return {
        "descriptor": {"name": "syft", "version": SYFT_VERSION},
        "artifacts": list(artifacts),
        "artifactRelationships": [],
    }


class ConcludeReleaseSbomLicensesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        write(
            self.root / "Cargo.toml",
            """[workspace]
members = ["crates/hyphae-core"]

[workspace.package]
version = "1.0.1"
license = "Apache-2.0"
""",
        )
        write(
            self.root / "crates/hyphae-core/Cargo.toml",
            """[package]
name = "hyphae-core"
version.workspace = true
license.workspace = true
""",
        )
        write(
            self.root / "Cargo.lock",
            """version = 4

[[package]]
name = "hyphae-core"
version = "1.0.1"
""",
        )
        write(
            self.root / "sdks/typescript/package.json",
            json.dumps(
                {
                    "name": "@hyphae_/hyphae",
                    "version": "1.0.1",
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        write(
            self.root / "integrations/javascript/package-lock.json",
            json.dumps(
                {
                    "packages": {
                        "": {
                            "name": "fixture",
                            "version": "1.0.1",
                            "license": SOFTWARE_LICENSE,
                            "peerDependencies": {"@hyphae_/hyphae": "1.0.1"},
                        },
                        "node_modules/@hyphae_/hyphae": {
                            "resolved": "../../sdks/typescript",
                            "link": True,
                        },
                        "../../sdks/typescript": {
                            "name": "@hyphae_/hyphae",
                            "version": "1.0.1",
                            "license": SOFTWARE_LICENSE,
                        },
                    }
                }
            ),
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_concludes_exact_rust_and_linked_npm_without_changing_identity(self) -> None:
        third_party = {"name": "serde", "licenses": [{"value": "MIT"}]}
        document = syft_document(rust_artifact(), linked_npm_artifact(), third_party)
        document["artifactRelationships"] = [
            {"parent": "rust-id", "child": "npm-link-id"}
        ]
        original_relationships = copy.deepcopy(document["artifactRelationships"])

        self.assertEqual(conclude_document(document, self.root), 2)
        self.assertEqual(document["artifactRelationships"], original_relationships)
        self.assertIs(document["artifacts"][2], third_party)
        for artifact in document["artifacts"][:2]:
            self.assertEqual(
                [(value["type"], value["value"]) for value in artifact["licenses"]],
                [("declared", SOFTWARE_LICENSE), ("concluded", SOFTWARE_LICENSE)],
            )
        self.assertEqual(document["artifacts"][1]["version"], "1.0.1")
        self.assertEqual(
            document["artifacts"][1]["purl"],
            "pkg:npm/%40hyphae_/hyphae@1.0.1",
        )
        self.assertIn(":1.0.1:", document["artifacts"][1]["cpes"][0]["cpe"])

        first = copy.deepcopy(document)
        self.assertEqual(conclude_document(document, self.root), 2)
        self.assertEqual(document, first)

    def test_rejects_conflicting_observed_license(self) -> None:
        artifact = rust_artifact()
        artifact["licenses"] = [
            {
                "value": "GPL-3.0-only",
                "spdxExpression": "GPL-3.0-only",
                "type": "declared",
            }
        ]
        with self.assertRaisesRegex(RuntimeError, "conflicting observed license"):
            conclude_document(syft_document(artifact), self.root)

    def test_rejects_registry_crate_with_first_party_name(self) -> None:
        artifact = rust_artifact()
        artifact["metadata"][
            "source"
        ] = "registry+https://github.com/rust-lang/crates.io-index"
        with self.assertRaisesRegex(RuntimeError, "not a local path package"):
            conclude_document(syft_document(artifact), self.root)

    def test_rejects_link_escape(self) -> None:
        artifact = linked_npm_artifact()
        lock = self.root / "integrations/javascript/package-lock.json"
        payload = json.loads(lock.read_text(encoding="utf-8"))
        payload["packages"]["node_modules/@hyphae_/hyphae"][
            "resolved"
        ] = "../../../../outside"
        lock.write_text(json.dumps(payload), encoding="utf-8")
        artifact["metadata"]["resolved"] = "../../../../outside"
        with self.assertRaisesRegex(RuntimeError, "escapes repository"):
            conclude_document(syft_document(artifact), self.root)

    def test_rejects_mismatched_link_target(self) -> None:
        artifact = linked_npm_artifact()
        manifest = self.root / "sdks/typescript/package.json"
        payload = json.loads(manifest.read_text(encoding="utf-8"))
        payload["license"] = "GPL-3.0-only"
        manifest.write_text(json.dumps(payload), encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "license must be"):
            conclude_document(syft_document(artifact), self.root)

    def test_rejects_unknown_first_party_identity(self) -> None:
        artifact = rust_artifact()
        artifact["name"] = "hyphae-untrusted"
        artifact["purl"] = "pkg:cargo/hyphae-untrusted@1.0.1"
        with self.assertRaisesRegex(RuntimeError, "no exact first-party"):
            conclude_document(syft_document(artifact), self.root)

    def test_rejects_truncated_first_party_inventory(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "missing=.*@hyphae_/hyphae"):
            conclude_document(syft_document(rust_artifact()), self.root)

    def test_supplements_manifest_backed_python_package(self) -> None:
        write(
            self.root / "sdks/python/pyproject.toml",
            """[project]
name = "hyphae-sdk"
version = "1.0.1"
license = "Apache-2.0"
dependencies = []
""",
        )
        document = syft_document(rust_artifact(), linked_npm_artifact())

        self.assertEqual(conclude_document(document, self.root), 3)

        python = document["artifacts"][-1]
        self.assertEqual(python["name"], "hyphae-sdk")
        self.assertEqual(python["purl"], "pkg:pypi/hyphae-sdk@1.0.1")
        self.assertEqual(python["foundBy"], "hyphae-manifest-cataloger")
        self.assertEqual(
            [license_value["type"] for license_value in python["licenses"]],
            ["declared", "concluded"],
        )

    def test_file_is_unchanged_when_any_artifact_fails(self) -> None:
        invalid = rust_artifact()
        invalid["name"] = "hyphae-untrusted"
        invalid["purl"] = "pkg:cargo/hyphae-untrusted@1.0.1"
        path = self.root / "sbom.syft.json"
        path.write_text(
            json.dumps(syft_document(rust_artifact(), invalid)), encoding="utf-8"
        )
        before = path.read_bytes()

        with self.assertRaisesRegex(RuntimeError, "no exact first-party"):
            conclude_file(path, self.root)

        self.assertEqual(path.read_bytes(), before)

    def test_private_conformance_tooling_is_not_shipped_first_party_identity(self) -> None:
        write(
            self.root / "conformance/mcp/hosts/package.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "private": True,
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        write(
            self.root / "conformance/mcp/hosts/package-lock.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "packages": {
                        "": {
                            "name": "hyphae-mcp-conformance-hosts",
                            "version": "1.0.0",
                            "license": SOFTWARE_LICENSE,
                        }
                    }
                }
            ),
        )
        document = syft_document(
            rust_artifact(), private_mcp_host_artifact(), linked_npm_artifact()
        )
        document["artifactRelationships"] = [
            {"parent": "rust-id", "child": "private-mcp-host-id"},
            {"parent": "rust-id", "child": "npm-link-id"},
        ]
        self.assertEqual(conclude_document(document, self.root), 2)
        self.assertEqual(
            [artifact["name"] for artifact in document["artifacts"]],
            ["hyphae-core", "@hyphae_/hyphae"],
        )
        self.assertEqual(
            document["artifactRelationships"],
            [{"parent": "rust-id", "child": "npm-link-id"}],
        )

    def test_mcp_tool_name_is_not_excluded_without_private_true(self) -> None:
        write(
            self.root / "conformance/mcp/hosts/package.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "private": False,
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        with self.assertRaisesRegex(RuntimeError, "private=true"):
            conclude_document(
                syft_document(rust_artifact(), linked_npm_artifact()), self.root
            )

    def test_mcp_tool_name_is_not_excluded_at_another_path(self) -> None:
        write(
            self.root / "tools/mcp-hosts/package.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "private": True,
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        with self.assertRaisesRegex(
            RuntimeError, "must use conformance/mcp/hosts/package.json"
        ):
            conclude_document(
                syft_document(rust_artifact(), linked_npm_artifact()), self.root
            )

    def test_mcp_tool_artifact_is_not_excluded_at_another_path(self) -> None:
        write(
            self.root / "conformance/mcp/hosts/package.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "private": True,
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        write(
            self.root / "conformance/mcp/hosts/package-lock.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "packages": {
                        "": {
                            "name": "hyphae-mcp-conformance-hosts",
                            "version": "1.0.0",
                        }
                    },
                }
            ),
        )
        write(
            self.root / "tools/package-lock.json",
            json.dumps({"packages": {}}),
        )
        artifact = private_mcp_host_artifact()
        artifact["locations"] = [{"path": "/tools/package-lock.json"}]
        with self.assertRaisesRegex(RuntimeError, "evidence does not match"):
            conclude_document(
                syft_document(rust_artifact(), artifact, linked_npm_artifact()),
                self.root,
            )

    def test_private_mcp_artifact_requires_exact_inventory(self) -> None:
        write(
            self.root / "conformance/mcp/hosts/package.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "private": True,
                    "license": SOFTWARE_LICENSE,
                }
            ),
        )
        write(
            self.root / "conformance/mcp/hosts/package-lock.json",
            json.dumps(
                {
                    "name": "hyphae-mcp-conformance-hosts",
                    "version": "1.0.0",
                    "packages": {
                        "": {
                            "name": "hyphae-mcp-conformance-hosts",
                            "version": "1.0.0",
                            "license": SOFTWARE_LICENSE,
                        }
                    },
                }
            ),
        )
        artifact = private_mcp_host_artifact()
        artifact["purl"] = "pkg:npm/hyphae-mcp-conformance-hosts@9.9.9"
        with self.assertRaisesRegex(RuntimeError, "inventory does not match"):
            conclude_document(
                syft_document(rust_artifact(), artifact, linked_npm_artifact()),
                self.root,
            )

    def test_private_website_is_not_shipped_first_party_identity(self) -> None:
        write(
            self.root / "website/package.json",
            json.dumps(
                {
                    "name": "hyphae-premium-site",
                    "version": "0.1.0",
                    "private": True,
                    "license": "UNLICENSED",
                }
            ),
        )
        write(
            self.root / "website/package-lock.json",
            json.dumps(
                {
                    "name": "hyphae-premium-site",
                    "version": "0.1.0",
                    "packages": {
                        "": {
                            "name": "hyphae-premium-site",
                            "version": "0.1.0",
                        }
                    },
                }
            ),
        )
        document = syft_document(rust_artifact(), linked_npm_artifact())
        self.assertEqual(conclude_document(document, self.root), 2)


if __name__ == "__main__":
    unittest.main()
