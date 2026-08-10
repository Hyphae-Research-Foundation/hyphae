#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import unittest

from tools.check_crate_packages import validate_release_graph


def package(name: str, version: str, dependencies: list[dict[str, str]] | None = None):
    return {
        "name": name,
        "version": version,
        "dependencies": dependencies or [],
    }


class ReleaseGraphTests(unittest.TestCase):
    def test_accepts_exact_acyclic_release_graph(self) -> None:
        release = {"version": "1.0.1", "layers": [["base"], ["product"]]}
        packages = {
            "base": package("base", "1.0.1"),
            "product": package(
                "product",
                "1.0.1",
                [{"name": "base", "kind": None, "req": "=1.0.1"}],
            ),
        }
        ordered, failures = validate_release_graph(
            release, packages, ("base", "product")
        )
        self.assertEqual(ordered, ("base", "product"))
        self.assertEqual(failures, [])

    def test_rejects_version_requirement_and_layer_drift(self) -> None:
        release = {"version": "1.0.1", "layers": [["base", "product"]]}
        packages = {
            "base": package("base", "1.0.0"),
            "product": package(
                "product",
                "1.0.1",
                [{"name": "base", "kind": None, "req": "^1.0"}],
            ),
        }
        _, failures = validate_release_graph(release, packages, ("base", "product"))
        self.assertEqual(len(failures), 3)
        self.assertTrue(any("version 1.0.0" in failure for failure in failures))
        self.assertTrue(any("requirement ^1.0" in failure for failure in failures))
        self.assertTrue(any("earlier release layer" in failure for failure in failures))

    def test_rejects_publishable_set_drift_but_ignores_dev_cycles(self) -> None:
        release = {"version": "1.0.1", "layers": [["base"], ["product"]]}
        packages = {
            "base": package(
                "base",
                "1.0.1",
                [{"name": "product", "kind": "dev", "req": "=1.0.1"}],
            ),
            "product": package("product", "1.0.1"),
        }
        _, failures = validate_release_graph(release, packages, ("base",))
        self.assertEqual(len(failures), 1)
        self.assertIn("publishable crate set differs", failures[0])

    def test_rejects_invalid_semver_baseline_packages(self) -> None:
        release = {
            "version": "1.0.1",
            "layers": [["base"]],
            "semver_baseline_packages": ["base", "missing"],
        }
        packages = {"base": package("base", "1.0.1")}
        _, failures = validate_release_graph(release, packages, ("base",))
        self.assertEqual(
            failures,
            ["semver baseline packages must belong to the release closure"],
        )


if __name__ == "__main__":
    unittest.main()
