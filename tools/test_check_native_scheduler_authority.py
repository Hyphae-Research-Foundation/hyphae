#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the composite Native scheduler authority checker."""

from __future__ import annotations

import copy
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_native_governor_policy import (
    CLASSES,
    class_compute_limit,
    class_io_limit,
    class_memory_limit,
)
from tools.check_native_execution_topology import ExecutionTopologyValidationError
from tools.check_native_hardware_calibration import CalibrationValidationError
from tools.check_native_scheduler_authority import (
    SchedulerAuthorityValidationError,
    main,
    source_authority,
    validate_authority,
)
from tools.test_check_native_hardware_calibration import valid_receipt


GIB = 1_024**3
FINGERPRINT = "1" * 64
CACHE_KEY = "2" * 64
SOURCE_COMMIT = "3" * 40
SOURCE_TREE = "4" * 40
EXECUTABLE_BLAKE3 = "5" * 64
SCHEMA_PATH = Path("contracts/json-schema/native-scheduler-authority-v1.schema.json")


def cpu_list(values: list[int]) -> str:
    ranges: list[tuple[int, int]] = []
    for value in sorted(values):
        if not ranges or value != ranges[-1][1] + 1:
            ranges.append((value, value))
        else:
            ranges[-1] = (ranges[-1][0], value)
    return ",".join(
        str(start) if start == end else f"{start}-{end}" for start, end in ranges
    )


def i7i_profile() -> dict:
    processors = []
    nodes: dict[int, list[int]] = {0: [], 1: []}
    for smt_rank in range(2):
        for node in range(2):
            for core in range(24):
                logical = smt_rank * 48 + node * 24 + core
                sibling = (1 - smt_rank) * 48 + node * 24 + core
                processors.append(
                    {
                        "logical_id": logical,
                        "core_id": core,
                        "socket_id": node,
                        "numa_node_id": node,
                        "thread_siblings": cpu_list([logical, sibling]),
                    }
                )
                nodes[node].append(logical)
    processors.sort(key=lambda processor: processor["logical_id"])
    total_memory = 768 * GIB
    return {
        "schema": "hyphae-native-hardware-profile-v1",
        "fingerprint": FINGERPRINT,
        "cpu": {
            "architecture": "x86_64",
            "logical_processors_available": 96,
            "physical_cores_visible": 48,
            "smt_threads_per_core": 2,
            "sockets_visible": 2,
            "numa_nodes_visible": 2,
            "affinity": "0-95",
            "quota_millicores": None,
            "instruction_sets": ["avx2", "avx512f", "fma"],
            "caches": [],
            "processor_topology": processors,
            "frequency_governors": ["performance"],
        },
        "memory": {
            "total_bytes": total_memory,
            "available_bytes": total_memory,
            "page_size_bytes": 4_096,
            "huge_page_size_bytes": 2 * 1_024 * 1_024,
            "huge_pages_total": 0,
            "numa_nodes": [
                {
                    "id": node,
                    "cpu_list": cpu_list(logical_ids),
                    "total_bytes": total_memory // 2,
                    "available_bytes": total_memory // 2,
                }
                for node, logical_ids in nodes.items()
            ],
        },
        "storage": {
            "path": "/mnt/hyphae-g7",
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


def measurement(template: dict, primitive: str, variant: str, size: int, unit: str, throughput: int) -> dict:
    cell = copy.deepcopy(template)
    cell.update(
        {
            "primitive": primitive,
            "variant": variant,
            "input_size": size,
            "input_unit": unit,
            "bytes_per_operation": 8 * 1_024 * 1_024,
        }
    )
    if primitive == "thread-scaling-memory-scan":
        cell["statistics"].update(
            {
                "minimum": 1_400_000,
                "median": 1_500_000,
                "maximum": 1_600_000,
                "median_absolute_deviation": 30_000,
                "relative_mad_ppm": 20_000,
                "relative_range_ppm": 133_333,
            }
        )
    cell["statistics"]["median_bytes_per_second"] = throughput
    return cell


def i7i_calibration() -> dict:
    receipt = valid_receipt()
    receipt["identity"]["hardware_fingerprint"] = FINGERPRINT
    receipt["identity"]["executable_blake3"] = EXECUTABLE_BLAKE3
    receipt["identity"]["cache_key"] = CACHE_KEY
    template = receipt["measurements"][0]
    scaling = []
    for threads, throughput in ((1, 1_000), (8, 8_000), (32, 32_000), (48, 48_000), (96, 49_000)):
        variant = (
            "persistent-workers-physical-range-linux-affinity"
            if threads <= 48
            else "persistent-workers-smt-range-linux-affinity"
        )
        scaling.append(
            measurement(
                template,
                "thread-scaling-memory-scan",
                variant,
                threads,
                "threads",
                throughput,
            )
        )
    io_cells = [
        measurement(
            template,
            "queue-depth-random-read",
            "persistent-sync-workers-buffered-4k",
            depth,
            "outstanding-reads",
            throughput,
        )
        for depth, throughput in ((1, 1_000), (4, 4_000), (8, 3_900))
    ]
    receipt["measurements"].extend(scaling + io_cells)
    receipt["selected_kernels"] = [
        {
            "primitive": cell["primitive"],
            "input_size": cell["input_size"],
            "input_unit": cell["input_unit"],
            "variant": cell["variant"],
            "reason": "candidate passed correctness and variance policy",
        }
        for cell in receipt["measurements"]
    ]
    receipt["thread_scaling"] = {
        "binding": "linux-sched-affinity",
        "physical_core_boundary": 48,
        "logical_processor_boundary": 96,
        "measured_thread_counts": [1, 8, 32, 48, 96],
        "status": "stable",
        "physical_peak_threads": 48,
        "physical_peak_bytes_per_second": 48_000,
        "smt_peak_threads": 96,
        "smt_peak_bytes_per_second": 49_000,
        "smt_to_physical_throughput_ppm": 1_020_833,
        "smt_recommended": False,
        "recommended_worker_count": 48,
        "recommendation": "physical range remains canonical in the fixture",
    }
    receipt["io_scaling"] = {
        "binding": "buffered-sync-workers",
        "measured_queue_depths": [1, 4, 8],
        "status": "stable",
        "peak_queue_depth": 4,
        "peak_bytes_per_second": 4_000,
        "recommended_io_slots": 4,
        "recommendation": "depth four is the first point within five percent of peak",
    }
    receipt["coverage"]["measured"] = sorted(
        {cell["primitive"] for cell in receipt["measurements"]}
    )
    receipt["coverage"]["unsupported"] = [{
        "primitive": "numa-local-remote-memory",
        "reason": "page residency unavailable",
    }]
    return receipt


def policy(memory_bytes: int | None = None) -> dict:
    total_memory = 768 * GIB
    governed_memory = memory_bytes if memory_bytes is not None else total_memory - total_memory * 15 // 100
    compute = 44
    io_slots = 4
    return {
        "schema": "hyphae-native-governor-policy-v1",
        "mode": "mixed",
        "hardware_fingerprint": FINGERPRINT,
        "calibration_cache_key": CACHE_KEY,
        "calibrated_worker_limit": 48,
        "reserved_system_threads": 4,
        "schedulable_compute_threads": compute,
        "io_slots": io_slots,
        "memory_bytes": governed_memory,
        "memory_headroom_percent": 15,
        "admission_queue_capacity": 2_816,
        "foreground_burst_limit": 16,
        "class_limits": [
            {
                "class": workload_class,
                "compute_threads": class_compute_limit("mixed", workload_class, compute),
                "io_slots": class_io_limit(workload_class, io_slots),
                "memory_bytes": class_memory_limit(workload_class, governed_memory),
            }
            for workload_class in CLASSES
        ],
    }


def topology() -> dict:
    pools = []
    worker_index = 0
    for node in range(2):
        workers = []
        for core in range(22):
            workers.append(
                {
                    "worker_index": worker_index,
                    "numa_node_id": node,
                    "logical_processor_id": node * 24 + core,
                    "socket_id": node,
                    "core_id": core,
                    "smt_rank": 0,
                }
            )
            worker_index += 1
        pools.append({"numa_node_id": node, "workers": workers})
    return {
        "schema": "hyphae-native-execution-topology-v1",
        "hardware_fingerprint": FINGERPRINT,
        "schedulable_compute_threads": 44,
        "hard_affinity": True,
        "pools": pools,
        "numa_steal_policy": {
            "schema": "hyphae-native-numa-steal-policy-v1",
            "calibration_cache_key": CACHE_KEY,
            "status": "disabled",
            "working_set_bytes": 8 * 1024 * 1024,
            "foreground_burst_limit": 16,
            "pools": [
                {
                    "worker_numa_node_id": node,
                    "steal_targets": [],
                }
                for node in range(2)
            ],
        },
    }


def validate_i7i(
    profile_value: dict,
    calibration_value: dict,
    policy_value: dict,
    topology_value: dict,
    *,
    source_commit: str = SOURCE_COMMIT,
    source_tree: str = SOURCE_TREE,
    executable_digest: str = EXECUTABLE_BLAKE3,
) -> dict:
    return validate_authority(
        profile_value,
        calibration_value,
        policy_value,
        topology_value,
        "mixed",
        source_commit,
        source_tree,
        executable_digest,
    )


class SchedulerAuthorityCheckerTests(unittest.TestCase):
    def test_source_authority_rejects_wrong_commit_and_dirty_tree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repository = Path(directory)
            subprocess.run(("git", "init", "-q"), cwd=repository, check=True)
            subprocess.run(
                ("git", "config", "user.email", "test@hyphae.invalid"),
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ("git", "config", "user.name", "Hyphae test"),
                cwd=repository,
                check=True,
            )
            tracked = repository / "authority.txt"
            tracked.write_text("canonical\n", encoding="utf-8")
            subprocess.run(("git", "add", "authority.txt"), cwd=repository, check=True)
            subprocess.run(
                ("git", "commit", "-q", "-m", "authority fixture"),
                cwd=repository,
                check=True,
            )
            commit = subprocess.run(
                ("git", "rev-parse", "HEAD"),
                cwd=repository,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            source_commit, source_tree = source_authority(repository, commit)
            self.assertEqual(source_commit, commit)
            self.assertRegex(source_tree, r"^[0-9a-f]{40}$")
            with self.assertRaisesRegex(SchedulerAuthorityValidationError, "differs"):
                source_authority(repository, "0" * 40)
            tracked.write_text("mutated\n", encoding="utf-8")
            with self.assertRaisesRegex(SchedulerAuthorityValidationError, "clean"):
                source_authority(repository, commit)

    def test_cli_emits_exact_source_executable_bound_authority(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            inputs = {
                "profile": i7i_profile(),
                "calibration": i7i_calibration(),
                "policy": policy(),
                "topology": topology(),
            }
            arguments = ["check_native_scheduler_authority.py"]
            for name, payload in inputs.items():
                path = root / f"{name}.json"
                path.write_text(json.dumps(payload), encoding="utf-8")
                arguments.extend((f"--{name}", str(path)))
            executable = root / "hyphae"
            executable.write_bytes(b"exact tested binary")
            output = root / "authority.json"
            arguments.extend(
                (
                    "--mode",
                    "mixed",
                    "--expected-commit",
                    SOURCE_COMMIT,
                    "--executable",
                    str(executable),
                    "--output",
                    str(output),
                )
            )
            with patch("sys.argv", arguments), patch(
                "tools.check_native_scheduler_authority.source_authority",
                return_value=(SOURCE_COMMIT, SOURCE_TREE),
                create=True,
            ), patch(
                "tools.check_native_scheduler_authority.executable_blake3",
                return_value=EXECUTABLE_BLAKE3,
                create=True,
            ):
                self.assertEqual(main(), 0)
            audit = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(audit["source_commit"], SOURCE_COMMIT)
            self.assertEqual(audit["source_tree"], SOURCE_TREE)
            self.assertEqual(audit["executable_blake3"], EXECUTABLE_BLAKE3)

    def test_accepts_i7i_shaped_canonical_authority(self) -> None:
        audit = validate_i7i(i7i_profile(), i7i_calibration(), policy(), topology())
        self.assertEqual(audit["status"], "verified")
        self.assertEqual(audit["calibrated_worker_limit"], 48)
        self.assertEqual(audit["schedulable_compute_threads"], 44)
        self.assertEqual(audit["numa_pools"], [0, 1])
        self.assertEqual(audit["numa_steal_status"], "disabled")
        self.assertEqual(audit["numa_steal_thresholds"], [])
        self.assertEqual(set(audit["input_canonical_sha256"]), {
            "profile", "calibration", "policy", "topology"
        })
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        self.assertEqual(set(audit), set(schema["required"]))

    def test_accepts_explicit_numa_unsupported_as_disabled(self) -> None:
        calibration = i7i_calibration()
        disabled = topology()
        audit = validate_i7i(i7i_profile(), calibration, policy(), disabled)
        self.assertEqual(audit["numa_steal_status"], "disabled")
        self.assertEqual(audit["numa_steal_thresholds"], [])

    def test_rejects_source_tree_or_executable_substitution(self) -> None:
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "source tree"):
            validate_i7i(
                i7i_profile(),
                i7i_calibration(),
                policy(),
                topology(),
                source_tree="not-a-tree",
            )
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "another executable"):
            validate_i7i(
                i7i_profile(),
                i7i_calibration(),
                policy(),
                topology(),
                executable_digest="6" * 64,
            )

    def test_rejects_swapped_hardware_and_calibration_authority(self) -> None:
        calibration = i7i_calibration()
        calibration["identity"]["hardware_fingerprint"] = "9" * 64
        with self.assertRaisesRegex(ValueError, "different hardware"):
            validate_i7i(i7i_profile(), calibration, policy(), topology())

    def test_rejects_unaccepted_calibration(self) -> None:
        calibration = i7i_calibration()
        calibration["accepted_for_scheduling"] = False
        with self.assertRaises(ValueError):
            validate_i7i(i7i_profile(), calibration, policy(), topology())

    def test_rejects_policy_from_another_calibration(self) -> None:
        changed = policy()
        changed["calibration_cache_key"] = "8" * 64
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "cache key"):
            validate_i7i(i7i_profile(), i7i_calibration(), changed, topology())

    def test_rejects_memory_not_derived_from_profile_headroom(self) -> None:
        changed = policy(memory_bytes=700 * GIB)
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "memory limit"):
            validate_i7i(i7i_profile(), i7i_calibration(), changed, topology())

    def test_rejects_portable_topology_when_physical_placement_exists(self) -> None:
        portable = topology()
        portable["hard_affinity"] = False
        portable["pools"] = [
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
                    for index in range(44)
                ],
            }
        ]
        portable["numa_steal_policy"].update({
            "status": "not-applicable",
            "pools": [{"worker_numa_node_id": None, "steal_targets": []}],
        })
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "canonical"):
            validate_i7i(i7i_profile(), i7i_calibration(), policy(), portable)

    def test_rejects_premature_smt_placement(self) -> None:
        changed = topology()
        worker = changed["pools"][0]["workers"][0]
        worker["logical_processor_id"] = 48
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "canonical"):
            validate_i7i(i7i_profile(), i7i_calibration(), policy(), changed)

    def test_rejects_fabricated_numa_threshold_while_residency_is_unsupported(self) -> None:
        changed = topology()
        changed["numa_steal_policy"]["status"] = "calibrated"
        changed["numa_steal_policy"]["pools"][0]["steal_targets"] = [{
            "home_numa_node_id": 1,
            "remote_to_local_latency_ppm": 4_000_000,
            "steal_after_nanoseconds": 3_000,
        }]
        with self.assertRaisesRegex(
            ExecutionTopologyValidationError,
            "directed remote target",
        ):
            validate_i7i(i7i_profile(), i7i_calibration(), policy(), changed)

    def test_rejects_first_touch_matrix_without_residency_evidence(self) -> None:
        calibration = i7i_calibration()
        template = calibration["measurements"][0]
        for source, reader, cpu in ((0, 0, 0), (0, 1, 24), (1, 0, 0), (1, 1, 24)):
            cell = copy.deepcopy(template)
            cell.update({
                "primitive": "numa-memory-read",
                "variant": f"linux-first-touch-node-{source}-read-node-{reader}-cpu-{cpu}",
                "input_size": 8 * 1024 * 1024,
                "input_unit": "working-set-bytes",
                "bytes_per_operation": 8 * 1024 * 1024,
            })
            calibration["measurements"].append(cell)
            calibration["selected_kernels"].append({
                "primitive": cell["primitive"],
                "input_size": cell["input_size"],
                "input_unit": cell["input_unit"],
                "variant": cell["variant"],
                "reason": "candidate passed correctness and variance policy",
            })
        calibration["coverage"]["measured"].append("numa-memory-read")
        calibration["coverage"]["measured"].sort()
        calibration["coverage"]["unsupported"] = []
        with self.assertRaisesRegex(CalibrationValidationError, "page-residency"):
            validate_i7i(i7i_profile(), calibration, policy(), topology())

    def test_rejects_invalid_source_commit(self) -> None:
        with self.assertRaisesRegex(SchedulerAuthorityValidationError, "source commit"):
            validate_i7i(
                i7i_profile(),
                i7i_calibration(),
                policy(),
                topology(),
                source_commit="not-a-sha",
            )


if __name__ == "__main__":
    unittest.main()
