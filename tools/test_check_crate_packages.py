#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import tempfile
import unittest
from pathlib import Path

from tools.check_crate_packages import validate_release_graph
from tools.verify_crate_packages import (
    locked_registry_packages,
    validate_local_resolution,
    validate_locked_registry_resolution,
    verification_manifest,
)


def package(
    name: str,
    version: str,
    dependencies: list[dict[str, object]] | None = None,
):
    return {
        "name": name,
        "version": version,
        "dependencies": dependencies or [],
    }


class ReleaseGraphTests(unittest.TestCase):
    def test_crate_package_checker_requires_third_party_notices(self) -> None:
        source = __import__("pathlib").Path(__file__).with_name("check_crate_packages.py").read_text()
        self.assertIn('"THIRD_PARTY_NOTICES.md"', source)

    def test_accepts_exact_acyclic_release_graph(self) -> None:
        release = {"version": "1.2.0", "layers": [["base"], ["product"]]}
        packages = {
            "base": package("base", "1.2.0"),
            "product": package(
                "product",
                "1.2.0",
                [{"name": "base", "kind": None, "req": "=1.2.0"}],
            ),
        }
        ordered, failures = validate_release_graph(
            release, packages, ("base", "product")
        )
        self.assertEqual(ordered, ("base", "product"))
        self.assertEqual(failures, [])

    def test_rejects_version_requirement_and_layer_drift(self) -> None:
        release = {"version": "1.2.0", "layers": [["base", "product"]]}
        packages = {
            "base": package("base", "1.1.0"),
            "product": package(
                "product",
                "1.2.0",
                [{"name": "base", "kind": None, "req": "^1.0"}],
            ),
        }
        _, failures = validate_release_graph(release, packages, ("base", "product"))
        self.assertEqual(len(failures), 3)
        self.assertTrue(any("version 1.1.0" in failure for failure in failures))
        self.assertTrue(any("requirement ^1.0" in failure for failure in failures))
        self.assertTrue(any("earlier release layer" in failure for failure in failures))

    def test_rejects_versioned_development_dependency_cycle(self) -> None:
        release = {"version": "1.2.0", "layers": [["base", "product"]]}
        packages = {
            "base": package(
                "base",
                "1.2.0",
                [{"name": "product", "kind": "dev", "req": "=1.2.0"}],
            ),
            "product": package("product", "1.2.0"),
        }
        _, failures = validate_release_graph(release, packages, ("base", "product"))
        self.assertEqual(len(failures), 1)
        self.assertIn("must be in an earlier release layer", failures[0])

    def test_accepts_path_only_development_dependency(self) -> None:
        release = {"version": "1.2.0", "layers": [["base", "product"]]}
        packages = {
            "base": package(
                "base",
                "1.2.0",
                [
                    {
                        "name": "product",
                        "kind": "dev",
                        "req": "*",
                        "path": "/workspace/product",
                    }
                ],
            ),
            "product": package("product", "1.2.0"),
        }
        _, failures = validate_release_graph(release, packages, ("base", "product"))
        self.assertEqual(failures, [])

    def test_rejects_invalid_semver_baseline_packages(self) -> None:
        release = {
            "version": "1.2.0",
            "layers": [["base"]],
            "semver_baseline_packages": ["base", "missing"],
        }
        packages = {"base": package("base", "1.2.0")}
        _, failures = validate_release_graph(release, packages, ("base",))
        self.assertEqual(
            failures,
            ["semver baseline packages must belong to the release closure"],
        )

    def test_verification_manifest_patches_every_extracted_package(self) -> None:
        manifest = verification_manifest(("base", "product"), "1.2.0")
        self.assertIn('"base" = { path = "packages/base-1.2.0" }', manifest)
        self.assertIn('"product" = { path = "packages/product-1.2.0" }', manifest)

    def test_local_resolution_rejects_registry_or_workspace_sources(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = (root / "base-1.2.0").resolve()
            product = (root / "product-1.2.0").resolve()
            metadata = {
                "packages": [
                    {
                        "name": "base",
                        "source": None,
                        "manifest_path": str(base / "Cargo.toml"),
                    },
                    {
                        "name": "product",
                        "source": "registry+https://github.com/rust-lang/crates.io-index",
                        "manifest_path": str(product / "Cargo.toml"),
                    },
                ]
            }
            failures = validate_local_resolution(
                metadata,
                ("base", "product"),
                {"base": base, "product": product},
            )
        self.assertEqual(failures, ["product: verification resolved a registry package"])

    def test_locked_resolution_rejects_versions_absent_from_the_lockfile(self) -> None:
        registry = "registry+https://github.com/rust-lang/crates.io-index"
        with tempfile.TemporaryDirectory() as directory:
            lockfile = Path(directory) / "Cargo.lock"
            lockfile.write_text(
                'version = 4\n\n'
                '[[package]]\nname = "base"\nversion = "1.2.0"\n\n'
                '[[package]]\nname = "tinyvec"\nversion = "1.12.0"\n'
                f'source = "{registry}"\n',
                encoding="utf-8",
            )
            locked = locked_registry_packages(lockfile)
        self.assertEqual(locked, {("tinyvec", "1.12.0")})
        metadata = {
            "packages": [
                {"name": "base", "version": "1.2.0", "source": None},
                {"name": "tinyvec", "version": "1.12.0", "source": registry},
                {"name": "tinyvec", "version": "1.13.0", "source": registry},
                {"name": "vendored", "version": "0.1.0", "source": None},
            ]
        }
        failures = validate_locked_registry_resolution(metadata, ("base",), locked)
        self.assertEqual(
            failures,
            [
                "tinyvec 1.13.0: verification resolved a registry copy absent from Cargo.lock",
                "vendored: verification resolved a non-registry dependency",
            ],
        )

if __name__ == "__main__":
    unittest.main()
