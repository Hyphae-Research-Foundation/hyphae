#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed semantic checker for Native governor policy v1."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-governor-policy-v1"
DIGEST = re.compile(r"^[0-9a-f]{64}$")
CLASSES = [
    "foreground-point",
    "foreground-bounded",
    "mutation",
    "bulk",
    "maintenance",
    "recovery",
    "administrative",
]


class GovernorPolicyValidationError(ValueError):
    """A governor policy violates its versioned semantic contract."""


def fail(message: str) -> None:
    raise GovernorPolicyValidationError(message)


def require_object(value: Any, field: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be an object")
    actual = set(value)
    if actual != keys:
        fail(f"{field} keys differ: missing={sorted(keys - actual)} extra={sorted(actual - keys)}")
    return value


def require_integer(value: Any, field: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    return value


def class_compute_limit(mode: str, workload_class: str, total: int) -> int:
    quarter = max(1, (total + 3) // 4)
    half = max(1, (total + 1) // 2)
    if workload_class == "foreground-point":
        return 1
    if workload_class == "mutation":
        return min(total, 2)
    if (mode == "latency" and workload_class == "foreground-bounded") or (
        mode == "bulk" and workload_class in {"bulk", "recovery"}
    ):
        return total
    if mode == "bulk" and workload_class == "foreground-bounded":
        return quarter
    if mode == "latency" or (
        mode == "mixed" and workload_class in {"maintenance", "recovery", "administrative"}
    ):
        return quarter
    return half


def class_io_limit(workload_class: str, total: int) -> int:
    if workload_class == "foreground-point":
        return 1
    if workload_class == "mutation":
        return min(total, 2)
    if workload_class == "foreground-bounded":
        return max(1, (total + 1) // 2)
    return total


def class_memory_limit(workload_class: str, total: int) -> int:
    if workload_class == "foreground-point":
        return min(total, 64 * 1_024 * 1_024)
    if workload_class == "mutation":
        return max(1, (total + 7) // 8)
    if workload_class == "foreground-bounded":
        return max(1, (total + 1) // 2)
    return total


def validate_policy(value: Any) -> None:
    root = require_object(
        value,
        "policy",
        {
            "schema",
            "mode",
            "hardware_fingerprint",
            "calibration_cache_key",
            "calibrated_worker_limit",
            "reserved_system_threads",
            "schedulable_compute_threads",
            "io_slots",
            "memory_bytes",
            "memory_headroom_percent",
            "admission_queue_capacity",
            "foreground_burst_limit",
            "class_limits",
        },
    )
    if root["schema"] != SCHEMA:
        fail("policy schema is not Native governor policy v1")
    mode = root["mode"]
    if mode not in {"latency", "bulk", "mixed"}:
        fail("policy mode is invalid")
    for field in ("hardware_fingerprint", "calibration_cache_key"):
        if not isinstance(root[field], str) or DIGEST.fullmatch(root[field]) is None:
            fail(f"{field} must be a lowercase BLAKE3 digest")
    worker_limit = require_integer(root["calibrated_worker_limit"], "calibrated_worker_limit", 1)
    reserve = require_integer(root["reserved_system_threads"], "reserved_system_threads")
    schedulable = require_integer(
        root["schedulable_compute_threads"], "schedulable_compute_threads", 1
    )
    expected_reserve = 0 if worker_limit <= 1 else min(worker_limit - 1, max(1, worker_limit // 12))
    if reserve != expected_reserve or schedulable != max(1, worker_limit - reserve):
        fail("worker limit, system reserve, and schedulable threads are inconsistent")
    io_slots = require_integer(root["io_slots"], "io_slots", 1)
    if io_slots > 64:
        fail("io_slots exceeds the v1 safety ceiling")
    memory_bytes = require_integer(root["memory_bytes"], "memory_bytes", 1)
    if root["memory_headroom_percent"] != 15:
        fail("memory_headroom_percent must be 15")
    expected_queue_capacity = min(4_096, max(64, schedulable * 64))
    if root["admission_queue_capacity"] != expected_queue_capacity:
        fail("admission_queue_capacity is not the canonical v1 limit")
    if root["foreground_burst_limit"] != 16:
        fail("foreground_burst_limit must be 16")

    rows = root["class_limits"]
    if not isinstance(rows, list) or len(rows) != len(CLASSES):
        fail("class_limits must contain exactly seven rows")
    actual_classes: list[str] = []
    for index, value_row in enumerate(rows):
        row = require_object(
            value_row,
            f"class_limits[{index}]",
            {"class", "compute_threads", "io_slots", "memory_bytes"},
        )
        workload_class = row["class"]
        actual_classes.append(workload_class)
        compute = require_integer(row["compute_threads"], f"class_limits[{index}].compute_threads", 1)
        row_io = require_integer(row["io_slots"], f"class_limits[{index}].io_slots", 1)
        row_memory = require_integer(row["memory_bytes"], f"class_limits[{index}].memory_bytes", 1)
        if compute != class_compute_limit(mode, workload_class, schedulable):
            fail(f"class_limits[{index}].compute_threads is not the canonical v1 limit")
        if row_io != class_io_limit(workload_class, io_slots):
            fail(f"class_limits[{index}].io_slots is not the canonical v1 limit")
        if row_memory != class_memory_limit(workload_class, memory_bytes):
            fail(f"class_limits[{index}].memory_bytes is not the canonical v1 limit")
        if compute > schedulable or row_io > io_slots or row_memory > memory_bytes:
            fail(f"class_limits[{index}] exceeds a global resource ceiling")
    if actual_classes != CLASSES:
        fail("class_limits are missing or not in canonical workload order")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", required=True, type=Path)
    args = parser.parse_args()
    try:
        with args.policy.open(encoding="utf-8") as handle:
            validate_policy(json.load(handle))
    except (OSError, json.JSONDecodeError, GovernorPolicyValidationError) as error:
        print(f"native governor policy check failed: {error}")
        return 1
    print(f"native governor policy check passed: {args.policy}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
