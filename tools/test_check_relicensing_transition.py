#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import json
import tempfile
import unittest
import subprocess
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
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(
                ["git", "-C", str(root), "config", "user.name", "Test Owner"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(root), "config", "user.email", "owner@example.test"],
                check=True,
            )
            (root / "content").write_text("base\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "content"], check=True)
            subprocess.run(["git", "-C", str(root), "commit", "-qm", "base"], check=True)
            base = subprocess.check_output(
                ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
            ).strip()
            tree = subprocess.check_output(
                ["git", "-C", str(root), "rev-parse", "HEAD^{tree}"], text=True
            ).strip()
            source = {
                "worktree_state": "content-bound-integration-tree",
                "base_commit": base,
                "base_tree": tree,
                "base_event": {
                    "kind": "interactive-owner-attestation",
                    "evidence": "docs/gates/evidence/relicensing-1.2.0-representative-attestation.json",
                },
            }
            (root / "content").write_text("dirty integration\n", encoding="utf-8")
            self.assertEqual(validate_source_anchor(root, source), [])
            dirty_digest = transitioned_content_digest(root, ["content"], set())
            subprocess.run(["git", "-C", str(root), "add", "content"], check=True)
            subprocess.run(["git", "-C", str(root), "commit", "-qm", "transition"], check=True)
            self.assertEqual(validate_source_anchor(root, source), [])
            self.assertEqual(
                transitioned_content_digest(root, ["content"], set()),
                dirty_digest,
            )

    def test_committed_tree_authority_rejects_dirty_source(self) -> None:
        with patch(
            "tools.check_relicensing_transition._git",
            return_value=" M LICENSE",
        ), self.assertRaisesRegex(ValueError, "exact clean tree"):
            transition_for_committed_tree(ROOT)


if __name__ == "__main__":
    unittest.main()
