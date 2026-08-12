#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed checker for thread-scaling diagnostic receipts."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-hardware-calibration-diagnostic-v1"
SURFACE = "thread-scaling-memory-scan"
SHA1 = re.compile(r"^[0-9a-f]{40}$")
DIGEST = re.compile(r"^[0-9a-f]{64}$")
PPM = 1_000_000
PICOSECONDS_PER_SECOND = 1_000_000_000_000
POLICY = {
    "mode": "thorough",
    "warmup_batches": 4,
    "samples_per_measurement": 31,
    "target_sample_duration_ms": 225,
    "maximum_relative_mad_ppm": 40_000,
    "operation_calibration_target_lower_ppm": 900_000,
    "operation_calibration_target_upper_ppm": 1_100_000,
    "operation_calibration_confirmations": 2,
    "operation_calibration_max_refinements": 6,
}
BATCH_MINIMUM_TARGET_PPM = 800_000
BATCH_MAXIMUM_TARGET_PPM = 1_250_000
THREAD_SCALING_OPERATION_CAP = 1_048_576


class DiagnosticValidationError(ValueError):
    """A diagnostic receipt violates its non-authority contract."""


def fail(message: str) -> None:
    raise DiagnosticValidationError(message)


def require_object(value: Any, field: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be an object")
    actual = set(value)
    if actual != keys:
        fail(
            f"{field} keys differ: missing={sorted(keys - actual)} "
            f"extra={sorted(actual - keys)}"
        )
    return value


def require_integer(value: Any, field: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    return value


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a non-empty string")
    return value


def require_digest(value: Any, field: str) -> str:
    digest = require_string(value, field)
    if DIGEST.fullmatch(digest) is None:
        fail(f"{field} must be a lowercase 64-character digest")
    return digest


def recompute_statistics(
    samples: list[int], bytes_per_operation: int
) -> dict[str, int | str]:
    ordered = sorted(samples)
    median = ordered[len(ordered) // 2]
    deviations = sorted(abs(sample - median) for sample in ordered)
    median_absolute_deviation = deviations[len(deviations) // 2]
    return {
        "unit": "picoseconds_per_operation",
        "minimum": ordered[0],
        "median": median,
        "maximum": ordered[-1],
        "median_absolute_deviation": median_absolute_deviation,
        "relative_mad_ppm": median_absolute_deviation * PPM // median,
        "relative_range_ppm": (ordered[-1] - ordered[0]) * PPM // median,
        "median_bytes_per_second": max(
            1,
            bytes_per_operation * PICOSECONDS_PER_SECOND // median,
        ),
    }


def validate_point(value: Any, index: int) -> dict[str, Any]:
    prefix = f"surface.worker_points[{index}]"
    point = require_object(
        value,
        prefix,
        {
            "worker_count",
            "variant",
            "bytes_per_operation",
            "operations_per_sample",
            "maximum_operations_per_sample",
            "batch_calibration_status",
            "samples_picoseconds_per_operation",
            "statistics",
            "correctness",
            "status",
        },
    )
    worker_count = require_integer(point["worker_count"], f"{prefix}.worker_count", 1)
    variant = require_string(point["variant"], f"{prefix}.variant")
    bytes_per_operation = require_integer(
        point["bytes_per_operation"], f"{prefix}.bytes_per_operation", 1
    )
    operations = require_integer(
        point["operations_per_sample"], f"{prefix}.operations_per_sample", 1
    )
    operation_cap = require_integer(
        point["maximum_operations_per_sample"],
        f"{prefix}.maximum_operations_per_sample",
        1,
    )
    if operation_cap != THREAD_SCALING_OPERATION_CAP:
        fail(f"{prefix}.maximum_operations_per_sample differs from the frozen cap")
    if operations > operation_cap:
        fail(f"{prefix}.operations_per_sample exceeds its operation cap")
    calibration_status = point["batch_calibration_status"]
    if calibration_status not in {"converged", "not-converged"}:
        fail(f"{prefix}.batch_calibration_status is invalid")

    samples = point["samples_picoseconds_per_operation"]
    if not isinstance(samples, list) or len(samples) != POLICY["samples_per_measurement"]:
        fail(f"{prefix} must contain exactly 31 chronological samples")
    validated_samples = [
        require_integer(sample, f"{prefix}.samples_picoseconds_per_operation[{sample_index}]", 1)
        for sample_index, sample in enumerate(samples)
    ]
    expected_statistics = recompute_statistics(validated_samples, bytes_per_operation)
    statistics = require_object(
        point["statistics"], f"{prefix}.statistics", set(expected_statistics)
    )
    if statistics != expected_statistics:
        fail(f"{prefix}.statistics do not recompute from chronological samples")

    correctness = require_object(
        point["correctness"],
        f"{prefix}.correctness",
        {"status", "result_digest_blake3", "reference_digest_blake3"},
    )
    result_digest = require_digest(
        correctness["result_digest_blake3"],
        f"{prefix}.correctness.result_digest_blake3",
    )
    reference_digest = require_digest(
        correctness["reference_digest_blake3"],
        f"{prefix}.correctness.reference_digest_blake3",
    )
    if correctness["status"] != "passed" or result_digest != reference_digest:
        fail(f"{prefix}.correctness did not pass differential validation")

    target_batch_picoseconds = POLICY["target_sample_duration_ms"] * 1_000_000_000
    median_batch_picoseconds = expected_statistics["median"] * operations
    batch_inside_diagnostic_window = (
        median_batch_picoseconds * PPM
        >= target_batch_picoseconds * BATCH_MINIMUM_TARGET_PPM
        and median_batch_picoseconds * PPM
        <= target_batch_picoseconds * BATCH_MAXIMUM_TARGET_PPM
        and operations < operation_cap
    )
    if calibration_status == "converged" and not batch_inside_diagnostic_window:
        fail(f"{prefix} claims convergence outside the diagnostic target window")
    stable = (
        calibration_status == "converged"
        and correctness["status"] == "passed"
        and expected_statistics["relative_mad_ppm"]
        <= POLICY["maximum_relative_mad_ppm"]
    )
    expected_status = "stable" if stable else "unstable"
    if point["status"] != expected_status:
        fail(f"{prefix}.status must be {expected_status}")
    return {"worker_count": worker_count, "variant": variant}


def validate_receipt(
    value: Any,
    *,
    expected_source_commit: str,
    expected_source_tree: str,
    expected_platform: str,
    expected_hardware_fingerprint: str,
    expected_producer_executable_blake3: str,
    expected_compiler_identity: str,
    expected_hyphae_build_identity: str,
    expected_worker_counts: list[int],
) -> dict[str, Any]:
    receipt = require_object(
        value,
        "receipt",
        {
            "schema",
            "authority",
            "evidence_class",
            "claims",
            "closure_declared",
            "source",
            "platform",
            "identity",
            "policy",
            "surface",
        },
    )
    if (
        receipt["schema"] != SCHEMA
        or receipt["authority"] is not False
        or receipt["evidence_class"] != "diagnostic-only"
        or receipt["claims"] != []
        or receipt["closure_declared"] is not False
    ):
        fail("receipt attempts authority, claims, closure, or another schema")

    source = require_object(receipt["source"], "source", {"commit", "tree"})
    for field, expected in (
        ("commit", expected_source_commit),
        ("tree", expected_source_tree),
    ):
        value = require_string(source[field], f"source.{field}")
        if SHA1.fullmatch(value) is None or value != expected:
            fail(f"source.{field} differs from the expected exact source")
    if require_string(receipt["platform"], "platform") != expected_platform:
        fail("diagnostic targets another platform")

    identity = require_object(
        receipt["identity"],
        "identity",
        {
            "hardware_fingerprint",
            "producer_executable_blake3",
            "compiler_identity",
            "hyphae_build_identity",
        },
    )
    hardware = require_digest(identity["hardware_fingerprint"], "identity.hardware_fingerprint")
    executable = require_digest(
        identity["producer_executable_blake3"],
        "identity.producer_executable_blake3",
    )
    if hardware != expected_hardware_fingerprint:
        fail("diagnostic targets another hardware fingerprint")
    if executable != expected_producer_executable_blake3:
        fail("diagnostic targets another producer executable")
    if (
        require_string(identity["compiler_identity"], "identity.compiler_identity")
        != expected_compiler_identity
    ):
        fail("diagnostic targets another compiler identity")
    if (
        require_string(identity["hyphae_build_identity"], "identity.hyphae_build_identity")
        != expected_hyphae_build_identity
    ):
        fail("diagnostic targets another Hyphae build identity")

    policy = require_object(receipt["policy"], "policy", set(POLICY))
    if policy != POLICY:
        fail("diagnostic policy differs from the frozen thorough policy")

    surface = require_object(
        receipt["surface"],
        "surface",
        {"primitive", "binding", "worker_points"},
    )
    if surface["primitive"] != SURFACE:
        fail("diagnostic contains another measurement surface")
    binding = surface["binding"]
    if binding not in {"linux-sched-affinity", "unbound"}:
        fail("diagnostic thread binding is invalid")
    points = surface["worker_points"]
    if not isinstance(points, list) or not points:
        fail("diagnostic worker points must be a non-empty list")
    identities = [validate_point(point, index) for index, point in enumerate(points)]
    worker_counts = [identity["worker_count"] for identity in identities]
    if worker_counts != sorted(set(worker_counts)) or worker_counts != expected_worker_counts:
        fail("diagnostic worker points differ from the exact ordered request")
    expected_suffix = "linux-affinity" if binding == "linux-sched-affinity" else "unbound"
    if any(not identity["variant"].endswith(expected_suffix) for identity in identities):
        fail("diagnostic variants disagree with the declared binding")
    return receipt


def parse_worker_counts(value: str) -> list[int]:
    try:
        counts = [int(item) for item in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "worker counts must be comma-separated integers"
        ) from error
    if not counts or any(count <= 0 for count in counts) or counts != sorted(set(counts)):
        raise argparse.ArgumentTypeError("worker counts must be positive, unique, and ordered")
    return counts


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--hardware-fingerprint", required=True)
    parser.add_argument("--producer-executable-blake3", required=True)
    parser.add_argument("--compiler-identity", required=True)
    parser.add_argument("--hyphae-build-identity", required=True)
    parser.add_argument("--worker-counts", type=parse_worker_counts, required=True)
    arguments = parser.parse_args()
    payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
    validate_receipt(
        payload,
        expected_source_commit=arguments.source_commit,
        expected_source_tree=arguments.source_tree,
        expected_platform=arguments.platform,
        expected_hardware_fingerprint=arguments.hardware_fingerprint,
        expected_producer_executable_blake3=arguments.producer_executable_blake3,
        expected_compiler_identity=arguments.compiler_identity,
        expected_hyphae_build_identity=arguments.hyphae_build_identity,
        expected_worker_counts=arguments.worker_counts,
    )
    print(f"Native hardware calibration diagnostic passed: {arguments.receipt}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
