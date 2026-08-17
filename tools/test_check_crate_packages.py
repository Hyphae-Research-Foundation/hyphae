#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import unittest

from tools.check_crate_packages import validate_release_graph


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

if __name__ == "__main__":
    unittest.main()
