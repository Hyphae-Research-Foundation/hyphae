#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
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
    "numa_steal_policy",
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
    validate_numa_steal_policy(root["numa_steal_policy"], pools)


def validate_numa_steal_policy(value: Any, topology_pools: list[dict[str, Any]]) -> None:
    policy = require_object(
        value,
        "numa_steal_policy",
        {
            "schema",
            "calibration_cache_key",
            "status",
            "working_set_bytes",
            "foreground_burst_limit",
            "pools",
        },
    )
    if policy["schema"] != "hyphae-native-numa-steal-policy-v1":
        fail("NUMA steal policy schema is invalid")
    cache_key = policy["calibration_cache_key"]
    if not isinstance(cache_key, str) or DIGEST.fullmatch(cache_key) is None:
        fail("NUMA steal policy calibration cache key must be one lowercase digest")
    if policy["working_set_bytes"] != 8 * 1024 * 1024:
        fail("NUMA steal policy working set differs from calibration v1")
    if policy["foreground_burst_limit"] != 16:
        fail("NUMA steal policy foreground burst limit must be 16")
    rows = policy["pools"]
    if not isinstance(rows, list) or len(rows) != len(topology_pools):
        fail("NUMA steal policy must contain one row per execution pool")
    topology_nodes = [pool["numa_node_id"] for pool in topology_pools]
    for offset, (row_value, node) in enumerate(zip(rows, topology_nodes)):
        row = require_object(
            row_value,
            f"numa_steal_policy.pools[{offset}]",
            {"worker_numa_node_id", "steal_targets"},
        )
        if row["worker_numa_node_id"] != node:
            fail("NUMA steal policy pool order differs from execution topology")
        targets = row["steal_targets"]
        if not isinstance(targets, list):
            fail("NUMA steal targets must be an array")
        parsed: list[tuple[int, int]] = []
        for target_offset, target_value in enumerate(targets):
            target = require_object(
                target_value,
                f"numa_steal_policy.pools[{offset}].steal_targets[{target_offset}]",
                {
                    "home_numa_node_id",
                    "remote_to_local_latency_ppm",
                    "steal_after_nanoseconds",
                },
            )
            home = require_id(
                target["home_numa_node_id"],
                "home_numa_node_id",
                nullable=False,
            )
            ratio = require_id(
                target["remote_to_local_latency_ppm"],
                "remote_to_local_latency_ppm",
                nullable=False,
            )
            delay = require_id(
                target["steal_after_nanoseconds"],
                "steal_after_nanoseconds",
                nullable=False,
            )
            if home == node:
                fail("NUMA steal policy cannot target its local pool")
            parsed.append((delay, home))
            if ratio == 0:
                fail("NUMA remote/local latency ratio must be positive")
            if policy["status"] == "calibrated" and (ratio <= 1_000_000 or delay == 0):
                fail("calibrated NUMA stealing requires one positive measured threshold")
        if parsed != sorted(set(parsed)):
            fail("NUMA steal targets must be unique and ordered by threshold then node")
        if len({home for _, home in parsed}) != len(parsed):
            fail("NUMA steal targets repeat one home node")

    status = policy["status"]
    all_targets_empty = all(not row["steal_targets"] for row in rows)
    if len(topology_nodes) == 1:
        if status != "not-applicable" or not all_targets_empty:
            fail("single-pool topology requires not-applicable NUMA stealing")
    elif status == "disabled":
        if not all_targets_empty:
            fail("disabled NUMA stealing cannot contain targets")
    elif status == "calibrated":
        if any(node is None for node in topology_nodes):
            fail("calibrated NUMA stealing requires physical node identity")
        expected = [set(topology_nodes) - {node} for node in topology_nodes]
        actual = [
            {target["home_numa_node_id"] for target in row["steal_targets"]}
            for row in rows
        ]
        if actual != expected:
            fail("calibrated NUMA stealing must cover every directed remote target")
    else:
        fail("multi-pool NUMA steal policy status is invalid")


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
