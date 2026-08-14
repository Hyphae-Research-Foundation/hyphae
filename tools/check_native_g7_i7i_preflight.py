#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed qualification for the dedicated AWS i7i.metal-24xl G7 host."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_hardware_profile import (
    HardwareProfileValidationError,
    parse_cpu_list,
    validate_profile,
)


EXPECTED_DATA_ROOT = "/mnt/hyphae-g7"
EXPECTED_LOGICAL_PROCESSORS = 96
EXPECTED_PHYSICAL_CORES = 48
EXPECTED_SMT_THREADS = 2
NOMINAL_MEMORY_BYTES = 768 * 1024**3
MEMORY_TOLERANCE_PERCENT = 5
NVME_BLOCK_DEVICE = re.compile(r"259:[0-9]+\Z")
LOCAL_FILESYSTEMS = {"ext4", "xfs"}


class I7iPreflightError(ValueError):
    """The discovered host is not the approved dedicated G7 machine shape."""


def fail(message: str) -> None:
    raise I7iPreflightError(message)


def require_equal(actual: Any, expected: Any, field: str) -> None:
    if actual != expected:
        fail(f"{field} must be {expected!r}, got {actual!r}")


def validate_i7i_profile(profile: Any) -> dict[str, Any]:
    try:
        validate_profile(profile)
    except HardwareProfileValidationError as error:
        fail(str(error))
    if not isinstance(profile, dict):
        fail("profile must be an object")

    cpu = profile["cpu"]
    operating_system = profile["operating_system"]
    memory = profile["memory"]
    storage = profile["storage"]

    require_equal(operating_system["family"], "linux", "operating_system.family")
    require_equal(
        operating_system["virtualization"],
        "none",
        "operating_system.virtualization",
    )
    require_equal(cpu["architecture"], "x86_64", "cpu.architecture")
    require_equal(
        cpu["logical_processors_available"],
        EXPECTED_LOGICAL_PROCESSORS,
        "cpu.logical_processors_available",
    )
    require_equal(
        cpu["physical_cores_visible"],
        EXPECTED_PHYSICAL_CORES,
        "cpu.physical_cores_visible",
    )
    require_equal(
        cpu["smt_threads_per_core"],
        EXPECTED_SMT_THREADS,
        "cpu.smt_threads_per_core",
    )
    require_equal(cpu["quota_millicores"], None, "cpu.quota_millicores")
    require_equal(cpu["frequency_governors"], ["performance"], "cpu.frequency_governors")

    affinity = parse_cpu_list(cpu["affinity"], "cpu.affinity")
    topology = cpu["processor_topology"]
    topology_ids = {processor["logical_id"] for processor in topology}
    if len(affinity) != EXPECTED_LOGICAL_PROCESSORS or topology_ids != affinity:
        fail("cpu affinity must expose the complete 96-processor topology")
    if any(
        len(parse_cpu_list(processor["thread_siblings"], "processor.thread_siblings"))
        != EXPECTED_SMT_THREADS
        for processor in topology
    ):
        fail("every physical core must expose exactly two SMT siblings")

    total_memory = memory["total_bytes"]
    if not isinstance(total_memory, int) or isinstance(total_memory, bool):
        fail("memory.total_bytes must be measured")
    tolerance = NOMINAL_MEMORY_BYTES * MEMORY_TOLERANCE_PERCENT // 100
    if not NOMINAL_MEMORY_BYTES - tolerance <= total_memory <= NOMINAL_MEMORY_BYTES + tolerance:
        fail("memory.total_bytes is outside the 768 GiB +/- 5% i7i envelope")

    require_equal(storage["path"], EXPECTED_DATA_ROOT, "storage.path")
    if storage["filesystem"] not in LOCAL_FILESYSTEMS:
        fail("storage.filesystem must be ext4 or xfs")
    if storage["rotational"] is not False:
        fail("storage must be explicitly non-rotational")
    device = storage["device"]
    if not isinstance(device, str) or NVME_BLOCK_DEVICE.fullmatch(device) is None:
        fail("storage.device must resolve to a Linux NVMe block device")
    queue_depth = storage["queue_depth"]
    if not isinstance(queue_depth, int) or isinstance(queue_depth, bool) or queue_depth <= 0:
        fail("storage.queue_depth must be measured and positive")

    return {
        "schema": "hyphae-native-g7-i7i-preflight-audit-v1",
        "status": "passed",
        "instance_type": "i7i.metal-24xl",
        "hardware_fingerprint": profile["fingerprint"],
        "logical_processors": EXPECTED_LOGICAL_PROCESSORS,
        "physical_cores": EXPECTED_PHYSICAL_CORES,
        "smt_threads_per_core": EXPECTED_SMT_THREADS,
        "memory_bytes": total_memory,
        "storage_path": storage["path"],
        "storage_device": device,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    try:
        profile = json.loads(arguments.profile.read_text(encoding="utf-8"))
        audit = validate_i7i_profile(profile)
        arguments.output.write_text(
            json.dumps(audit, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    except (OSError, UnicodeError, json.JSONDecodeError, I7iPreflightError) as error:
        print(f"native G7 i7i preflight failed: {error}")
        return 1
    print(f"native G7 i7i preflight passed: {arguments.profile}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
