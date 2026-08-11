#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed composite checker for Native scheduler authority v1."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from collections import defaultdict
from pathlib import Path
from typing import Any

from tools.check_native_execution_topology import validate_topology
from tools.check_native_governor_policy import validate_policy
from tools.check_native_hardware_calibration import validate_numa_measurements, validate_receipt
from tools.check_native_hardware_profile import validate_profile


SCHEMA = "hyphae-native-scheduler-authority-v1"
COMMIT = re.compile(r"^[0-9a-f]{40}$")
DIGEST = re.compile(r"^[0-9a-f]{64}$")
ROOT = Path(__file__).resolve().parents[1]


class SchedulerAuthorityValidationError(ValueError):
    """Scheduler artifacts do not form one canonical measured authority."""


def fail(message: str) -> None:
    raise SchedulerAuthorityValidationError(message)


def canonical_sha256(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def source_authority(repository: Path, expected_commit: str) -> tuple[str, str]:
    if COMMIT.fullmatch(expected_commit) is None:
        fail("expected source commit must be one lowercase 40-character SHA")

    def git(*arguments: str) -> str:
        completed = subprocess.run(
            ("git", *arguments),
            cwd=repository,
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode != 0:
            fail(f"git source authority failed: {completed.stderr.strip()}")
        return completed.stdout.strip()

    commit = git("rev-parse", "HEAD")
    tree = git("rev-parse", "HEAD^{tree}")
    if commit != expected_commit:
        fail("checked-out source commit differs from the expected exact SHA")
    if COMMIT.fullmatch(tree) is None:
        fail("checked-out source tree is not canonical")
    if git("status", "--porcelain"):
        fail("exact-source scheduler authority requires a clean worktree")
    return commit, tree


def executable_blake3(executable: Path) -> str:
    try:
        from blake3 import blake3
    except ImportError as error:
        raise SchedulerAuthorityValidationError(
            "executable verification requires the pinned blake3 Python package"
        ) from error
    hasher = blake3()
    with executable.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1_024 * 1_024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def expected_memory_bytes(profile: dict[str, Any]) -> int:
    total = profile["memory"]["total_bytes"]
    if isinstance(total, bool) or not isinstance(total, int) or total <= 0:
        fail("profile does not provide positive total memory")
    headroom = total * 15 // 100
    static_limit = max(0, total - headroom)
    available = profile["memory"]["available_bytes"]
    if available is None:
        return static_limit
    if isinstance(available, bool) or not isinstance(available, int):
        fail("profile available memory is invalid")
    return min(static_limit, max(0, available - headroom))


def expected_io_slots(calibration: dict[str, Any]) -> int:
    scaling = calibration["io_scaling"]
    if scaling["status"] != "stable":
        return 1
    recommended = scaling["recommended_io_slots"]
    if isinstance(recommended, bool) or not isinstance(recommended, int):
        fail("stable calibration omitted its I/O recommendation")
    return max(1, min(64, recommended))


def smt_rank_by_processor(profile: dict[str, Any]) -> dict[int, int]:
    siblings: dict[tuple[int, int], list[int]] = defaultdict(list)
    for processor in profile["cpu"]["processor_topology"]:
        siblings[(processor["socket_id"], processor["core_id"])].append(
            processor["logical_id"]
        )
    ranks: dict[int, int] = {}
    for logical_ids in siblings.values():
        for rank, logical_id in enumerate(sorted(logical_ids)):
            ranks[logical_id] = rank
    return ranks


def expected_topology(
    profile: dict[str, Any], policy: dict[str, Any], calibration: dict[str, Any]
) -> dict[str, Any]:
    worker_count = policy["schedulable_compute_threads"]
    processors = profile["cpu"]["processor_topology"]
    if not processors:
        pools = [
            {
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
                    for index in range(worker_count)
                ],
            }
        ]
        return {
            "schema": "hyphae-native-execution-topology-v1",
            "hardware_fingerprint": profile["fingerprint"],
            "schedulable_compute_threads": worker_count,
            "hard_affinity": False,
            "pools": pools,
            "numa_steal_policy": expected_numa_steal_policy(pools, policy, calibration),
        }

    ranks = smt_rank_by_processor(profile)
    candidates = sorted(
        processors,
        key=lambda processor: (
            ranks[processor["logical_id"]],
            processor["core_id"],
            -1 if processor["numa_node_id"] is None else processor["numa_node_id"],
            processor["socket_id"],
            processor["logical_id"],
        ),
    )
    if worker_count > len(candidates):
        fail("policy worker count exceeds discovered processor topology")
    grouped: dict[int | None, list[dict[str, Any]]] = defaultdict(list)
    for processor in candidates[:worker_count]:
        node = processor["numa_node_id"]
        grouped[node].append(
            {
                "worker_index": 0,
                "numa_node_id": node,
                "logical_processor_id": processor["logical_id"],
                "socket_id": processor["socket_id"],
                "core_id": processor["core_id"],
                "smt_rank": ranks[processor["logical_id"]],
            }
        )
    pools = [
        {"numa_node_id": node, "workers": grouped[node]}
        for node in sorted(grouped, key=lambda value: -1 if value is None else value)
    ]
    for worker_index, worker in enumerate(
        worker for pool in pools for worker in pool["workers"]
    ):
        worker["worker_index"] = worker_index
    return {
        "schema": "hyphae-native-execution-topology-v1",
        "hardware_fingerprint": profile["fingerprint"],
        "schedulable_compute_threads": worker_count,
        "hard_affinity": profile["operating_system"]["family"] == "linux",
        "pools": pools,
        "numa_steal_policy": expected_numa_steal_policy(pools, policy, calibration),
    }


def expected_numa_steal_policy(
    pools: list[dict[str, Any]],
    policy: dict[str, Any],
    calibration: dict[str, Any],
) -> dict[str, Any]:
    nodes = [pool["numa_node_id"] for pool in pools]
    base = {
        "schema": "hyphae-native-numa-steal-policy-v1",
        "calibration_cache_key": policy["calibration_cache_key"],
        "status": "not-applicable",
        "working_set_bytes": 8 * 1024 * 1024,
        "foreground_burst_limit": policy["foreground_burst_limit"],
        "pools": [
            {"worker_numa_node_id": node, "steal_targets": []} for node in nodes
        ],
    }
    if len(nodes) <= 1:
        return base
    if any(node is None for node in nodes):
        fail("multi-pool topology lacks complete NUMA node identity")
    matrix = validate_numa_measurements(calibration["measurements"])
    if matrix is None:
        unsupported = {
            item["primitive"] for item in calibration["coverage"]["unsupported"]
        }
        if "numa-local-remote-memory" not in unsupported:
            fail("multi-node scheduler authority lacks NUMA calibration")
        base["status"] = "disabled"
        return base
    required = {(source, reader) for source in nodes for reader in nodes}
    if not required.issubset(matrix):
        fail("NUMA calibration does not cover every execution topology node")
    base["status"] = "calibrated"
    for row, worker_node in zip(base["pools"], nodes):
        targets = []
        for home_node in nodes:
            if home_node == worker_node:
                continue
            local = matrix[(home_node, home_node)]["statistics"]["median"]
            remote = matrix[(home_node, worker_node)]["statistics"]["median"]
            if remote <= local:
                fail("remote NUMA calibration does not establish a positive steal threshold")
            targets.append({
                "home_numa_node_id": home_node,
                "remote_to_local_latency_ppm": (remote * 1_000_000 + local - 1) // local,
                "steal_after_nanoseconds": (max(0, remote - local) + 999) // 1_000,
            })
        row["steal_targets"] = sorted(
            targets,
            key=lambda target: (
                target["steal_after_nanoseconds"],
                target["home_numa_node_id"],
            ),
        )
    return base


def validate_authority(
    profile: Any,
    calibration: Any,
    policy: Any,
    topology: Any,
    mode: str,
    expected_commit: str,
    expected_source_tree: str,
    expected_executable_blake3: str,
) -> dict[str, Any]:
    validate_profile(profile)
    validate_receipt(calibration)
    validate_policy(policy)
    validate_topology(topology)
    if COMMIT.fullmatch(expected_commit) is None:
        fail("expected source commit must be one lowercase 40-character SHA")
    if COMMIT.fullmatch(expected_source_tree) is None:
        fail("expected source tree must be one lowercase 40-character SHA")
    if DIGEST.fullmatch(expected_executable_blake3) is None:
        fail("expected executable BLAKE3 must be one lowercase digest")
    if mode not in {"latency", "bulk", "mixed"}:
        fail("governor mode is invalid")
    if calibration["status"] != "stable" or calibration["accepted_for_scheduling"] is not True:
        fail("calibration is not accepted for scheduling")
    if calibration["identity"]["executable_blake3"] != expected_executable_blake3:
        fail("calibration targets another executable")

    fingerprint = profile["fingerprint"]
    if {
        calibration["identity"]["hardware_fingerprint"],
        policy["hardware_fingerprint"],
        topology["hardware_fingerprint"],
    } != {fingerprint}:
        fail("scheduler artifacts target different hardware fingerprints")
    cache_key = calibration["identity"]["cache_key"]
    if policy["calibration_cache_key"] != cache_key:
        fail("governor policy targets another calibration cache key")
    if policy["mode"] != mode:
        fail("governor policy mode differs from the requested authority mode")

    recommended_workers = calibration["thread_scaling"]["recommended_worker_count"]
    if policy["calibrated_worker_limit"] != recommended_workers:
        fail("governor worker limit differs from the calibrated recommendation")
    if policy["io_slots"] != expected_io_slots(calibration):
        fail("governor I/O limit differs from the calibrated recommendation")
    memory_bytes = expected_memory_bytes(profile)
    if memory_bytes <= 0 or policy["memory_bytes"] != memory_bytes:
        fail("governor memory limit differs from profile headroom")

    derived_topology = expected_topology(profile, policy, calibration)
    if topology != derived_topology:
        fail("execution topology is not the canonical physical-core-first derivation")
    numa_policy = topology["numa_steal_policy"]
    numa_thresholds = [
        {
            "worker_numa_node_id": row["worker_numa_node_id"],
            **target,
        }
        for row in numa_policy["pools"]
        for target in row["steal_targets"]
    ]

    return {
        "schema": SCHEMA,
        "source_commit": expected_commit,
        "source_tree": expected_source_tree,
        "executable_blake3": expected_executable_blake3,
        "mode": mode,
        "hardware_fingerprint": fingerprint,
        "calibration_cache_key": cache_key,
        "input_canonical_sha256": {
            "profile": canonical_sha256(profile),
            "calibration": canonical_sha256(calibration),
            "policy": canonical_sha256(policy),
            "topology": canonical_sha256(topology),
        },
        "calibrated_worker_limit": policy["calibrated_worker_limit"],
        "schedulable_compute_threads": policy["schedulable_compute_threads"],
        "io_slots": policy["io_slots"],
        "memory_bytes": memory_bytes,
        "numa_pools": [pool["numa_node_id"] for pool in topology["pools"]],
        "hard_affinity": topology["hard_affinity"],
        "numa_steal_policy_schema": numa_policy["schema"],
        "numa_steal_status": numa_policy["status"],
        "numa_steal_thresholds": numa_thresholds,
        "status": "verified",
        "claims": [],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True, type=Path)
    parser.add_argument("--calibration", required=True, type=Path)
    parser.add_argument("--policy", required=True, type=Path)
    parser.add_argument("--topology", required=True, type=Path)
    parser.add_argument("--mode", required=True, choices=("latency", "bulk", "mixed"))
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--executable", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    try:
        source_commit, source_tree = source_authority(ROOT, arguments.expected_commit)
        executable = arguments.executable.resolve(strict=True)
        measured_executable_blake3 = executable_blake3(executable)
        inputs = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in (
                arguments.profile,
                arguments.calibration,
                arguments.policy,
                arguments.topology,
            )
        ]
        audit = validate_authority(
            *inputs,
            mode=arguments.mode,
            expected_commit=source_commit,
            expected_source_tree=source_tree,
            expected_executable_blake3=measured_executable_blake3,
        )
        arguments.output.write_text(
            json.dumps(audit, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        print(f"native scheduler authority check failed: {error}")
        return 1
    print(f"native scheduler authority check passed: {arguments.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
