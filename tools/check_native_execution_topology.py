#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Fail-closed semantic checker for Native execution topology v1."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-execution-topology-v1"
DIGEST = re.compile(r"^[0-9a-f]{64}$")
ROOT_KEYS = {
    "schema",
    "hardware_fingerprint",
    "schedulable_compute_threads",
    "hard_affinity",
    "pools",
}
POOL_KEYS = {"numa_node_id", "workers"}
WORKER_KEYS = {
    "worker_index",
    "numa_node_id",
    "logical_processor_id",
    "socket_id",
    "core_id",
    "smt_rank",
}


class ExecutionTopologyValidationError(ValueError):
    """An execution topology violates its versioned semantic contract."""


def fail(message: str) -> None:
    raise ExecutionTopologyValidationError(message)


def require_object(value: Any, field: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be an object")
    actual = set(value)
    if actual != keys:
        fail(f"{field} keys differ: missing={sorted(keys - actual)} extra={sorted(actual - keys)}")
    return value


def require_id(value: Any, field: str, *, nullable: bool = True) -> int | None:
    if value is None and nullable:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        fail(f"{field} must be a nonnegative integer{' or null' if nullable else ''}")
    return value


def validate_topology(value: Any) -> None:
    root = require_object(value, "topology", ROOT_KEYS)
    if root["schema"] != SCHEMA:
        fail("topology schema is not Native execution topology v1")
    fingerprint = root["hardware_fingerprint"]
    if not isinstance(fingerprint, str) or DIGEST.fullmatch(fingerprint) is None:
        fail("hardware_fingerprint must be one lowercase digest")
    expected_workers = require_id(
        root["schedulable_compute_threads"],
        "schedulable_compute_threads",
        nullable=False,
    )
    if expected_workers == 0:
        fail("schedulable_compute_threads must be positive")
    hard_affinity = root["hard_affinity"]
    if not isinstance(hard_affinity, bool):
        fail("hard_affinity must be boolean")
    pools = root["pools"]
    if not isinstance(pools, list) or not pools:
        fail("pools must be a nonempty array")

    nodes: set[int | None] = set()
    worker_indices: list[int] = []
    logical_processors: set[int] = set()
    physical_ranks: dict[tuple[int, int], set[int]] = {}
    portable_workers = 0
    for pool_offset, pool_value in enumerate(pools):
        pool = require_object(pool_value, f"pools[{pool_offset}]", POOL_KEYS)
        node = require_id(pool["numa_node_id"], f"pools[{pool_offset}].numa_node_id")
        if node in nodes:
            fail("NUMA pool identities must be unique")
        nodes.add(node)
        workers = pool["workers"]
        if not isinstance(workers, list) or not workers:
            fail(f"pools[{pool_offset}].workers must be a nonempty array")
        for worker_offset, worker_value in enumerate(workers):
            field = f"pools[{pool_offset}].workers[{worker_offset}]"
            worker = require_object(worker_value, field, WORKER_KEYS)
            worker_index = require_id(worker["worker_index"], f"{field}.worker_index", nullable=False)
            worker_indices.append(worker_index)
            worker_node = require_id(worker["numa_node_id"], f"{field}.numa_node_id")
            if worker_node != node:
                fail(f"{field} belongs to a different NUMA node")
            logical = require_id(worker["logical_processor_id"], f"{field}.logical_processor_id")
            socket = require_id(worker["socket_id"], f"{field}.socket_id")
            core = require_id(worker["core_id"], f"{field}.core_id")
            rank = require_id(worker["smt_rank"], f"{field}.smt_rank")
            placement = (logical, socket, core, rank)
            if all(component is None for component in placement):
                portable_workers += 1
                continue
            if any(component is None for component in placement):
                fail(f"{field} has partial physical placement")
            if logical in logical_processors:
                fail("logical processor identities must be unique")
            logical_processors.add(logical)
            physical_ranks.setdefault((socket, core), set()).add(rank)

    if worker_indices != list(range(expected_workers)):
        fail("worker indices must be complete, unique, and canonically ordered")
    if portable_workers:
        if portable_workers != expected_workers or len(pools) != 1 or next(iter(nodes)) is not None:
            fail("portable placement must use one complete null NUMA pool")
        if hard_affinity:
            fail("portable placement cannot claim hard affinity")
    elif hard_affinity and len(logical_processors) != expected_workers:
        fail("hard affinity requires every logical processor identity")
    for ranks in physical_ranks.values():
        if ranks != set(range(max(ranks) + 1)):
            fail("SMT ranks for one physical core must be contiguous from zero")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--topology", required=True, type=Path)
    args = parser.parse_args()
    try:
        with args.topology.open(encoding="utf-8") as handle:
            validate_topology(json.load(handle))
    except (OSError, json.JSONDecodeError, ExecutionTopologyValidationError) as error:
        print(f"native execution topology check failed: {error}")
        return 1
    print(f"native execution topology check passed: {args.topology}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
