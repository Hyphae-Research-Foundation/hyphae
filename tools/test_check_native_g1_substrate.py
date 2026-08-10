#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import copy
import unittest

from tools.check_native_g1_substrate import GateFailure, validate_metadata


class NativeG1SubstrateTests(unittest.TestCase):
    def metadata(self) -> dict:
        packages = []
        names = [
            "hyphae-native-runtime",
            "hyphae-native-pages",
            "hyphae-native-blobs",
            "hyphae-native-wal",
            "hyphae-native-catalog",
            "hyphae-native-mvcc",
        ]
        for index, name in enumerate(names):
            dependencies = []
            if name == "hyphae-native-runtime":
                dependencies = [{"name": item} for item in names[1:]]
            packages.append(
                {
                    "id": f"path+file:///repo/{name}#0.2.1-{index}",
                    "name": name,
                    "source": None,
                    "dependencies": dependencies,
                }
            )
        return {"packages": packages, "workspace_members": [row["id"] for row in packages]}

    def test_exact_native_kernel_and_runtime_closure_pass(self) -> None:
        result = validate_metadata(self.metadata())
        self.assertEqual(result["kernel_crates"], 5)
        self.assertFalse(result["redb_reachable"])
        self.assertEqual(
            result["runtime_native_dependencies"],
            [
                "hyphae-native-blobs",
                "hyphae-native-catalog",
                "hyphae-native-mvcc",
                "hyphae-native-pages",
                "hyphae-native-wal",
            ],
        )

    def test_missing_kernel_crate_fails_closed(self) -> None:
        metadata = self.metadata()
        metadata["packages"] = [
            row for row in metadata["packages"] if row["name"] != "hyphae-native-wal"
        ]
        with self.assertRaisesRegex(GateFailure, "kernel crate set"):
            validate_metadata(metadata)

    def test_redb_in_runtime_closure_fails_closed(self) -> None:
        metadata = self.metadata()
        runtime = metadata["packages"][0]
        runtime["dependencies"] = [
            dependency for dependency in runtime["dependencies"] if dependency["name"] != "hyphae-native-pages"
        ]
        runtime["dependencies"].append({"name": "hyphae-native-pages"})
        pages = next(row for row in metadata["packages"] if row["name"] == "hyphae-native-pages")
        pages["dependencies"].append({"name": "redb"})
        metadata["packages"].append(
            {
                "id": "registry+https://github.com/rust-lang/crates.io-index#redb@2.6.3",
                "name": "redb",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "dependencies": [],
            }
        )
        with self.assertRaisesRegex(GateFailure, "redb"):
            validate_metadata(metadata)

    def test_unexpected_external_kernel_substitute_fails_closed(self) -> None:
        metadata = copy.deepcopy(self.metadata())
        metadata["packages"][0]["dependencies"].append({"name": "rocksdb"})
        metadata["packages"].append(
            {
                "id": "registry+index#rocksdb@1.0.0",
                "name": "rocksdb",
                "source": "registry+index",
                "dependencies": [],
            }
        )
        with self.assertRaisesRegex(GateFailure, "forbidden storage engine"):
            validate_metadata(metadata)


if __name__ == "__main__":
    unittest.main()
