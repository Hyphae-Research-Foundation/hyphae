#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the Native hardware profile semantic checker."""

from __future__ import annotations

import copy
import unittest

from tools.check_native_hardware_profile import HardwareProfileValidationError, validate_profile


def valid_profile() -> dict:
    processors = []
    for logical, core, node, siblings in (
        (0, 0, 0, "0,4"),
        (1, 1, 0, "1,5"),
        (4, 0, 0, "0,4"),
        (5, 1, 0, "1,5"),
    ):
        processors.append(
            {
                "logical_id": logical,
                "core_id": core,
                "socket_id": 0,
                "numa_node_id": node,
                "thread_siblings": siblings,
            }
        )
    return {
        "schema": "hyphae-native-hardware-profile-v1",
        "fingerprint": "1" * 64,
        "cpu": {
            "architecture": "x86_64",
            "logical_processors_available": 4,
            "physical_cores_visible": 2,
            "smt_threads_per_core": 2,
            "sockets_visible": 1,
            "numa_nodes_visible": 1,
            "affinity": "0-1,4-5",
            "quota_millicores": None,
            "instruction_sets": ["avx2"],
            "caches": [],
            "processor_topology": processors,
            "frequency_governors": ["performance"],
        },
        "memory": {
            "total_bytes": 1_024,
            "available_bytes": 512,
            "page_size_bytes": 4_096,
            "huge_page_size_bytes": None,
            "huge_pages_total": 0,
            "numa_nodes": [
                {
                    "id": 0,
                    "cpu_list": "0-1,4-5",
                    "total_bytes": 1_024,
                    "available_bytes": None,
                }
            ],
        },
        "storage": {
            "path": "/data",
            "filesystem": "xfs",
            "device": "259:0",
            "mount_options": ["rw"],
            "rotational": False,
            "queue_depth": 64,
            "discard_max_bytes": 0,
        },
        "operating_system": {
            "family": "linux",
            "kernel_release": "test-kernel",
            "virtualization": "none",
            "local_transports": ["embedded", "unix-domain-socket"],
        },
    }


class HardwareProfileCheckerTests(unittest.TestCase):
    def test_accepts_consistent_processor_numa_topology(self) -> None:
        validate_profile(valid_profile())

    def test_rejects_topology_outside_affinity(self) -> None:
        profile = valid_profile()
        profile["cpu"]["affinity"] = "0-1,4"
        with self.assertRaisesRegex(HardwareProfileValidationError, "differs from process affinity"):
            validate_profile(profile)

    def test_rejects_cross_core_sibling_group(self) -> None:
        profile = copy.deepcopy(valid_profile())
        profile["cpu"]["processor_topology"][0]["thread_siblings"] = "0-1"
        with self.assertRaisesRegex(HardwareProfileValidationError, "crosses a physical core"):
            validate_profile(profile)

    def test_rejects_numa_mapping_disagreement(self) -> None:
        profile = valid_profile()
        profile["cpu"]["processor_topology"][0]["numa_node_id"] = 1
        with self.assertRaisesRegex(HardwareProfileValidationError, "NUMA CPU lists disagree"):
            validate_profile(profile)

    def test_rejects_noncanonical_cpu_list(self) -> None:
        profile = valid_profile()
        profile["cpu"]["affinity"] = "4-5,0-1"
        with self.assertRaisesRegex(HardwareProfileValidationError, "canonically ordered"):
            validate_profile(profile)


if __name__ == "__main__":
    unittest.main()
