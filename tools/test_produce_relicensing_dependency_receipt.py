#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.produce_relicensing_dependency_receipt import produce, source_inputs


class RelicensingDependencyReceiptTests(unittest.TestCase):
    def test_current_exact_source_produces_passing_bounded_receipt(self) -> None:
        def git_result(_root: Path, *arguments: str) -> bytes:
            if arguments == (
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ):
                return b""
            if arguments == ("rev-parse", "HEAD"):
                return b"f" * 40 + b"\n"
            if arguments == ("rev-parse", "HEAD^{tree}"):
                return b"1" * 40 + b"\n"
            raise AssertionError(arguments)

        packages = [
            {
                "license": "Apache-2.0",
                "license_file": None,
                "name": "hyphae-test",
                "source": "workspace:Cargo.toml",
                "version": "1.1.0",
            },
            {
                "license": "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
                "license_file": None,
                "name": "r-efi",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "version": "5.3.0",
            },
        ]
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.produce_relicensing_dependency_receipt.git_bytes",
            side_effect=git_result,
        ), patch(
            "tools.produce_relicensing_dependency_receipt.dependency_inventory",
            return_value=packages,
        ), patch(
            "tools.produce_relicensing_dependency_receipt.source_inputs",
            return_value=[],
        ), patch(
            "tools.produce_relicensing_dependency_receipt.tree_observation",
            return_value={},
        ), patch(
            "tools.produce_relicensing_dependency_receipt.run_observed",
            return_value={"exit_status": 0},
        ), patch(
            "tools.produce_relicensing_dependency_receipt.version",
            return_value="test-tool 1.0",
        ):
            receipt = produce(Path(directory), "2026-08-15T23:00:00Z")
            self.assertEqual(receipt["source"]["commit"], "f" * 40)
            self.assertEqual(receipt["inventory"]["package_count"], 2)
            self.assertEqual(receipt["compatibility_review"]["result"], "pass")
            self.assertEqual(receipt["source"]["mode"], "clean-commit")
            self.assertEqual(
                receipt["source"]["legal_base_commit"],
                "fcf2f918e1539cfb7d67fd52abf0c7d57169ec18",
            )
            self.assertIn(
                "evolved from legal base", receipt["scope"]["evidence_evolution"]
            )
            self.assertEqual(
                receipt["compatibility_review"][
                    "external_strong_copyleft_without_permissive_alternative"
                ],
                [],
            )

    def test_dirty_source_fails_closed_before_dependency_commands(self) -> None:
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.produce_relicensing_dependency_receipt.git_bytes",
            return_value=b" M file\n",
        ):
            with self.assertRaisesRegex(ValueError, "source worktree must be clean"):
                produce(Path(directory), "2026-08-15T23:00:00Z")

    def test_dirty_integration_tree_is_explicit_and_content_bound(self) -> None:
        def git_result(_root: Path, *arguments: str) -> bytes:
            if arguments == (
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ):
                return b" M Cargo.toml\n"
            if arguments == ("rev-parse", "HEAD"):
                return b"f" * 40 + b"\n"
            if arguments == ("rev-parse", "HEAD^{tree}"):
                return b"1" * 40 + b"\n"
            raise AssertionError(arguments)

        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.produce_relicensing_dependency_receipt.git_bytes",
            side_effect=git_result,
        ), patch(
            "tools.produce_relicensing_dependency_receipt.dependency_inventory",
            return_value=[],
        ), patch(
            "tools.produce_relicensing_dependency_receipt.source_inputs",
            return_value=[{"path": "Cargo.toml", "sha256": "0" * 64}],
        ), patch(
            "tools.produce_relicensing_dependency_receipt.tree_observation",
            return_value={},
        ), patch(
            "tools.produce_relicensing_dependency_receipt.run_observed",
            return_value={"exit_status": 0},
        ), patch(
            "tools.produce_relicensing_dependency_receipt.version",
            return_value="test-tool 1.0",
        ):
            receipt = produce(
                Path(directory),
                "2026-08-15T23:00:00Z",
                allow_integration_tree=True,
            )
        self.assertEqual(receipt["source"]["mode"], "integration-tree")
        self.assertFalse(receipt["source"]["worktree_clean"])
        self.assertRegex(receipt["source"]["source_inputs_sha256"], r"^[0-9a-f]{64}$")

    def test_checked_in_receipt_is_current_and_explicit_about_source_mode(self) -> None:
        root = Path(__file__).resolve().parents[1]
        receipt = json.loads(
            (
                root
                / "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json"
            ).read_text(encoding="utf-8")
        )
        self.assertEqual(
            receipt["source"]["commit"],
            "8572af2c2e9ccb95f4ebc8002d3a524efbba67f0",
        )
        self.assertEqual(receipt["source"]["mode"], "integration-tree")
        self.assertFalse(receipt["source"]["worktree_clean"])
        self.assertIn("evolved from legal base", receipt["scope"]["evidence_evolution"])
        self.assertEqual(receipt["inventory"]["package_count"], 299)
        self.assertIn(
            "same-file",
            {package["name"] for package in receipt["inventory"]["packages"]},
        )
        self.assertIn(
            "winapi-util",
            {package["name"] for package in receipt["inventory"]["packages"]},
        )

    def test_source_inputs_include_every_cargo_manifest_at_the_commit(self) -> None:
        root = Path(__file__).resolve().parents[1]
        receipt = json.loads(
            (
                root
                / "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json"
            ).read_text(encoding="utf-8")
        )
        self.assertEqual(
            source_inputs(
                root,
                receipt["source"]["commit"],
                integration_tree=receipt["source"].get("mode") == "integration-tree",
            ),
            receipt["source_inputs"],
        )


if __name__ == "__main__":
    unittest.main()
