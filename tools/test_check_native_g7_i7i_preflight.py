#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Tests for the dedicated AWS i7i.metal-24xl G7 preflight."""

from __future__ import annotations

import copy
import unittest

from tools.check_native_g7_i7i_preflight import I7iPreflightError, validate_i7i_profile


def i7i_profile() -> dict:
    topology = []
    for logical in range(96):
        physical = logical % 48
        socket = physical // 24
        core = physical % 24
        topology.append(
            {
                "logical_id": logical,
                "core_id": core,
                "socket_id": socket,
                "numa_node_id": socket,
                "thread_siblings": f"{physical},{physical + 48}",
            }
        )
    return {
        "schema": "hyphae-native-hardware-profile-v1",
        "fingerprint": "1" * 64,
        "cpu": {
            "architecture": "x86_64",
            "logical_processors_available": 96,
            "physical_cores_visible": 48,
            "smt_threads_per_core": 2,
            "sockets_visible": 2,
            "numa_nodes_visible": 2,
            "affinity": "0-95",
            "quota_millicores": None,
            "instruction_sets": ["avx2"],
            "caches": [],
            "processor_topology": topology,
            "frequency_governors": ["performance"],
        },
        "memory": {
            "total_bytes": 768 * 1024**3,
            "available_bytes": 700 * 1024**3,
            "page_size_bytes": 4_096,
            "huge_page_size_bytes": 2 * 1024**2,
            "huge_pages_total": 0,
            "numa_nodes": [
                {
                    "id": 0,
                    "cpu_list": "0-23,48-71",
                    "total_bytes": 384 * 1024**3,
                    "available_bytes": None,
                },
                {
                    "id": 1,
                    "cpu_list": "24-47,72-95",
                    "total_bytes": 384 * 1024**3,
                    "available_bytes": None,
                },
            ],
        },
        "storage": {
            "path": "/mnt/hyphae-g7",
            "filesystem": "xfs",
            "device": "259:7",
            "mount_options": ["rw"],
            "rotational": False,
            "queue_depth": 1_023,
            "discard_max_bytes": 0,
        },
        "operating_system": {
            "family": "linux",
            "kernel_release": "test-kernel",
            "virtualization": "none",
            "local_transports": ["embedded", "unix-domain-socket"],
        },
    }


class I7iPreflightTests(unittest.TestCase):
    def test_accepts_exact_dedicated_i7i_profile(self) -> None:
        audit = validate_i7i_profile(i7i_profile())
        self.assertEqual(audit["status"], "passed")
        self.assertEqual(audit["instance_type"], "i7i.metal-24xl")

    def test_rejects_virtualized_or_quota_limited_host(self) -> None:
        for field, value in (("virtualization", "hypervisor"),):
            with self.subTest(field=field):
                profile = i7i_profile()
                profile["operating_system"][field] = value
                with self.assertRaises(I7iPreflightError):
                    validate_i7i_profile(profile)
        profile = i7i_profile()
        profile["cpu"]["quota_millicores"] = 96_000
        with self.assertRaisesRegex(I7iPreflightError, "quota_millicores"):
            validate_i7i_profile(profile)

    def test_rejects_wrong_cpu_shape_or_partial_affinity(self) -> None:
        for field, value in (
            ("logical_processors_available", 95),
            ("physical_cores_visible", 47),
            ("smt_threads_per_core", 1),
        ):
            with self.subTest(field=field):
                profile = i7i_profile()
                profile["cpu"][field] = value
                with self.assertRaises(I7iPreflightError):
                    validate_i7i_profile(profile)

        profile = i7i_profile()
        profile["cpu"]["affinity"] = "0-94"
        with self.assertRaises(I7iPreflightError):
            validate_i7i_profile(profile)

    def test_rejects_memory_outside_tolerance(self) -> None:
        profile = i7i_profile()
        profile["memory"]["total_bytes"] = 700 * 1024**3
        with self.assertRaisesRegex(I7iPreflightError, "768 GiB"):
            validate_i7i_profile(profile)

    def test_rejects_nonlocal_storage_shape(self) -> None:
        for field, value in (
            ("path", "/"),
            ("device", "8:0"),
            ("rotational", True),
        ):
            with self.subTest(field=field):
                profile = copy.deepcopy(i7i_profile())
                profile["storage"][field] = value
                with self.assertRaises(I7iPreflightError):
                    validate_i7i_profile(profile)

    def test_rejects_nonperformance_governor(self) -> None:
        profile = i7i_profile()
        profile["cpu"]["frequency_governors"] = ["powersave"]
        with self.assertRaisesRegex(I7iPreflightError, "frequency_governors"):
            validate_i7i_profile(profile)


if __name__ == "__main__":
    unittest.main()
