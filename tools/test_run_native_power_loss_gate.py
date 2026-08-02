#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the non-privileged power-loss harness boundaries."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from subprocess import CompletedProcess
from unittest.mock import patch

from tools.run_native_power_loss_gate import (
    assert_owned_path,
    normalize_loop_backing,
    mounted_source,
    require_loop_device,
    validate_label,
    validate_mapper_name,
)


class NativePowerLossGateSafetyTests(unittest.TestCase):
    def test_receipt_labels_reject_shell_and_json_metacharacters(self) -> None:
        self.assertEqual(validate_label("environment", "aws-ext4:dm_log-1"), "aws-ext4:dm_log-1")
        for value in ("", "../escape", "value with space", 'quote"', "x" * 129):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    validate_label("environment", value)

    def test_mapper_names_are_owned_by_the_harness_namespace(self) -> None:
        self.assertEqual(validate_mapper_name("hyphae-pl-123-7"), "hyphae-pl-123-7")
        for value in ("hyphae", "hyphae-pl-x-7", "../hyphae-pl-1-1", "hyphae-pl-1-1/extra"):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    validate_mapper_name(value)

    def test_loop_device_parser_rejects_non_loop_targets(self) -> None:
        self.assertEqual(require_loop_device("/dev/loop17\n"), "/dev/loop17")
        for value in ("/dev/nvme0n1p1", "/dev/mapper/root", "loop7", "/dev/loop7/child"):
            with self.subTest(value=value):
                with self.assertRaises(RuntimeError):
                    require_loop_device(value)

    def test_owned_paths_cannot_equal_or_escape_the_unique_root(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            child = root / "scenario"
            child.mkdir()
            self.assertEqual(assert_owned_path(root, child), child)
            with self.assertRaises(RuntimeError):
                assert_owned_path(root, root)
            with self.assertRaises(RuntimeError):
                assert_owned_path(root, root.parent)

    def test_deleted_suffix_is_normalized_without_changing_the_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary, "device.img")
            image.touch()
            self.assertEqual(
                normalize_loop_backing(f"{image} (deleted)\n"),
                image.resolve(),
            )

    @patch("tools.run_native_power_loss_gate.run")
    def test_mount_detection_requires_an_exact_mountpoint(self, run_mock) -> None:
        run_mock.return_value = CompletedProcess((), 1, "", "")
        mountpoint = Path("/var/tmp/hyphae-power-loss-test/mount")
        self.assertIsNone(mounted_source(mountpoint))
        arguments = run_mock.call_args.args[0]
        self.assertIn("--mountpoint", arguments)
        self.assertNotIn("--target", arguments)


if __name__ == "__main__":
    unittest.main()
