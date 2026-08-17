#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_dependency_license_aggregate import (
    AGGREGATE_PATH,
    LGPL_OBLIGATIONS,
    NPM_LOCK_PATHS,
    PROPRIETARY_TOOLING_EXCEPTION,
    ROOT,
    _npm_inventory,
    validate_aggregate,
)


class DependencyLicenseAggregateTests(unittest.TestCase):
    def test_checked_in_aggregate_covers_every_ecosystem(self) -> None:
        self.assertEqual(validate_aggregate(ROOT), [])
        aggregate = json.loads((ROOT / AGGREGATE_PATH).read_text(encoding="utf-8"))
        self.assertEqual(
            [item["lock"] for item in aggregate["inventories"]["npm"]],
            [path.as_posix() for path in NPM_LOCK_PATHS],
        )
        self.assertEqual(aggregate["inventories"]["rust"]["result"], "pass")
        self.assertEqual(aggregate["inventories"]["python"]["result"], "pass")

    def test_generic_incompatible_or_unknown_license_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lock = root / NPM_LOCK_PATHS[-1]
            lock.parent.mkdir(parents=True)
            lock.write_text(
                json.dumps(
                    {
                        "packages": {
                            "": {"license": "Apache-2.0"},
                            "node_modules/hostile": {
                                "version": "1.0.0",
                                "license": "GPL-3.0-only",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            _, failures = _npm_inventory(root, NPM_LOCK_PATHS[-1])
        self.assertTrue(any("unreviewed or incompatible" in failure for failure in failures))

    def test_new_npm_or_python_inventory_cannot_escape_the_aggregate(self) -> None:
        aggregate = json.loads((ROOT / AGGREGATE_PATH).read_text(encoding="utf-8"))
        npm_locks = {item["lock"] for item in aggregate["inventories"]["npm"]}
        self.assertEqual(npm_locks, {path.as_posix() for path in NPM_LOCK_PATHS})
        python = aggregate["inventories"]["python"]
        self.assertEqual(python["inventory"], "sdks/python/build-dependencies.json")
        self.assertEqual(python["manifest"], "sdks/python/pyproject.toml")

    def test_proprietary_exception_is_exact_in_identity_version_and_scope(self) -> None:
        self.assertEqual(len(PROPRIETARY_TOOLING_EXCEPTION), 9)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lock = root / NPM_LOCK_PATHS[0]
            lock.parent.mkdir(parents=True)
            lock.write_text(
                json.dumps(
                    {
                        "packages": {
                            "": {"license": "Apache-2.0"},
                            "node_modules/unreviewed": {
                                "version": "2.1.233",
                                "license": "SEE LICENSE IN LICENSE.md",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            _, failures = _npm_inventory(root, NPM_LOCK_PATHS[0])
        self.assertTrue(any("not an exact tooling exception" in failure for failure in failures))

    def test_lgpl_obligations_are_exact_and_distribution_safe(self) -> None:
        self.assertEqual(len(LGPL_OBLIGATIONS), 4)
        joined = " ".join(LGPL_OBLIGATIONS)
        for required in (
            "license text",
            "corresponding source",
            "replace or relink",
            "Do not include",
        ):
            self.assertIn(required, joined)


if __name__ == "__main__":
    unittest.main()
