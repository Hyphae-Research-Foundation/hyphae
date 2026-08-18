#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed semantic checker for Native hardware calibration v1."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-hardware-calibration-v1"
DIGEST = re.compile(r"^[0-9a-f]{64}$")
SMT_RECOMMENDATION_RATIO_PPM = 1_050_000
IO_RECOMMENDATION_FLOOR_PPM = 950_000
THREAD_SCALING_BATCH_MINIMUM_TARGET_PPM = 800_000
THREAD_SCALING_BATCH_MAXIMUM_TARGET_PPM = 1_250_000
REQUIRED_SCHEDULER_MEASUREMENT_PRIMITIVES = {
    "numa-memory-read",
    "thread-scaling-memory-scan",
}
ROBUST_MEDIAN_MEASUREMENT_PRIMITIVES = REQUIRED_SCHEDULER_MEASUREMENT_PRIMITIVES | {
    "queue-depth-random-read"
}
NUMA_VARIANT = re.compile(
    r"^linux-first-touch-node-(?P<source>[0-9]+)-read-node-(?P<reader>[0-9]+)-cpu-(?P<cpu>[0-9]+)$"
)


class CalibrationValidationError(ValueError):
    """A calibration receipt violates its frozen semantic contract."""


def fail(message: str) -> None:
    raise CalibrationValidationError(message)


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


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a non-empty string")
    return value


def require_digest(value: Any, field: str) -> str:
    digest = require_string(value, field)
    if DIGEST.fullmatch(digest) is None:
        fail(f"{field} must be a lowercase BLAKE3 digest")
    return digest


def validate_identity(value: Any) -> None:
    identity = require_object(
        value,
        "identity",
        {
            "hardware_fingerprint",
            "kernel_release",
            "filesystem",
            "compiler_identity",
            "hyphae_build_identity",
            "executable_blake3",
            "cache_key",
        },
    )
    require_digest(identity["hardware_fingerprint"], "identity.hardware_fingerprint")
    require_string(identity["kernel_release"], "identity.kernel_release")
    if identity["filesystem"] is not None:
        require_string(identity["filesystem"], "identity.filesystem")
    require_string(identity["compiler_identity"], "identity.compiler_identity")
    require_string(identity["hyphae_build_identity"], "identity.hyphae_build_identity")
    require_digest(identity["executable_blake3"], "identity.executable_blake3")
    require_digest(identity["cache_key"], "identity.cache_key")


def validate_policy(value: Any) -> dict[str, int]:
    policy = require_object(
        value,
        "policy",
        {
            "minimum_duration_ms",
            "maximum_duration_ms",
            "warmup_batches",
            "measurement_retry_limit",
            "samples_per_measurement",
            "target_sample_duration_ms",
            "maximum_relative_mad_ppm",
            "maximum_relative_range_ppm",
        },
    )
    for field in policy:
        minimum = 3 if field == "samples_per_measurement" else 1
        if field.startswith("maximum_relative_"):
            minimum = 0
        require_integer(policy[field], f"policy.{field}", minimum)
    if policy["maximum_duration_ms"] < policy["minimum_duration_ms"]:
        fail("policy maximum duration is below minimum duration")
    if policy["maximum_relative_mad_ppm"] > 1_000_000:
        fail("policy maximum relative MAD exceeds 1,000,000 ppm")
    if not 1 <= policy["measurement_retry_limit"] <= 4:
        fail("policy.measurement_retry_limit must be between 1 and 4")
    if policy["maximum_relative_range_ppm"] > 1_000_000:
        fail("policy maximum relative range exceeds 1,000,000 ppm")
    return policy


def validate_statistics(value: Any, prefix: str) -> tuple[int, int]:
    statistics = require_object(
        value,
        prefix,
        {
            "unit",
            "minimum",
            "median",
            "maximum",
            "median_absolute_deviation",
            "relative_mad_ppm",
            "relative_range_ppm",
            "median_bytes_per_second",
        },
    )
    if statistics["unit"] != "picoseconds_per_operation":
        fail(f"{prefix}.unit is not canonical")
    minimum = require_integer(statistics["minimum"], f"{prefix}.minimum", 1)
    median = require_integer(statistics["median"], f"{prefix}.median", 1)
    maximum = require_integer(statistics["maximum"], f"{prefix}.maximum", 1)
    if not minimum <= median <= maximum:
        fail(f"{prefix} timing order is invalid")
    require_integer(
        statistics["median_absolute_deviation"],
        f"{prefix}.median_absolute_deviation",
    )
    mad_ppm = require_integer(statistics["relative_mad_ppm"], f"{prefix}.relative_mad_ppm")
    range_ppm = require_integer(statistics["relative_range_ppm"], f"{prefix}.relative_range_ppm")
    if statistics["median_bytes_per_second"] is not None:
        require_integer(
            statistics["median_bytes_per_second"],
            f"{prefix}.median_bytes_per_second",
            1,
        )
    return mad_ppm, range_ppm


def validate_measurement(value: Any, policy: dict[str, int], index: int) -> tuple[tuple[Any, ...], bool]:
    prefix = f"measurements[{index}]"
    measurement = require_object(
        value,
        prefix,
        {
            "primitive",
            "variant",
            "input_size",
            "input_unit",
            "bytes_per_operation",
            "operations_per_sample",
            "maximum_operations_per_sample",
            "sample_count",
            "statistics",
            "correctness",
            "status",
            "retry_history",
        },
    )
    primitive = require_string(measurement["primitive"], f"{prefix}.primitive")
    variant = require_string(measurement["variant"], f"{prefix}.variant")
    input_size = require_integer(measurement["input_size"], f"{prefix}.input_size", 1)
    input_unit = require_string(measurement["input_unit"], f"{prefix}.input_unit")
    require_integer(measurement["bytes_per_operation"], f"{prefix}.bytes_per_operation")
    operations = require_integer(
        measurement["operations_per_sample"], f"{prefix}.operations_per_sample", 1
    )
    operation_cap = require_integer(
        measurement["maximum_operations_per_sample"],
        f"{prefix}.maximum_operations_per_sample",
        1,
    )
    if operations > operation_cap:
        fail(f"{prefix}.operations_per_sample exceeds its recorded hard limit")
    thread_scaling_batch_is_stable = True
    sample_count = require_integer(measurement["sample_count"], f"{prefix}.sample_count", 3)
    if sample_count != policy["samples_per_measurement"]:
        fail(f"{prefix}.sample_count differs from policy")

    statistics = measurement["statistics"]
    mad_ppm, range_ppm = validate_statistics(statistics, f"{prefix}.statistics")
    if primitive == "thread-scaling-memory-scan":
        median = statistics["median"]
        median_batch_picoseconds = median * operations
        target_batch_picoseconds = policy["target_sample_duration_ms"] * 1_000_000_000
        thread_scaling_batch_is_stable = operations < operation_cap and not (
            median_batch_picoseconds * 1_000_000
            < target_batch_picoseconds * THREAD_SCALING_BATCH_MINIMUM_TARGET_PPM
            or median_batch_picoseconds * 1_000_000
            > target_batch_picoseconds * THREAD_SCALING_BATCH_MAXIMUM_TARGET_PPM
        )
    retry_history = measurement["retry_history"]
    if not isinstance(retry_history, list):
        fail(f"{prefix}.retry_history must be a list")
    retry_limit = policy["measurement_retry_limit"]
    previous_attempt = 0
    for retry_index, attempt in enumerate(retry_history):
        attempt = require_object(
            attempt,
            f"{prefix}.retry_history[{retry_index}]",
            {"attempt", "status", "statistics"},
        )
        ordinal = require_integer(
            attempt["attempt"], f"{prefix}.retry_history[{retry_index}].attempt", 1
        )
        if ordinal != previous_attempt + 1:
            fail(f"{prefix}.retry_history[{retry_index}].attempt must ascend from 1")
        previous_attempt = ordinal
        if attempt["status"] != "unstable":
            fail(f"{prefix}.retry_history[{retry_index}].status must be unstable")
        validate_statistics(
            attempt["statistics"], f"{prefix}.retry_history[{retry_index}].statistics"
        )
    if len(retry_history) > retry_limit - 1:
        fail(f"{prefix}.retry_history exceeds measurement_retry_limit")
    if measurement["status"] == "rejected" and retry_history:
        fail(f"{prefix}.retry_history must be empty for rejected measurements")

    correctness = require_object(
        measurement["correctness"],
        f"{prefix}.correctness",
        {"status", "result_digest_blake3", "reference_digest_blake3"},
    )
    if correctness["status"] not in {"passed", "failed"}:
        fail(f"{prefix}.correctness.status is invalid")
    result_digest = require_digest(
        correctness["result_digest_blake3"], f"{prefix}.correctness.result_digest_blake3"
    )
    reference_digest = require_digest(
        correctness["reference_digest_blake3"], f"{prefix}.correctness.reference_digest_blake3"
    )
    passed = correctness["status"] == "passed"
    if passed != (result_digest == reference_digest):
        fail(f"{prefix} correctness status disagrees with result digests")
    scheduler_input = primitive in ROBUST_MEDIAN_MEASUREMENT_PRIMITIVES
    stable = (
        passed
        and mad_ppm <= policy["maximum_relative_mad_ppm"]
        and (scheduler_input or range_ppm <= policy["maximum_relative_range_ppm"])
        and thread_scaling_batch_is_stable
    )
    expected_status = "stable" if stable else ("rejected" if not passed else "unstable")
    if measurement["status"] != expected_status:
        fail(f"{prefix}.status must be {expected_status}")
    return (primitive, input_size, input_unit, variant), passed


def validate_thread_scaling(value: Any, measurements: list[dict[str, Any]]) -> None:
    summary = require_object(
        value,
        "thread_scaling",
        {
            "binding",
            "physical_core_boundary",
            "logical_processor_boundary",
            "measured_thread_counts",
            "status",
            "physical_peak_threads",
            "physical_peak_bytes_per_second",
            "smt_peak_threads",
            "smt_peak_bytes_per_second",
            "smt_to_physical_throughput_ppm",
            "smt_recommended",
            "recommended_worker_count",
            "recommendation",
        },
    )
    binding = summary["binding"]
    if binding not in {"unbound", "linux-sched-affinity", "inconsistent"}:
        fail("thread_scaling.binding is invalid")
    physical = require_integer(
        summary["physical_core_boundary"], "thread_scaling.physical_core_boundary", 1
    )
    logical = require_integer(
        summary["logical_processor_boundary"], "thread_scaling.logical_processor_boundary", 1
    )
    if physical > logical:
        fail("thread_scaling physical boundary exceeds logical boundary")
    counts = summary["measured_thread_counts"]
    if not isinstance(counts, list) or counts != sorted(set(counts)):
        fail("thread_scaling.measured_thread_counts must be unique and ordered")
    for index, count in enumerate(counts):
        require_integer(count, f"thread_scaling.measured_thread_counts[{index}]", 1)
        if count > logical:
            fail("thread_scaling measured count exceeds logical boundary")

    cells = [
        measurement
        for measurement in measurements
        if measurement["primitive"] == "thread-scaling-memory-scan"
    ]
    actual_counts = sorted(measurement["input_size"] for measurement in cells)
    if counts != actual_counts:
        fail("thread_scaling measured counts differ from scaling measurements")
    if cells and not {1, physical, logical}.issubset(set(counts)):
        fail("thread_scaling curve omits a boundary point")
    observed_bindings = {
        "linux-sched-affinity"
        if cell["variant"].endswith("linux-affinity")
        else "unbound"
        if cell["variant"].endswith("unbound")
        else "invalid"
        for cell in cells
    }
    expected_binding = (
        next(iter(observed_bindings)) if len(observed_bindings) == 1 else "inconsistent"
    )
    if cells and binding != expected_binding:
        fail("thread_scaling.binding disagrees with scaling measurement variants")
    for cell in cells:
        suffix = "linux-affinity" if binding == "linux-sched-affinity" else "unbound"
        expected_variant = (
            f"persistent-workers-physical-range-{suffix}"
            if cell["input_size"] <= physical
            else f"persistent-workers-smt-range-{suffix}"
        )
        if cell["variant"] != expected_variant or cell["input_unit"] != "threads":
            fail("thread_scaling measurement variant or unit disagrees with its boundary")

    stable = binding != "inconsistent" and bool(cells) and all(
        cell["status"] == "stable"
        and cell["correctness"]["status"] == "passed"
        and cell["statistics"]["median_bytes_per_second"] is not None
        for cell in cells
    )
    expected_status = "stable" if stable else "unavailable"
    if summary["status"] != expected_status:
        fail(f"thread_scaling.status must be {expected_status}")
    require_string(summary["recommendation"], "thread_scaling.recommendation")
    output_fields = (
        "physical_peak_threads",
        "physical_peak_bytes_per_second",
        "smt_peak_threads",
        "smt_peak_bytes_per_second",
        "smt_to_physical_throughput_ppm",
        "recommended_worker_count",
    )
    if not stable:
        if any(summary[field] is not None for field in output_fields) or summary["smt_recommended"] is not False:
            fail("unavailable thread_scaling must not recommend a worker count")
        return

    def peak(candidates: list[dict[str, Any]]) -> tuple[int, int] | None:
        if not candidates:
            return None
        selected = max(
            candidates,
            key=lambda cell: (cell["statistics"]["median_bytes_per_second"], -cell["input_size"]),
        )
        return selected["input_size"], selected["statistics"]["median_bytes_per_second"]

    physical_peak = peak([cell for cell in cells if cell["input_size"] <= physical])
    if physical_peak is None:
        fail("stable thread_scaling has no physical-range point")
    smt_peak = peak([cell for cell in cells if cell["input_size"] > physical])
    smt_ratio = None
    if smt_peak is not None:
        smt_ratio = smt_peak[1] * 1_000_000 // physical_peak[1]
    smt_recommended = smt_ratio is not None and smt_ratio >= SMT_RECOMMENDATION_RATIO_PPM
    recommended = smt_peak[0] if smt_recommended else physical_peak[0]
    expected = {
        "physical_peak_threads": physical_peak[0],
        "physical_peak_bytes_per_second": physical_peak[1],
        "smt_peak_threads": None if smt_peak is None else smt_peak[0],
        "smt_peak_bytes_per_second": None if smt_peak is None else smt_peak[1],
        "smt_to_physical_throughput_ppm": smt_ratio,
        "smt_recommended": smt_recommended,
        "recommended_worker_count": recommended,
    }
    for field, expected_value in expected.items():
        if summary[field] != expected_value:
            fail(f"thread_scaling.{field} disagrees with the measured curve")


def validate_io_scaling(value: Any, measurements: list[dict[str, Any]]) -> None:
    summary = require_object(
        value,
        "io_scaling",
        {
            "binding",
            "measured_queue_depths",
            "status",
            "peak_queue_depth",
            "peak_bytes_per_second",
            "recommended_io_slots",
            "recommendation",
        },
    )
    if summary["binding"] != "buffered-sync-workers":
        fail("io_scaling.binding is not the portable v1 adapter")
    depths = summary["measured_queue_depths"]
    if not isinstance(depths, list) or depths != sorted(set(depths)):
        fail("io_scaling.measured_queue_depths must be unique and ordered")
    for index, depth in enumerate(depths):
        require_integer(depth, f"io_scaling.measured_queue_depths[{index}]", 1)
        if depth > 64:
            fail("io_scaling measured depth exceeds the v1 safety ceiling")
    cells = [
        measurement
        for measurement in measurements
        if measurement["primitive"] == "queue-depth-random-read"
    ]
    if depths != sorted(measurement["input_size"] for measurement in cells):
        fail("io_scaling depths differ from queue-depth measurements")
    for cell in cells:
        if (
            cell["variant"] != "persistent-sync-workers-buffered-4k"
            or cell["input_unit"] != "outstanding-reads"
        ):
            fail("io_scaling measurement variant or unit is invalid")
    stable = bool(cells) and all(
        cell["status"] == "stable"
        and cell["correctness"]["status"] == "passed"
        and cell["statistics"]["median_bytes_per_second"] is not None
        for cell in cells
    )
    expected_status = "stable" if stable else "unavailable"
    if summary["status"] != expected_status:
        fail(f"io_scaling.status must be {expected_status}")
    require_string(summary["recommendation"], "io_scaling.recommendation")
    if not stable:
        if any(
            summary[field] is not None
            for field in ("peak_queue_depth", "peak_bytes_per_second", "recommended_io_slots")
        ):
            fail("unavailable io_scaling must not recommend I/O concurrency")
        return
    peak = max(
        cells,
        key=lambda cell: (cell["statistics"]["median_bytes_per_second"], -cell["input_size"]),
    )
    peak_bytes = peak["statistics"]["median_bytes_per_second"]
    recommended = min(
        cell["input_size"]
        for cell in cells
        if cell["statistics"]["median_bytes_per_second"] * 1_000_000
        >= peak_bytes * IO_RECOMMENDATION_FLOOR_PPM
    )
    expected = {
        "peak_queue_depth": peak["input_size"],
        "peak_bytes_per_second": peak_bytes,
        "recommended_io_slots": recommended,
    }
    for field, expected_value in expected.items():
        if summary[field] != expected_value:
            fail(f"io_scaling.{field} disagrees with the measured curve")


def validate_numa_measurements(
    measurements: list[dict[str, Any]],
) -> dict[tuple[int, int], dict[str, Any]] | None:
    cells = [
        measurement
        for measurement in measurements
        if measurement["primitive"] == "numa-memory-read"
    ]
    if not cells:
        return None
    parsed: list[tuple[dict[str, Any], re.Match[str]]] = []
    for cell in cells:
        match = NUMA_VARIANT.fullmatch(cell["variant"])
        if match is None:
            fail("NUMA calibration variant is not canonical")
        if cell["input_unit"] != "working-set-bytes":
            fail("NUMA calibration input unit is not working-set-bytes")
        if cell["input_size"] != 8 * 1024 * 1024:
            fail("NUMA calibration working set differs from the frozen v1 size")
        if cell["bytes_per_operation"] != cell["input_size"]:
            fail("NUMA calibration byte accounting differs from its working set")
        if cell["statistics"]["unit"] != "picoseconds_per_operation":
            fail("NUMA calibration timing unit is not picoseconds_per_operation")
        parsed.append((cell, match))
    source_nodes = {int(match.group("source")) for _, match in parsed}
    reader_nodes = {int(match.group("reader")) for _, match in parsed}
    if len(source_nodes) < 2 or source_nodes != reader_nodes:
        fail("NUMA calibration must cover the same two or more source and reader nodes")
    expected_pairs = {
        (source, reader) for source in source_nodes for reader in reader_nodes
    }
    matrix: dict[tuple[int, int], dict[str, Any]] = {}
    reader_cpus: dict[int, int] = {}
    for cell, match in parsed:
        source = int(match.group("source"))
        reader = int(match.group("reader"))
        cpu = int(match.group("cpu"))
        pair = (source, reader)
        if pair in matrix:
            fail("NUMA calibration repeats one directed source/reader cell")
        if reader in reader_cpus and reader_cpus[reader] != cpu:
            fail("NUMA calibration uses inconsistent representative CPUs for one reader node")
        reader_cpus[reader] = cpu
        matrix[pair] = cell
    if set(matrix) != expected_pairs:
        fail("NUMA calibration must contain the complete directed node matrix")
    fail("NUMA calibration v1 has no safe exact page-residency evidence")


def validate_receipt(receipt: Any) -> None:
    root = require_object(
        receipt,
        "receipt",
        {
            "schema",
            "mode",
            "status",
            "accepted_for_scheduling",
            "cache_status",
            "elapsed_ms",
            "identity",
            "policy",
            "feature_detection",
            "measurements",
            "selected_kernels",
            "thread_scaling",
            "io_scaling",
            "coverage",
            "claims",
        },
    )
    if root["schema"] != SCHEMA:
        fail("receipt schema is not native hardware calibration v1")
    if root["mode"] not in {"quick", "thorough"}:
        fail("receipt mode is invalid")
    if not isinstance(root["accepted_for_scheduling"], bool):
        fail("accepted_for_scheduling must be boolean")
    if root["cache_status"] not in {"disabled", "hit", "miss"}:
        fail("cache_status is invalid")
    elapsed_ms = require_integer(root["elapsed_ms"], "elapsed_ms", 1)
    validate_identity(root["identity"])
    policy = validate_policy(root["policy"])

    measurements = root["measurements"]
    if not isinstance(measurements, list) or not measurements:
        fail("measurements must be a non-empty array")
    keys: list[tuple[Any, ...]] = []
    correctness: list[bool] = []
    for index, measurement in enumerate(measurements):
        key, passed = validate_measurement(measurement, policy, index)
        keys.append(key)
        correctness.append(passed)
    if len(keys) != len(set(keys)):
        fail("measurement identities are not unique")

    feature = require_object(
        root["feature_detection"],
        "feature_detection",
        {"instruction_sets", "differential_tests_passed"},
    )
    instructions = feature["instruction_sets"]
    if not isinstance(instructions, list) or any(not isinstance(item, str) or not item for item in instructions):
        fail("feature_detection.instruction_sets is invalid")
    if len(instructions) != len(set(instructions)):
        fail("feature_detection.instruction_sets contains duplicates")
    all_correct = all(correctness)
    if feature["differential_tests_passed"] is not all_correct:
        fail("differential_tests_passed disagrees with measurements")

    timing_valid = policy["minimum_duration_ms"] <= elapsed_ms <= policy["maximum_duration_ms"]
    scheduling_inputs_stable = all(
        item["status"] == "stable"
        for item in measurements
        if item["primitive"] in REQUIRED_SCHEDULER_MEASUREMENT_PRIMITIVES
    )
    accepted = all_correct and timing_valid and scheduling_inputs_stable
    if root["accepted_for_scheduling"] is not accepted:
        fail("accepted_for_scheduling disagrees with correctness, timing, or scheduler variance")
    if root["cache_status"] == "hit" and not accepted:
        fail("a cache hit must be accepted for scheduling")
    expected_root_status = "stable" if accepted else ("rejected" if not all_correct or not timing_valid else "unstable")
    if root["status"] != expected_root_status:
        fail(f"receipt status must be {expected_root_status}")

    selections = root["selected_kernels"]
    if not isinstance(selections, list):
        fail("selected_kernels must be an array")
    selected_keys = []
    for index, selection_value in enumerate(selections):
        selection = require_object(
            selection_value,
            f"selected_kernels[{index}]",
            {"primitive", "input_size", "input_unit", "variant", "reason"},
        )
        key = (
            require_string(selection["primitive"], f"selected_kernels[{index}].primitive"),
            require_integer(selection["input_size"], f"selected_kernels[{index}].input_size", 1),
            require_string(selection["input_unit"], f"selected_kernels[{index}].input_unit"),
            require_string(selection["variant"], f"selected_kernels[{index}].variant"),
        )
        require_string(selection["reason"], f"selected_kernels[{index}].reason")
        selected_keys.append(key)
    expected_selections = (
        {
            key
            for key, measurement in zip(keys, measurements, strict=True)
            if measurement["status"] == "stable"
        }
        if accepted
        else set()
    )
    if set(selected_keys) != expected_selections or len(selected_keys) != len(expected_selections):
        fail("selected_kernels must exactly match accepted stable measurements")

    validate_thread_scaling(root["thread_scaling"], measurements)
    validate_io_scaling(root["io_scaling"], measurements)
    numa_matrix = validate_numa_measurements(measurements)

    coverage = require_object(root["coverage"], "coverage", {"measured", "unsupported"})
    measured = coverage["measured"]
    if not isinstance(measured, list) or measured != sorted(set(measured)):
        fail("coverage.measured must be unique and canonically ordered")
    if set(measured) != {key[0] for key in keys}:
        fail("coverage.measured differs from measurement primitives")
    unsupported = coverage["unsupported"]
    if not isinstance(unsupported, list):
        fail("coverage.unsupported must be an array")
    unsupported_names = []
    for index, item_value in enumerate(unsupported):
        item = require_object(item_value, f"coverage.unsupported[{index}]", {"primitive", "reason"})
        unsupported_names.append(require_string(item["primitive"], f"coverage.unsupported[{index}].primitive"))
        require_string(item["reason"], f"coverage.unsupported[{index}].reason")
    if len(unsupported_names) != len(set(unsupported_names)):
        fail("coverage.unsupported contains duplicate primitives")
    if set(unsupported_names) & set(measured):
        fail("coverage cannot report the same primitive as measured and unsupported")
    numa_unsupported = "numa-local-remote-memory" in unsupported_names
    if (numa_matrix is not None) == numa_unsupported:
        fail("NUMA calibration must be either measured or explicitly unsupported")
    if root["claims"] != []:
        fail("calibration receipts cannot carry performance claims")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", required=True, type=Path)
    args = parser.parse_args()
    try:
        with args.receipt.open(encoding="utf-8") as handle:
            validate_receipt(json.load(handle))
    except (OSError, json.JSONDecodeError, CalibrationValidationError) as error:
        print(f"native hardware calibration check failed: {error}")
        return 1
    print(f"native hardware calibration check passed: {args.receipt}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
