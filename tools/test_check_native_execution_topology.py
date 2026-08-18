#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the Native execution topology semantic checker."""

from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

from tools.check_native_execution_topology import (
    ExecutionTopologyValidationError,
    validate_topology,
)


SCHEMA_PATH = Path("contracts/json-schema/native-execution-topology-v1.schema.json")


def worker(index: int, node: int, logical: int, core: int) -> dict:
    return {
        "worker_index": index,
        "numa_node_id": node,
        "logical_processor_id": logical,
        "socket_id": node,
        "core_id": core,
        "smt_rank": 0,
    }


def valid_topology() -> dict:
    return {
        "schema": "hyphae-native-execution-topology-v1",
        "hardware_fingerprint": "1" * 64,
        "schedulable_compute_threads": 4,
        "hard_affinity": True,
        "pools": [
            {"numa_node_id": 0, "workers": [worker(0, 0, 0, 0), worker(1, 0, 2, 1)]},
            {"numa_node_id": 1, "workers": [worker(2, 1, 4, 0), worker(3, 1, 6, 1)]},
        ],
        "numa_steal_policy": {
            "schema": "hyphae-native-numa-steal-policy-v1",
            "calibration_cache_key": "2" * 64,
            "status": "calibrated",
            "working_set_bytes": 8 * 1024 * 1024,
            "foreground_burst_limit": 16,
            "pools": [
                {
                    "worker_numa_node_id": 0,
                    "steal_targets": [{
                        "home_numa_node_id": 1,
                        "remote_to_local_latency_ppm": 2_000_000,
                        "steal_after_nanoseconds": 1_000,
                    }],
                },
                {
                    "worker_numa_node_id": 1,
                    "steal_targets": [{
                        "home_numa_node_id": 0,
                        "remote_to_local_latency_ppm": 2_000_000,
                        "steal_after_nanoseconds": 1_000,
                    }],
                },
            ],
        },
    }


class ExecutionTopologyCheckerTests(unittest.TestCase):
    def test_accepts_physical_and_portable_topologies(self) -> None:
        physical = valid_topology()
        validate_topology(physical)
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        self.assertEqual(set(physical), set(schema["required"]))
        portable = valid_topology()
        portable["hard_affinity"] = False
        portable["pools"] = [{
            "numa_node_id": None,
            "workers": [
                {
                    "worker_index": index,
                    "numa_node_id": None,
                    "logical_processor_id": None,
                    "socket_id": None,
                    "core_id": None,
                    "smt_rank": None,
                }
                for index in range(4)
            ],
        }]
        portable["numa_steal_policy"].update({
            "status": "not-applicable",
            "pools": [{"worker_numa_node_id": None, "steal_targets": []}],
        })
        validate_topology(portable)

    def test_rejects_worker_gaps_and_cross_node_placement(self) -> None:
        topology = valid_topology()
        topology["pools"][1]["workers"][0]["worker_index"] = 7
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "worker indices"):
            validate_topology(topology)
        topology = valid_topology()
        topology["pools"][1]["workers"][0]["numa_node_id"] = 0
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "different NUMA"):
            validate_topology(topology)

    def test_rejects_duplicate_processors_and_partial_placement(self) -> None:
        topology = copy.deepcopy(valid_topology())
        topology["pools"][0]["workers"][1]["logical_processor_id"] = 0
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "logical processor"):
            validate_topology(topology)
        topology = valid_topology()
        topology["pools"][0]["workers"][0]["core_id"] = None
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "partial physical"):
            validate_topology(topology)

    def test_rejects_incomplete_portable_and_noncanonical_smt_rank(self) -> None:
        topology = valid_topology()
        topology["hard_affinity"] = False
        topology["pools"][0]["workers"][0].update({
            "logical_processor_id": None,
            "socket_id": None,
            "core_id": None,
            "smt_rank": None,
        })
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "portable placement"):
            validate_topology(topology)
        topology = valid_topology()
        topology["pools"][0]["workers"][0]["smt_rank"] = 1
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "SMT ranks"):
            validate_topology(topology)

    def test_rejects_incomplete_or_misordered_numa_steal_targets(self) -> None:
        topology = valid_topology()
        topology["numa_steal_policy"]["pools"][0]["steal_targets"] = []
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "every directed"):
            validate_topology(topology)

        topology = valid_topology()
        topology["numa_steal_policy"]["pools"][0]["worker_numa_node_id"] = 1
        with self.assertRaisesRegex(ExecutionTopologyValidationError, "pool order"):
            validate_topology(topology)


if __name__ == "__main__":
    unittest.main()
