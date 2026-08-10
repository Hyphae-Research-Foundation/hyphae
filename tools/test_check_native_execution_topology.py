#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Tests for the Native execution topology semantic checker."""

from __future__ import annotations

import copy
import unittest

from tools.check_native_execution_topology import (
    ExecutionTopologyValidationError,
    validate_topology,
)


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
    }


class ExecutionTopologyCheckerTests(unittest.TestCase):
    def test_accepts_physical_and_portable_topologies(self) -> None:
        validate_topology(valid_topology())
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


if __name__ == "__main__":
    unittest.main()
