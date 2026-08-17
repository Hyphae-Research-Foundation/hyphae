#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the Native governor policy semantic checker."""

from __future__ import annotations

import copy
import unittest

from tools.check_native_governor_policy import GovernorPolicyValidationError, validate_policy


def class_limit(name: str, compute: int) -> dict:
    io_slots = {
        "foreground-point": 1,
        "mutation": 2,
        "foreground-bounded": 2,
    }.get(name, 4)
    memory_bytes = {
        "mutation": 125,
        "foreground-bounded": 500,
    }.get(name, 1_000)
    return {
        "class": name,
        "compute_threads": compute,
        "io_slots": io_slots,
        "memory_bytes": memory_bytes,
    }


def valid_policy() -> dict:
    classes = [
        ("foreground-point", 1),
        ("foreground-bounded", 4),
        ("mutation", 2),
        ("bulk", 4),
        ("maintenance", 2),
        ("recovery", 2),
        ("administrative", 2),
    ]
    return {
        "schema": "hyphae-native-governor-policy-v1",
        "mode": "mixed",
        "hardware_fingerprint": "1" * 64,
        "calibration_cache_key": "2" * 64,
        "calibrated_worker_limit": 8,
        "reserved_system_threads": 1,
        "schedulable_compute_threads": 7,
        "io_slots": 4,
        "memory_bytes": 1_000,
        "memory_headroom_percent": 15,
        "admission_queue_capacity": 448,
        "foreground_burst_limit": 16,
        "class_limits": [class_limit(name, compute) for name, compute in classes],
    }


class GovernorPolicyCheckerTests(unittest.TestCase):
    def test_accepts_canonical_policy(self) -> None:
        validate_policy(valid_policy())

    def test_rejects_inconsistent_system_reserve(self) -> None:
        policy = valid_policy()
        policy["reserved_system_threads"] = 0
        with self.assertRaisesRegex(GovernorPolicyValidationError, "inconsistent"):
            validate_policy(policy)

    def test_rejects_noncanonical_io_cap(self) -> None:
        policy = valid_policy()
        policy["class_limits"][3]["io_slots"] = 5
        with self.assertRaisesRegex(GovernorPolicyValidationError, "canonical v1 limit"):
            validate_policy(policy)

    def test_rejects_reordered_or_missing_class(self) -> None:
        policy = copy.deepcopy(valid_policy())
        policy["class_limits"].reverse()
        with self.assertRaisesRegex(GovernorPolicyValidationError, "canonical workload order"):
            validate_policy(policy)

    def test_rejects_noncanonical_compute_cap(self) -> None:
        policy = valid_policy()
        policy["class_limits"][1]["compute_threads"] = 7
        with self.assertRaisesRegex(GovernorPolicyValidationError, "canonical v1 limit"):
            validate_policy(policy)

    def test_rejects_noncanonical_queue_capacity_and_burst(self) -> None:
        policy = valid_policy()
        policy["admission_queue_capacity"] = 447
        with self.assertRaisesRegex(GovernorPolicyValidationError, "queue_capacity"):
            validate_policy(policy)
        policy = valid_policy()
        policy["foreground_burst_limit"] = 17
        with self.assertRaisesRegex(GovernorPolicyValidationError, "burst_limit"):
            validate_policy(policy)


if __name__ == "__main__":
    unittest.main()
