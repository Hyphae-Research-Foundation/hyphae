#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_relicensing_transition import (
    _load_receipt,
    transition_for_committed_tree,
    transitioned_content_digest,
    validate_source_anchor,
    validate_transition,
)
from tools.check_relicensing_preflight import ROOT


class RelicensingTransitionTests(unittest.TestCase):
    def test_digest_is_path_sorted_and_content_bound(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "a").write_bytes(b"one")
            (root / "b").write_bytes(b"two")
            first = transitioned_content_digest(root, ["b", "a"], set())
            second = transitioned_content_digest(root, ["a", "b"], set())
            self.assertEqual(first, second)
            (root / "b").write_bytes(b"changed")
            self.assertNotEqual(first, transitioned_content_digest(root, ["a", "b"], set()))

    def test_digest_exclusion_breaks_receipt_cycle(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source").write_bytes(b"source")
            receipt = root / "receipt.json"
            receipt.write_text(json.dumps({"digest": "first"}), encoding="utf-8")
            first = transitioned_content_digest(
                root, ["source", "receipt.json"], {"receipt.json"}
            )
            receipt.write_text(json.dumps({"digest": "second"}), encoding="utf-8")
            second = transitioned_content_digest(
                root, ["receipt.json", "source"], {"receipt.json"}
            )
            self.assertEqual(first, second)

    def test_digest_rejects_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "target").write_bytes(b"target")
            (root / "link").symlink_to("target")
            with self.assertRaisesRegex(ValueError, "regular file"):
                transitioned_content_digest(root, ["link"], set())

    def test_checked_in_transition_is_effective_and_digest_bound(self) -> None:
        self.assertEqual(validate_transition(ROOT), [])

    def test_transition_rejects_receipt_digest_drift(self) -> None:
        value = copy.deepcopy(_load_receipt(ROOT))
        value["transitioned_tree"]["content_sha256"] = "0" * 64
        with patch("tools.check_relicensing_transition._load_receipt", return_value=value), patch(
            "tools.check_relicensing_transition.validate_historical_release_evidence",
            return_value=[],
        ), patch(
            "tools.check_relicensing_transition.validate_preflight_evidence",
            return_value=[],
        ), patch(
            "tools.check_relicensing_transition.validate_repository",
            return_value=[],
        ):
            failures = validate_transition(ROOT)
        self.assertTrue(any("content digest differs" in error for error in failures))

    def test_transition_does_not_silently_skip_marker_capable_files(self) -> None:
        with patch(
            "tools.check_relicensing_transition.repository_machine_files",
            return_value=[ROOT / "DCO"],
        ), patch(
            "tools.check_relicensing_transition.validate_historical_release_evidence",
            return_value=[],
        ), patch(
            "tools.check_relicensing_transition.validate_preflight_evidence",
            return_value=[],
        ), patch(
            "tools.check_relicensing_transition.validate_repository",
            return_value=[],
        ):
            failures = validate_transition(ROOT)
        self.assertTrue(
            any("DCO: classified marker-capable file lacks SPDX marker" in error for error in failures)
        )

    def test_source_anchor_accepts_dirty_and_committed_descendant_content(self) -> None:
        source = copy.deepcopy(_load_receipt(ROOT)["source"])
        self.assertEqual(validate_source_anchor(ROOT, source), [])

    def test_source_anchor_rejects_unrelated_commit_or_tree(self) -> None:
        original = _load_receipt(ROOT)["source"]
        cases = (
            (
                "commit",
                "e88f2ea2c3455a393e3ac0cd69e25486cc26888e",
                "c131ab057c8ab05ed2e2389954f0e8145a71dbdb",
            ),
            (
                "tree",
                original["base_commit"],
                "163633dec3a79931507d184926b10e6cc17722ea",
            ),
        )
        for name, commit, tree in cases:
            with self.subTest(name=name):
                source = copy.deepcopy(original)
                source["base_commit"] = commit
                source["base_tree"] = tree
                self.assertNotEqual(validate_source_anchor(ROOT, source), [])

    def test_committed_tree_authority_rejects_dirty_source(self) -> None:
        with patch(
            "tools.check_relicensing_transition._git",
            return_value=" M LICENSE",
        ), self.assertRaisesRegex(ValueError, "exact clean tree"):
            transition_for_committed_tree(ROOT)


if __name__ == "__main__":
    unittest.main()
