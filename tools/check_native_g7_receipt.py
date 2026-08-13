#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation for one controlled Native G7 receipt."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX32 = re.compile(r"[0-9a-f]{32}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
MAX_INITIAL_ANN_BULK_PARTITIONS = 111
G7_LOGICAL_ANN_PARTITIONS = 64
G7_PREFERRED_ANN_PARTITIONS = 32
G7_ANN_PARTITION_POLICY = "g7-fixed-64-logical-partitions-v1"
G7_GROUP_COMMIT_COHORT_WIDTH = 32
G7_GROUP_COMMIT_QUEUE_CAPACITY = 64
G7_PROFILE_PATH = Path(__file__).resolve().parents[1] / "config/native-g7-readiness-profile.json"


class GateFailure(ValueError):
    pass


@dataclass(frozen=True)
class G7ProfileAuthority:
    cells: frozenset[str]
    counters: frozenset[str]
    observations: int
    warmup: int
    documents: int
    vectors: int
    vector_dimension: int
    warm_targets: dict[str, tuple[int, int]]
    advisory_targets: dict[str, tuple[int, int]]


def _latency_targets(value: object, name: str) -> dict[str, tuple[int, int]]:
    if not isinstance(value, dict):
        raise GateFailure(f"G7 {name} latency targets are invalid")
    targets: dict[str, tuple[int, int]] = {}
    for surface, target in value.items():
        if (
            not isinstance(surface, str)
            or not surface
            or not isinstance(target, dict)
            or set(target) != {"p50", "p99"}
        ):
            raise GateFailure(f"G7 {name} latency targets are invalid")
        p50 = target["p50"]
        p99 = target["p99"]
        if (
            not isinstance(p50, int)
            or isinstance(p50, bool)
            or not isinstance(p99, int)
            or isinstance(p99, bool)
            or p50 <= 0
            or p99 < p50
        ):
            raise GateFailure(f"G7 {name} latency targets are invalid")
        targets[surface] = (p50, p99)
    return targets


def profile_authority(profile: object) -> G7ProfileAuthority:
    if not isinstance(profile, dict):
        raise GateFailure("G7 readiness profile must be an object")
    cells_value = profile.get("required_cells")
    counters_value = profile.get("required_counters")
    dataset = profile.get("required_dataset")
    observations = profile.get("minimum_hot_observations")
    warmup = profile.get("required_hot_warmup")
    if (
        not isinstance(cells_value, list)
        or not cells_value
        or any(not isinstance(cell, str) or not cell for cell in cells_value)
        or len(cells_value) != len(set(cells_value))
        or not isinstance(counters_value, list)
        or not counters_value
        or any(not isinstance(counter, str) or not counter for counter in counters_value)
        or len(counters_value) != len(set(counters_value))
        or not isinstance(dataset, dict)
        or set(dataset) != {"documents", "vectors", "vector_dimension"}
        or not isinstance(observations, int)
        or isinstance(observations, bool)
        or observations <= 0
        or not isinstance(warmup, int)
        or isinstance(warmup, bool)
        or warmup <= 0
        or any(
            not isinstance(dataset[field], int)
            or isinstance(dataset[field], bool)
            or dataset[field] <= 0
            for field in ("documents", "vectors", "vector_dimension")
        )
    ):
        raise GateFailure("G7 normative measurement authority is invalid")
    cells = frozenset(cells_value)
    warm_targets = _latency_targets(profile.get("warm_targets_nanoseconds"), "warm")
    advisory_targets = _latency_targets(
        profile.get("advisory_targets_nanoseconds"),
        "advisory",
    )
    if set(warm_targets).intersection(advisory_targets) or (
        set(warm_targets) | set(advisory_targets)
    ) != set(cells):
        raise GateFailure("G7 latency targets do not cover the required cells exactly")
    return G7ProfileAuthority(
        cells=cells,
        counters=frozenset(counters_value),
        observations=observations,
        warmup=warmup,
        documents=dataset["documents"],
        vectors=dataset["vectors"],
        vector_dimension=dataset["vector_dimension"],
        warm_targets=warm_targets,
        advisory_targets=advisory_targets,
    )


def load_profile_authority(path: Path = G7_PROFILE_PATH) -> G7ProfileAuthority:
    try:
        profile = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateFailure(f"G7 readiness profile could not be loaded: {error}") from error
    return profile_authority(profile)


DEFAULT_AUTHORITY = load_profile_authority()
CELLS = set(DEFAULT_AUTHORITY.cells)
COUNTERS = set(DEFAULT_AUTHORITY.counters)


def resolve_expected_tree(
    expected_commit: str,
    *,
    repository: Path = Path(__file__).resolve().parents[1],
) -> str:
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("source commit is not canonical SHA-1")
    try:
        completed = subprocess.run(
            ("git", "rev-parse", "--verify", f"{expected_commit}^{{tree}}"),
            cwd=repository,
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise GateFailure("G7 source tree authority could not be resolved") from error
    source_tree = completed.stdout.strip()
    if HEX40.fullmatch(source_tree) is None:
        raise GateFailure("resolved G7 source tree is not canonical SHA-1")
    return source_tree


def validate(
    payload: dict[str, Any],
    expected_commit: str,
    *,
    expected_tree: str | None = None,
    profile: object | None = None,
) -> dict[str, Any]:
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("source commit is not canonical SHA-1")
    if expected_tree is None:
        expected_tree = resolve_expected_tree(expected_commit)
    elif HEX40.fullmatch(expected_tree) is None:
        raise GateFailure("expected source tree is not canonical SHA-1")
    authority = DEFAULT_AUTHORITY if profile is None else profile_authority(profile)
    required = {
        "schema", "gate", "status", "evidence_class", "source_commit", "platform",
        "state", "concurrency", "background_mode", "dataset", "hardware", "cells", "counters", "saturation",
        "background_interference", "claims", "closure_declared", "physical_observation",
        "build", "workload", "durability", "proofs_included", "correctness",
        "initial_ann_bulk", "execution_authority",
    }
    if set(payload) != required:
        raise GateFailure("G7 receipt fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-receipt-v4"
        or payload["gate"] != "G7"
        or payload["status"] != "passed"
        or payload["evidence_class"] != "closure-candidate"
        or payload["source_commit"] != expected_commit
        or payload["claims"] != []
        or payload["closure_declared"] is not False
        or payload["platform"] not in {"linux", "darwin"}
    ):
        raise GateFailure("G7 receipt identity or open state mismatch")
    if payload["state"] not in {"warm", "cold"} or payload["concurrency"] not in {1, 8, 32}:
        raise GateFailure("G7 state or concurrency is invalid")
    if payload["background_mode"] not in {"control", "interference"}:
        raise GateFailure("G7 background mode is invalid")
    dataset = payload["dataset"]
    if (
        not isinstance(dataset, dict)
        or dataset.get("observations") != authority.observations
        or dataset.get("warmup") != authority.warmup
        or dataset.get("search_documents") != authority.documents
        or dataset.get("vector_count") != authority.vectors
        or dataset.get("vector_dimension") != authority.vector_dimension
        or not isinstance(dataset.get("generator"), str)
        or HEX64.fullmatch(dataset.get("digest", "")) is None
    ):
        raise GateFailure("G7 dataset measurement counts or corpus differ from authority")
    build = payload["build"]
    if (
        not isinstance(build, dict)
        or set(build) != {
            "rustc", "cargo", "profile", "target", "os", "binary_sha256",
            "source_tree",
        }
        or build.get("profile") != "release"
        or (
            payload["platform"] == "linux"
            and "linux" not in str(build.get("target", ""))
        )
        or (
            payload["platform"] == "darwin"
            and "apple-darwin" not in str(build.get("target", ""))
        )
        or any(not isinstance(build.get(field), str) or not build[field] for field in ("rustc", "cargo", "target", "os"))
        or HEX64.fullmatch(str(build.get("binary_sha256", ""))) is None
        or build.get("source_tree") != expected_tree
    ):
        raise GateFailure("G7 build identity or source tree is incomplete")
    workload = payload["workload"]
    expected_workload = {
        "structure_keys": 2_048,
        "sql_rows": 128,
        "point_value_bytes": 64,
        "search_documents": 1_000_000,
        "vector_count": 1_000_000,
        "vector_dimension": 384,
        "lexical_rare_documents": 1,
        "filtered_documents": 500_000,
        "result_limit": 10,
        "lexical_index_state": "committed-hot",
        "vector_index_state": "committed-hot",
    }
    if workload != expected_workload:
        raise GateFailure("G7 workload envelope differs from the normative corpus")
    if payload["durability"] != {
        "read_seed": "memory-committed",
        "search_seed": "memory-committed",
        "commit_cell": "group-physical-sync",
    } or payload["proofs_included"] is not False:
        raise GateFailure("G7 durability or proof boundary differs")
    if payload["correctness"] != {
        "cell_assertions": "passed",
        "ann_recall_floor": 0.95,
        "cross_engine_visibility": "native-same-snapshot-search",
    }:
        raise GateFailure("G7 correctness evidence differs")
    validate_initial_ann_bulk(payload["initial_ann_bulk"], payload)
    validate_execution_authority_evidence(
        payload["execution_authority"],
        payload["initial_ann_bulk"],
        payload["background_mode"],
    )
    hardware = payload["hardware"]
    hardware_fields = {"dedicated", "cpu", "topology", "ram_bytes", "storage", "filesystem", "governor", "affinity", "priority", "background_services", "virtualization"}
    if not isinstance(hardware, dict) or set(hardware) != hardware_fields or hardware.get("dedicated") is not True or hardware.get("virtualization") != "none":
        raise GateFailure("G7 hardware is not dedicated and fully disclosed")
    saturation = payload["saturation"]
    if (
        not isinstance(saturation, dict)
        or saturation.get("status") != "measured"
        or saturation.get("levels") != [1, 8, 32]
        or saturation.get("method") != "executed-concurrency-sweep"
        or set(saturation.get("throughput_per_second", {})) != set(authority.cells)
    ):
        raise GateFailure("G7 saturation evidence is incomplete")
    for levels in saturation["throughput_per_second"].values():
        if set(levels) != {"1", "8", "32"} or any(not isinstance(value, (int, float)) or value <= 0 for value in levels.values()):
            raise GateFailure("G7 saturation throughput is invalid")
    background = payload["background_interference"]
    expected_background = "measured" if payload["background_mode"] == "interference" else "control"
    if not isinstance(background, dict) or background.get("status") != expected_background:
        raise GateFailure("G7 background-interference evidence is incomplete")
    if expected_background == "measured" and (
        not isinstance(background.get("operations"), int)
        or background["operations"] <= 0
        or set(background.get("p99_ratio_by_cell", {})) != set(authority.cells)
    ):
        raise GateFailure("G7 background control comparison is incomplete")
    cells = payload["cells"]
    if not isinstance(cells, dict) or set(cells) != set(authority.cells):
        raise GateFailure("G7 cell identity is invalid")
    for name, cell in cells.items():
        if not isinstance(cell, dict) or cell.get("status") != "measured":
            raise GateFailure(f"G7 cell is not measured: {name}")
        for field in ("p50", "p95", "p99", "p999", "maximum"):
            value = cell.get(field)
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise GateFailure(f"G7 latency field is invalid: {name}.{field}")
        if not isinstance(cell.get("throughput_per_second"), (int, float)) or cell["throughput_per_second"] <= 0:
            raise GateFailure(f"G7 throughput is invalid: {name}")
        materialization = cell.get("materialization")
        if (
            not isinstance(materialization, dict)
            or set(materialization)
            != {"full_state_loads", "full_catalog_loads", "provider"}
            or materialization.get("full_state_loads") != 0
            or materialization.get("full_catalog_loads") != 0
            or materialization.get("provider") != "process-interval-atomic-counters"
        ):
            raise GateFailure(f"G7 hot path materialized complete state: {name}")
        if (
            payload["state"] == "warm"
            and payload["concurrency"] == 1
            and payload["background_mode"] == "control"
            and name in authority.warm_targets
        ):
            target_p50, target_p99 = authority.warm_targets[name]
            if cell["p50"] > target_p50 or cell["p99"] > target_p99:
                raise GateFailure(f"G7 latency target missed: {name}")
    ann_recall = cells["ann-top10-recall-095"].get("recall_at_10")
    if not isinstance(ann_recall, (int, float)) or ann_recall < 0.95:
        raise GateFailure("G7 ANN recall floor was not met")
    validate_ann_read_view_cell(
        cells["ann-top10-recall-095"],
        payload["initial_ann_bulk"],
        payload["dataset"]["observations"],
    )
    validate_hybrid_read_view_cell(
        cells["hybrid-top10"],
        cells["ann-top10-recall-095"],
        payload["dataset"]["observations"],
    )
    validate_bm25_read_view_cell(
        cells["bm25-top10"],
        cells["hybrid-top10"],
        payload["dataset"]["observations"],
    )
    validate_filtered_bm25_read_view_cell(
        cells["filtered-bm25-top10"],
        cells["hybrid-top10"],
        payload["dataset"]["observations"],
    )
    validate_strict_group_commit_cell(
        cells["strict-group-commit"],
        payload["dataset"]["observations"],
        payload["concurrency"],
    )
    counters = payload["counters"]
    if set(counters) != set(authority.counters):
        raise GateFailure("G7 counters are incomplete")
    for name, counter in counters.items():
        if not isinstance(counter, dict) or counter.get("status") != "measured":
            raise GateFailure(f"G7 counter status is invalid: {name}")
        if not isinstance(counter.get("value"), int) or counter["value"] < 0:
            raise GateFailure(f"G7 counter value is invalid: {name}")
        if (
            not isinstance(counter.get("unit"), str)
            or not counter["unit"]
            or not isinstance(counter.get("provider"), str)
            or counter["provider"] in {"", "none"}
        ):
            raise GateFailure(f"G7 counter provenance is invalid: {name}")
    physical = payload["physical_observation"]
    physical_fields = {
        "page_count", "physical_page_reads", "wal_bytes",
        "process_full_state_loads", "process_full_catalog_loads",
    }
    if not isinstance(physical, dict) or set(physical) != physical_fields:
        raise GateFailure("G7 physical observation fields differ")
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value < 0
        for value in physical.values()
    ):
        raise GateFailure("G7 physical observation contains an invalid counter")
    if physical["page_count"] == 0 or physical["wal_bytes"] == 0:
        raise GateFailure("G7 physical observation does not cover persisted Native state")
    return {
        "schema": "hyphae-native-g7-receipt-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "state": payload["state"],
        "concurrency": payload["concurrency"],
        "measured_cells": len(cells),
        "counter_status": {name: value["status"] for name, value in counters.items()},
        "claims": [],
        "closure_declared": False,
    }


def validate_strict_group_commit_cell(
    cell: Any,
    expected_observations: int,
    expected_concurrency: int,
) -> None:
    """Validate one fixed-width, bounded, physically durable commit interval."""
    latency_fields = ("p50", "p95", "p99", "p999", "maximum")
    if (
        not isinstance(cell, dict)
        or cell.get("status") != "measured"
        or cell.get("durability") != "group-physical-sync"
        or not isinstance(cell.get("throughput_per_second"), (int, float))
        or isinstance(cell.get("throughput_per_second"), bool)
        or not math.isfinite(cell["throughput_per_second"])
        or cell["throughput_per_second"] <= 0
        or any(
            not isinstance(cell.get(field), int)
            or isinstance(cell[field], bool)
            or cell[field] <= 0
            for field in latency_fields
        )
        or any(
            cell[left] > cell[right]
            for left, right in zip(latency_fields, latency_fields[1:])
        )
        or not isinstance(expected_observations, int)
        or isinstance(expected_observations, bool)
        or expected_observations < G7_GROUP_COMMIT_COHORT_WIDTH
        or expected_concurrency not in {1, 8, 32}
        or isinstance(expected_concurrency, bool)
    ):
        raise GateFailure("G7 strict group-commit cell identity is invalid")
    evidence = cell.get("group_commit_evidence")
    fields = {
        "schema", "latency_scope", "throughput_scope", "submission_mode",
        "producer_concurrency", "maximum_active_producers", "cohort_width",
        "scheduler_queue_capacity", "outstanding_limit", "maximum_outstanding",
        "logical_commits", "cohort_count", "final_cohort_size",
        "cohort_size_histogram", "cohort_position_histogram", "first_commit_csn",
        "last_commit_csn", "distinct_commit_csns", "commit_receipt_digest_algorithm",
        "commit_receipt_digest", "page_synchronizations", "wal_synchronizations",
        "cohort_execution_nanos_total", "page_synchronization_nanos_total",
        "wal_synchronization_nanos_total", "timing_sample_count",
        "timings_nanoseconds", "reopen",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise GateFailure("G7 strict group-commit evidence fields mismatch")
    _validate_group_commit_configuration(evidence, expected_concurrency)
    _validate_group_commit_cohorts(evidence, expected_observations)
    _validate_group_commit_receipts(evidence, expected_observations)
    _validate_group_commit_synchronization(evidence)
    _validate_group_commit_timings(cell, evidence, expected_observations)
    _validate_group_commit_reopen(evidence, expected_observations)


def _validate_group_commit_configuration(
    evidence: dict[str, Any],
    expected_concurrency: int,
) -> None:
    if (
        evidence["schema"]
        != "hyphae-native-g7-strict-group-commit-evidence-v1"
        or evidence["latency_scope"]
        != "scheduler-enqueue-through-durable-response-v1"
        or evidence["throughput_scope"] != "bounded-cohort-window-wall-time-v1"
        or evidence["submission_mode"] != "explicit-bounded-cohort-v1"
        or evidence["producer_concurrency"] != expected_concurrency
        or isinstance(evidence["producer_concurrency"], bool)
        or evidence["maximum_active_producers"] != expected_concurrency
        or isinstance(evidence["maximum_active_producers"], bool)
        or evidence["cohort_width"] != G7_GROUP_COMMIT_COHORT_WIDTH
        or isinstance(evidence["cohort_width"], bool)
        or evidence["scheduler_queue_capacity"] != G7_GROUP_COMMIT_QUEUE_CAPACITY
        or isinstance(evidence["scheduler_queue_capacity"], bool)
        or evidence["outstanding_limit"] != G7_GROUP_COMMIT_COHORT_WIDTH
        or isinstance(evidence["outstanding_limit"], bool)
        or evidence["maximum_outstanding"] != G7_GROUP_COMMIT_COHORT_WIDTH
        or isinstance(evidence["maximum_outstanding"], bool)
    ):
        raise GateFailure("G7 strict group-commit configuration is invalid")


def _validate_group_commit_cohorts(
    evidence: dict[str, Any],
    expected_observations: int,
) -> None:
    full_cohorts, remainder = divmod(
        expected_observations,
        G7_GROUP_COMMIT_COHORT_WIDTH,
    )
    expected_cohort_count = full_cohorts + int(remainder > 0)
    expected_size_histogram = {str(G7_GROUP_COMMIT_COHORT_WIDTH): full_cohorts}
    if remainder:
        expected_size_histogram[str(remainder)] = 1
    expected_position_histogram = {
        str(position): full_cohorts + int(position < remainder)
        for position in range(G7_GROUP_COMMIT_COHORT_WIDTH)
    }
    size_histogram = evidence["cohort_size_histogram"]
    position_histogram = evidence["cohort_position_histogram"]
    if (
        evidence["logical_commits"] != expected_observations
        or isinstance(evidence["logical_commits"], bool)
        or evidence["cohort_count"] != expected_cohort_count
        or isinstance(evidence["cohort_count"], bool)
        or evidence["final_cohort_size"]
        != (remainder or G7_GROUP_COMMIT_COHORT_WIDTH)
        or isinstance(evidence["final_cohort_size"], bool)
        or not _is_canonical_count_histogram(size_histogram)
        or size_histogram != expected_size_histogram
        or not _is_canonical_count_histogram(position_histogram)
        or position_histogram != expected_position_histogram
    ):
        raise GateFailure("G7 strict group-commit cohort evidence is invalid")


def _is_canonical_count_histogram(value: Any) -> bool:
    return (
        isinstance(value, dict)
        and all(isinstance(key, str) and key.isascii() and key.isdecimal() for key in value)
        and all(
            isinstance(count, int) and not isinstance(count, bool) and count > 0
            for count in value.values()
        )
    )


def _validate_group_commit_receipts(
    evidence: dict[str, Any],
    expected_observations: int,
) -> None:
    first_csn = evidence["first_commit_csn"]
    last_csn = evidence["last_commit_csn"]
    if (
        not isinstance(first_csn, int)
        or isinstance(first_csn, bool)
        or first_csn <= 0
        or not isinstance(last_csn, int)
        or isinstance(last_csn, bool)
        or last_csn < first_csn
        or last_csn - first_csn + 1 != expected_observations
        or evidence["distinct_commit_csns"] != expected_observations
        or isinstance(evidence["distinct_commit_csns"], bool)
        or evidence["commit_receipt_digest_algorithm"]
        != "blake3-csn-ordered-native-commit-receipts-v1"
        or not isinstance(evidence["commit_receipt_digest"], str)
        or HEX64.fullmatch(evidence["commit_receipt_digest"]) is None
    ):
        raise GateFailure("G7 strict group-commit commit receipt evidence is invalid")


def _validate_group_commit_synchronization(evidence: dict[str, Any]) -> None:
    cohort_count = evidence["cohort_count"]
    execution_nanos = evidence["cohort_execution_nanos_total"]
    page_nanos = evidence["page_synchronization_nanos_total"]
    wal_nanos = evidence["wal_synchronization_nanos_total"]
    if (
        evidence["page_synchronizations"] != cohort_count
        or isinstance(evidence["page_synchronizations"], bool)
        or evidence["wal_synchronizations"] != cohort_count
        or isinstance(evidence["wal_synchronizations"], bool)
        or any(
            not isinstance(value, int) or isinstance(value, bool) or value <= 0
            for value in (execution_nanos, page_nanos, wal_nanos)
        )
        or execution_nanos < page_nanos + wal_nanos
    ):
        raise GateFailure("G7 strict group-commit synchronization evidence is invalid")


def _validate_group_commit_timings(
    cell: dict[str, Any],
    evidence: dict[str, Any],
    expected_observations: int,
) -> None:
    timings = evidence["timings_nanoseconds"]
    components = {
        "admission_wait", "queue_wait", "cohort_execution",
        "page_synchronization", "wal_synchronization", "end_to_end",
    }
    if (
        evidence["timing_sample_count"] != expected_observations
        or isinstance(evidence["timing_sample_count"], bool)
        or not isinstance(timings, dict)
        or set(timings) != components
    ):
        raise GateFailure("G7 strict group-commit timing evidence is invalid")
    for component in components:
        _validate_group_commit_latency_summary(timings[component])
    end_to_end = timings["end_to_end"]
    if any(end_to_end[field] != cell.get(field) for field in end_to_end):
        raise GateFailure("G7 strict group-commit timing differs from end-to-end latency")


def _validate_group_commit_latency_summary(value: Any) -> None:
    fields = ("p50", "p95", "p99", "p999", "maximum")
    if (
        not isinstance(value, dict)
        or set(value) != set(fields)
        or any(
            not isinstance(value[field], int)
            or isinstance(value[field], bool)
            or value[field] < 0
            for field in fields
        )
        or any(value[left] > value[right] for left, right in zip(fields, fields[1:]))
    ):
        raise GateFailure("G7 strict group-commit timing summary is invalid")


def _validate_group_commit_reopen(
    evidence: dict[str, Any],
    expected_observations: int,
) -> None:
    reopen = evidence["reopen"]
    fields = {
        "provider", "baseline_visible_csn", "baseline_committed_transactions",
        "reopened_visible_csn", "reopened_committed_transactions",
        "verified_logical_commits", "missing_keys", "mismatched_values",
        "state_digest_algorithm", "expected_state_digest", "recovered_state_digest",
        "open_time_nanos", "verification_time_nanos",
    }
    if not isinstance(reopen, dict) or set(reopen) != fields:
        raise GateFailure("G7 strict group-commit reopen evidence fields mismatch")
    baseline_visible = reopen["baseline_visible_csn"]
    baseline_committed = reopen["baseline_committed_transactions"]
    reopened_visible = reopen["reopened_visible_csn"]
    reopened_committed = reopen["reopened_committed_transactions"]
    if (
        reopen["provider"] != "single-reopened-root-snapshot-full-key-digest-v1"
        or any(
            not isinstance(value, int) or isinstance(value, bool) or value < 0
            for value in (
                baseline_visible,
                baseline_committed,
                reopened_visible,
                reopened_committed,
                reopen["verified_logical_commits"],
                reopen["missing_keys"],
                reopen["mismatched_values"],
            )
        )
        or reopened_visible - baseline_visible != expected_observations
        or reopened_committed - baseline_committed != expected_observations
        or evidence["first_commit_csn"] != baseline_visible + 1
        or evidence["last_commit_csn"] != reopened_visible
        or reopen["verified_logical_commits"] != expected_observations
        or reopen["missing_keys"] != 0
        or reopen["mismatched_values"] != 0
        or reopen["state_digest_algorithm"] != "blake3-logical-id-key-value-v1"
        or not isinstance(reopen["expected_state_digest"], str)
        or HEX64.fullmatch(reopen["expected_state_digest"]) is None
        or reopen["recovered_state_digest"] != reopen["expected_state_digest"]
        or not isinstance(reopen["recovered_state_digest"], str)
        or any(
            not isinstance(reopen[field], int)
            or isinstance(reopen[field], bool)
            or reopen[field] <= 0
            for field in ("open_time_nanos", "verification_time_nanos")
        )
    ):
        raise GateFailure("G7 strict group-commit reopen evidence is invalid")


def validate_ann_read_view_cell(
    cell: Any,
    initial_bulk: dict[str, Any],
    expected_observations: int,
) -> None:
    if not isinstance(cell, dict):
        raise GateFailure("G7 ANN cell is not an evidence object")
    worker_limit = cell.get("per_query_worker_limit")
    if (
        not isinstance(worker_limit, int)
        or isinstance(worker_limit, bool)
        or worker_limit <= 0
        or worker_limit
        > min(G7_LOGICAL_ANN_PARTITIONS, initial_bulk["topology_workers"])
    ):
        raise GateFailure("G7 ANN cell omitted its governed per-query worker limit")
    queue_wait = cell.get("query_queue_wait_millis")
    if (
        not isinstance(queue_wait, int)
        or isinstance(queue_wait, bool)
        or queue_wait <= 0
    ):
        raise GateFailure("G7 ANN cell omitted its bounded queue wait")
    view = cell.get("ann_read_view_open")
    fields = {
        "root_identity", "snapshot_csn", "base_build_identity", "view_identity",
        "routing_policy_identity", "logical_partitions", "planned_physical_entries",
        "planned_physical_bytes", "observed_physical_entries", "observed_physical_bytes",
        "planned_peak_memory_bytes", "retained_memory_bytes", "hydration_restore_count",
        "process_physical_page_read_delta", "governor_generation",
    }
    if not isinstance(view, dict) or set(view) != fields:
        raise GateFailure("G7 ANN cell omitted its durable read-view open receipt")
    for name in (
        "root_identity", "base_build_identity", "view_identity", "routing_policy_identity",
    ):
        if not isinstance(view[name], str) or HEX64.fullmatch(view[name]) is None:
            raise GateFailure(f"G7 ANN read-view identity is invalid: {name}")
    for name in (
        "logical_partitions", "planned_physical_entries", "planned_physical_bytes",
        "observed_physical_entries", "observed_physical_bytes",
        "planned_peak_memory_bytes", "retained_memory_bytes", "governor_generation",
        "snapshot_csn",
    ):
        value = view[name]
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise GateFailure(f"G7 ANN read-view resource is invalid: {name}")
    for name in ("hydration_restore_count", "process_physical_page_read_delta"):
        value = view[name]
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise GateFailure(f"G7 ANN read-view open counter is invalid: {name}")
    if (
        view["logical_partitions"] != initial_bulk["planned_partitions"]
        or view["hydration_restore_count"] != 1
        or view["observed_physical_entries"] > view["planned_physical_entries"]
        or view["observed_physical_bytes"] > view["planned_physical_bytes"]
        or view["retained_memory_bytes"] > view["planned_peak_memory_bytes"]
    ):
        raise GateFailure("G7 ANN read-view open receipt contradicts its durable plan")
    if cell.get("ann_read_view_query_interval") != {
        "physical_page_reads": 0,
        "index_scoped_restores": 0,
        "provider": "database-page-counter-plus-process-ann-restore-counter",
    }:
        raise GateFailure("G7 ANN read view crossed its hydration boundary")
    if (
        cell.get("post_open_hydration_performed") is not False
        or cell.get("post_open_physical_page_reads") != 0
        or cell.get("post_open_restore_count") != 0
    ):
        raise GateFailure("G7 ANN cell performed storage or restore work after read-view open")
    routing = cell.get("ann_routing_interval")
    if (
        cell.get("preferred_partition_budget") != G7_PREFERRED_ANN_PARTITIONS
    ):
        raise GateFailure("G7 ANN routing interval was not selected-certified")
    _validate_selected_routing_interval(
        routing,
        expected_observations=expected_observations,
        worker_limit=worker_limit,
        label="ANN",
    )


def validate_bm25_read_view_cell(
    cell: Any,
    hybrid_cell: Any,
    expected_observations: int,
) -> None:
    """Validate one prepared BM25 interval against its shared lexical authority."""
    if not isinstance(cell, dict) or not isinstance(hybrid_cell, dict):
        raise GateFailure("G7 BM25 cell is not an evidence object")
    if cell.get("route") != "native-retained-lexical-read-view":
        raise GateFailure("G7 BM25 did not use its retained lexical read-view route")
    view = cell.get("lexical_read_view_open")
    hybrid_view = hybrid_cell.get("hybrid_read_view_open")
    fields = {
        "root_identity", "snapshot_csn", "lexical_index_identity_algorithm",
        "lexical_index_identity", "lexical_plan_scope", "index_id",
        "planned_terms", "retained_postings", "maximum_retained_postings",
        "maximum_retained_bytes", "planned_physical_entries",
        "planned_physical_bytes", "observed_physical_entries",
        "observed_physical_bytes", "admitted_retained_memory_bytes",
        "retained_memory_bytes", "open_physical_page_reads",
    }
    if (
        not isinstance(view, dict)
        or set(view) != fields
        or not isinstance(hybrid_view, dict)
    ):
        raise GateFailure("G7 BM25 read-view open receipt is incomplete")
    for name in ("root_identity", "lexical_index_identity"):
        if not isinstance(view[name], str) or HEX64.fullmatch(view[name]) is None:
            raise GateFailure(f"G7 BM25 read-view identity is invalid: {name}")
    if (
        not isinstance(view["index_id"], str)
        or HEX32.fullmatch(view["index_id"]) is None
        or int(view["index_id"], 16) == 0
    ):
        raise GateFailure("G7 BM25 read-view index identity is invalid")
    positive_fields = (
        "snapshot_csn", "planned_terms", "retained_postings",
        "maximum_retained_postings", "maximum_retained_bytes",
        "planned_physical_entries", "planned_physical_bytes",
        "observed_physical_entries", "observed_physical_bytes",
        "admitted_retained_memory_bytes", "retained_memory_bytes",
    )
    if any(
        not isinstance(view[name], int)
        or isinstance(view[name], bool)
        or view[name] <= 0
        for name in positive_fields
    ) or (
        not isinstance(view["open_physical_page_reads"], int)
        or isinstance(view["open_physical_page_reads"], bool)
        or view["open_physical_page_reads"] < 0
    ):
        raise GateFailure("G7 BM25 read-view resource evidence is invalid")
    if (
        view["lexical_index_identity_algorithm"]
        != "blake3-search-root-page-object-format-v1"
        or view["lexical_plan_scope"] != "query-bound-encoded-postings-v1"
        or view["planned_terms"] != 1
        or view["retained_postings"] != 1
        or view["maximum_retained_postings"] != 10
        or view["maximum_retained_bytes"] != 1_048_576
        or view["observed_physical_entries"] > view["planned_physical_entries"]
        or view["observed_physical_bytes"] > view["planned_physical_bytes"]
        or view["admitted_retained_memory_bytes"] > view["maximum_retained_bytes"]
        or view["retained_memory_bytes"] > view["admitted_retained_memory_bytes"]
    ):
        raise GateFailure("G7 BM25 read-view plan or retention boundary is invalid")
    shared_fields = (
        "root_identity", "snapshot_csn", "lexical_index_identity",
        "lexical_plan_scope", "planned_physical_entries", "planned_physical_bytes",
        "observed_physical_entries", "observed_physical_bytes",
        "admitted_retained_memory_bytes", "retained_memory_bytes",
    )
    if any(view[name] != hybrid_view.get(name) for name in shared_fields):
        raise GateFailure("G7 BM25 read-view differs from the hybrid lexical authority")

    interval = cell.get("lexical_read_view_query_interval")
    interval_fields = {
        "observations", "postings_evaluated", "execution_sequence_first",
        "execution_sequence_last", "receipt_physical_page_reads",
        "process_physical_page_reads", "full_state_loads", "full_catalog_loads",
        "lexical_execution", "provider",
    }
    if not isinstance(interval, dict) or set(interval) != interval_fields:
        raise GateFailure("G7 BM25 read-view interval fields mismatch")
    integer_fields = interval_fields - {"lexical_execution", "provider"}
    if any(
        not isinstance(interval[name], int) or isinstance(interval[name], bool)
        for name in integer_fields
    ):
        raise GateFailure("G7 BM25 read-view interval contains invalid counters")
    first = interval["execution_sequence_first"]
    last = interval["execution_sequence_last"]
    if (
        interval["observations"] != expected_observations
        or interval["postings_evaluated"] != expected_observations
        or first <= 0
        or last < first
        or last - first + 1 != expected_observations
        or interval["receipt_physical_page_reads"] != 0
        or interval["process_physical_page_reads"] != 0
        or interval["full_state_loads"] != 0
        or interval["full_catalog_loads"] != 0
        or interval["lexical_execution"]
        != "decode-bm25-rank-per-observation-v1"
        or interval["provider"] != "lexical-read-view-interval-counters-v1"
    ):
        raise GateFailure("G7 BM25 read-view interval crossed or skipped its authority")


def validate_filtered_bm25_read_view_cell(
    cell: Any,
    hybrid_cell: Any,
    expected_observations: int,
) -> None:
    """Validate root-bound predicate evaluation before BM25 ranking."""
    if not isinstance(cell, dict) or not isinstance(hybrid_cell, dict):
        raise GateFailure("G7 filtered BM25 cell is not an evidence object")
    if (
        cell.get("route") != "native-root-bound-filter-before-rank"
        or cell.get("correctness_scope")
        != "lexical-and-structure-one-root-query-bound"
        or not isinstance(cell.get("corpus_filter_density"), float)
        or not math.isfinite(cell["corpus_filter_density"])
        or cell["corpus_filter_density"] != 0.5
        or not isinstance(cell.get("candidate_filter_selectivity"), float)
        or not math.isfinite(cell["candidate_filter_selectivity"])
        or cell["candidate_filter_selectivity"] != 1.0
    ):
        raise GateFailure("G7 filtered BM25 route or selectivity authority is invalid")

    view = cell.get("filtered_lexical_read_view_open")
    hybrid_view = hybrid_cell.get("hybrid_read_view_open")
    fields = {
        "root_identity", "snapshot_csn", "lexical_index_identity",
        "lexical_plan_scope", "structure_filter_identity_algorithm",
        "structure_filter_value_scope", "structure_filter_identity",
        "retained_filter_records", "planned_filter_physical_entries",
        "planned_filter_physical_bytes", "observed_filter_physical_entries",
        "observed_filter_physical_bytes", "retained_filter_memory_bytes",
        "filter_planning", "filter_hydration", "open_filter_physical_page_reads",
    }
    if (
        not isinstance(view, dict)
        or set(view) != fields
        or not isinstance(hybrid_view, dict)
    ):
        raise GateFailure("G7 filtered BM25 read-view open receipt is incomplete")
    for name in (
        "root_identity", "lexical_index_identity", "structure_filter_identity",
    ):
        if (
            not isinstance(view[name], str)
            or HEX64.fullmatch(view[name]) is None
            or (name == "structure_filter_identity" and int(view[name], 16) == 0)
        ):
            raise GateFailure(f"G7 filtered BM25 identity is invalid: {name}")
    positive_fields = (
        "snapshot_csn", "retained_filter_records",
        "planned_filter_physical_entries", "planned_filter_physical_bytes",
        "observed_filter_physical_entries", "observed_filter_physical_bytes",
        "retained_filter_memory_bytes",
    )
    if any(
        not isinstance(view[name], int)
        or isinstance(view[name], bool)
        or view[name] <= 0
        for name in positive_fields
    ) or (
        not isinstance(view["open_filter_physical_page_reads"], int)
        or isinstance(view["open_filter_physical_page_reads"], bool)
        or view["open_filter_physical_page_reads"] < 0
    ):
        raise GateFailure("G7 filtered BM25 read-view open resources are invalid")
    if (
        view["root_identity"] != hybrid_view.get("root_identity")
        or view["snapshot_csn"] != hybrid_view.get("snapshot_csn")
        or view["lexical_index_identity"]
        != hybrid_view.get("lexical_index_identity")
        or view["lexical_plan_scope"] != "query-bound-encoded-postings-v1"
        or view["lexical_plan_scope"] != hybrid_view.get("lexical_plan_scope")
        or view["structure_filter_identity_algorithm"]
        != "blake3-structure-root-key-prefix-value-time-v1"
        or view["structure_filter_value_scope"] != "inline-scalar-only-v1"
    ):
        raise GateFailure("G7 filtered BM25 same-root authority is invalid")
    if (
        view["retained_filter_records"] != 1
        or view["planned_filter_physical_entries"] != 1
        or view["observed_filter_physical_entries"] != 1
        or view["observed_filter_physical_bytes"]
        > view["planned_filter_physical_bytes"]
    ):
        raise GateFailure("G7 filtered BM25 read-view open plan is invalid")
    planning_memory = _validate_filtered_bm25_admission(
        view["filter_planning"], "planning"
    )
    hydration_memory = _validate_filtered_bm25_admission(
        view["filter_hydration"], "hydration"
    )
    if (
        planning_memory <= 0
        or view["retained_filter_memory_bytes"] > hydration_memory
    ):
        raise GateFailure("G7 filtered BM25 read-view admission is insufficient")

    interval = cell.get("filtered_lexical_read_view_query_interval")
    fields = {
        "observations", "execution_sequence_first", "execution_sequence_last",
        "postings_scored", "filter_records_evaluated", "filter_records_matched",
        "receipt_physical_page_reads",
        "process_physical_page_reads", "full_state_loads", "full_catalog_loads",
        "filter_execution", "provider",
    }
    if not isinstance(interval, dict) or set(interval) != fields:
        raise GateFailure("G7 filtered BM25 read-view interval fields mismatch")
    numeric_fields = fields - {"filter_execution", "provider"}
    first = interval.get("execution_sequence_first")
    last = interval.get("execution_sequence_last")
    if any(
        not isinstance(interval[name], int) or isinstance(interval[name], bool)
        for name in numeric_fields
    ) or (
        interval["observations"] != expected_observations
        or first <= 0
        or last < first
        or last - first + 1 != expected_observations
        or interval["postings_scored"] != expected_observations
        or interval["filter_records_evaluated"] != expected_observations
        or interval["filter_records_matched"] != expected_observations
        or interval["receipt_physical_page_reads"] != 0
        or interval["process_physical_page_reads"] != 0
        or interval["full_state_loads"] != 0
        or interval["full_catalog_loads"] != 0
        or interval["filter_execution"]
        != "decode-expiry-inline-value-filter-before-rank-v1"
        or interval["provider"]
        != "filtered-lexical-read-view-interval-counters-v1"
    ):
        raise GateFailure("G7 filtered BM25 read-view interval changed its predicate")


def _validate_filtered_bm25_admission(value: Any, phase: str) -> int:
    fields = {
        "class", "compute_threads", "io_slots", "memory_bytes", "queue_ticket",
        "initial_queue_depth", "queue_time_nanos", "execution_time_nanos",
    }
    if not isinstance(value, dict) or set(value) != fields:
        raise GateFailure(f"G7 filtered BM25 {phase} admission fields mismatch")
    queue_ticket = value["queue_ticket"]
    nonnegative_fields = (
        "initial_queue_depth", "queue_time_nanos", "execution_time_nanos",
    )
    if (
        value["class"] != "foreground-bounded"
        or value["compute_threads"] != 1
        or isinstance(value["compute_threads"], bool)
        or value["io_slots"] != 1
        or isinstance(value["io_slots"], bool)
        or not isinstance(value["memory_bytes"], int)
        or isinstance(value["memory_bytes"], bool)
        or value["memory_bytes"] <= 0
        or any(
            not isinstance(value[name], int)
            or isinstance(value[name], bool)
            or value[name] < 0
            for name in nonnegative_fields
        )
        or value["execution_time_nanos"] == 0
        or (
            queue_ticket is not None
            and (
                not isinstance(queue_ticket, int)
                or isinstance(queue_ticket, bool)
                or queue_ticket < 0
            )
        )
    ):
        raise GateFailure(f"G7 filtered BM25 {phase} admission is invalid")
    return value["memory_bytes"]


def _validate_selected_routing_interval(
    routing: Any,
    *,
    expected_observations: int,
    worker_limit: int,
    label: str,
) -> None:
    fields = {
        "observations", "execution_workers_max", "execution_worker_batches_max",
        "execution_waves_max", "selected_certified", "full_fanout_requested",
        "full_fanout_budget_fallback", "single_generation_fallback",
        "next_partition_lower_bound_present", "selected_partitions_max",
        "minimum_next_partition_lower_bound", "maximum_kth_distance",
    }
    if not isinstance(routing, dict) or set(routing) != fields:
        raise GateFailure(f"G7 {label} routing interval fields mismatch")
    observations = routing["observations"]
    integer_fields = (
        "execution_workers_max", "execution_worker_batches_max", "execution_waves_max",
        "selected_certified", "full_fanout_requested", "full_fanout_budget_fallback",
        "single_generation_fallback", "next_partition_lower_bound_present",
        "selected_partitions_max",
    )
    if (
        not isinstance(observations, int)
        or isinstance(observations, bool)
        or observations != expected_observations
        or any(
            not isinstance(routing[field], int) or isinstance(routing[field], bool)
            for field in integer_fields
        )
        or routing["selected_certified"] != observations
        or routing["full_fanout_requested"] != 0
        or routing["full_fanout_budget_fallback"] != 0
        or routing["single_generation_fallback"] != 0
        or routing["next_partition_lower_bound_present"] != observations
        or not 1 <= routing["selected_partitions_max"] <= G7_PREFERRED_ANN_PARTITIONS
        or not 1 <= routing["execution_workers_max"] <= worker_limit
        or not 1
        <= routing["execution_worker_batches_max"]
        <= G7_PREFERRED_ANN_PARTITIONS
        or not 1 <= routing["execution_waves_max"] <= 6
    ):
        raise GateFailure(f"G7 {label} routing interval was not selected-certified")
    lower_bound = routing["minimum_next_partition_lower_bound"]
    kth_distance = routing["maximum_kth_distance"]
    if (
        not isinstance(lower_bound, float)
        or not math.isfinite(lower_bound)
        or not isinstance(kth_distance, float)
        or not math.isfinite(kth_distance)
        or lower_bound <= kth_distance
    ):
        raise GateFailure(f"G7 {label} routing omission bound is not strict and finite")


def validate_hybrid_read_view_cell(
    cell: Any,
    ann_cell: Any,
    expected_observations: int,
) -> None:
    """Validate one G7 hybrid cell against its shared ANN/root authority."""
    if not isinstance(cell, dict) or not isinstance(ann_cell, dict):
        raise GateFailure("G7 hybrid cell is not an evidence object")
    worker_limit = cell.get("per_query_worker_limit")
    if (
        not isinstance(worker_limit, int)
        or isinstance(worker_limit, bool)
        or worker_limit <= 0
        or worker_limit != ann_cell.get("per_query_worker_limit")
        or cell.get("preferred_partition_budget") != G7_PREFERRED_ANN_PARTITIONS
    ):
        raise GateFailure("G7 hybrid routing authority is invalid")
    queue_wait = cell.get("query_queue_wait_millis")
    if (
        not isinstance(queue_wait, int)
        or isinstance(queue_wait, bool)
        or queue_wait <= 0
        or queue_wait != ann_cell.get("query_queue_wait_millis")
    ):
        raise GateFailure("G7 hybrid query queue wait differs from ANN authority")
    ann_open = ann_cell.get("ann_read_view_open")
    view = cell.get("hybrid_read_view_open")
    fields = {
        "root_identity", "snapshot_csn", "lexical_index_identity", "ann_view_identity",
        "lexical_plan_scope", "planned_physical_entries", "planned_physical_bytes",
        "observed_physical_entries", "observed_physical_bytes",
        "admitted_retained_memory_bytes", "retained_memory_bytes",
    }
    if (
        not isinstance(ann_open, dict)
        or not isinstance(view, dict)
        or set(view) != fields
    ):
        raise GateFailure("G7 hybrid read-view open receipt is incomplete")
    for name in ("root_identity", "lexical_index_identity", "ann_view_identity"):
        if not isinstance(view[name], str) or HEX64.fullmatch(view[name]) is None:
            raise GateFailure(f"G7 hybrid read-view identity is invalid: {name}")
    for name in (
        "snapshot_csn", "planned_physical_entries", "planned_physical_bytes",
        "observed_physical_entries", "observed_physical_bytes",
        "admitted_retained_memory_bytes", "retained_memory_bytes",
    ):
        value = view[name]
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise GateFailure(f"G7 hybrid read-view resource is invalid: {name}")
    if (
        view["root_identity"] != ann_open.get("root_identity")
        or view["snapshot_csn"] != ann_open.get("snapshot_csn")
        or view["ann_view_identity"] != ann_open.get("view_identity")
        or view["lexical_plan_scope"] != "query-bound-encoded-postings-v1"
    ):
        raise GateFailure("G7 hybrid read-view authority differs from ANN root authority")
    if (
        view["observed_physical_entries"] > view["planned_physical_entries"]
        or view["observed_physical_bytes"] > view["planned_physical_bytes"]
    ):
        raise GateFailure("G7 hybrid read-view open receipt exceeds its physical plan")
    if view["retained_memory_bytes"] > view["admitted_retained_memory_bytes"]:
        raise GateFailure("G7 hybrid read-view retained memory exceeds its admission")
    interval = cell.get("hybrid_read_view_query_interval")
    interval_fields = {
        "observations", "hydrations", "physical_page_reads",
        "index_scoped_restores", "full_state_loads", "full_catalog_loads",
        "lexical_execution", "peak_admission_executions",
        "peak_admission_class", "peak_admission_compute_threads",
        "peak_admission_io_slots", "peak_admission_memory_bytes_min",
        "peak_admission_memory_bytes_max", "result_retention_executions",
        "result_retention_class", "result_retention_compute_threads",
        "result_retention_io_slots", "result_retention_memory_bytes_min",
        "result_retention_memory_bytes_max", "fusion_executions", "fusion_class",
        "fusion_compute_threads", "fusion_io_slots", "fusion_memory_bytes",
        "provider",
    }
    if not isinstance(interval, dict) or set(interval) != interval_fields:
        raise GateFailure("G7 hybrid query interval fields mismatch")
    storage_fields = (
        "observations", "hydrations", "physical_page_reads",
        "index_scoped_restores", "full_state_loads", "full_catalog_loads",
    )
    if (
        any(
            not isinstance(interval[name], int) or isinstance(interval[name], bool)
            for name in storage_fields
        )
        or interval["observations"] != expected_observations
        or any(interval[name] != 0 for name in storage_fields[1:])
        or interval["lexical_execution"]
        != "decode-bm25-rank-per-observation-v1"
        or interval["provider"] != "hybrid-read-view-interval-counters-v1"
    ):
        raise GateFailure("G7 hybrid query interval crossed storage or materialization boundaries")
    _validate_hybrid_peak_admission(
        interval,
        expected_observations=expected_observations,
        worker_limit=worker_limit,
    )
    _validate_hybrid_result_retention(
        interval,
        expected_observations=expected_observations,
    )
    _validate_hybrid_fusion(
        interval,
        expected_observations=expected_observations,
    )
    _validate_selected_routing_interval(
        cell.get("hybrid_ann_routing_interval"),
        expected_observations=expected_observations,
        worker_limit=worker_limit,
        label="hybrid",
    )
    _validate_hybrid_oracle(cell.get("hybrid_oracle"), view)


def _validate_hybrid_peak_admission(
    interval: dict[str, Any],
    *,
    expected_observations: int,
    worker_limit: int,
) -> None:
    integers = (
        "peak_admission_executions", "peak_admission_compute_threads",
        "peak_admission_io_slots", "peak_admission_memory_bytes_min",
        "peak_admission_memory_bytes_max",
    )
    minimum = interval["peak_admission_memory_bytes_min"]
    maximum = interval["peak_admission_memory_bytes_max"]
    if (
        any(
            not isinstance(interval[name], int) or isinstance(interval[name], bool)
            for name in integers
        )
        or interval["peak_admission_executions"] != expected_observations
        or interval["peak_admission_class"] != "foreground-bounded"
        or interval["peak_admission_compute_threads"] != worker_limit
        or interval["peak_admission_io_slots"] != 0
        or minimum <= 0
        or maximum < minimum
    ):
        raise GateFailure("G7 hybrid peak admission is invalid")


def _validate_hybrid_result_retention(
    interval: dict[str, Any],
    *,
    expected_observations: int,
) -> None:
    integers = (
        "result_retention_executions", "result_retention_compute_threads",
        "result_retention_io_slots", "result_retention_memory_bytes_min",
        "result_retention_memory_bytes_max",
    )
    minimum = interval["result_retention_memory_bytes_min"]
    maximum = interval["result_retention_memory_bytes_max"]
    if (
        any(
            not isinstance(interval[name], int) or isinstance(interval[name], bool)
            for name in integers
        )
        or interval["result_retention_executions"] != expected_observations
        or interval["result_retention_class"] != "foreground-bounded"
        or interval["result_retention_compute_threads"] != 0
        or interval["result_retention_io_slots"] != 0
        or minimum <= 0
        or maximum < minimum
    ):
        raise GateFailure("G7 hybrid result retention is invalid")
    if interval["peak_admission_memory_bytes_min"] < maximum:
        raise GateFailure("G7 hybrid peak admission is below result retention")


def _validate_hybrid_fusion(
    interval: dict[str, Any],
    *,
    expected_observations: int,
) -> None:
    integers = (
        "fusion_executions", "fusion_compute_threads", "fusion_io_slots",
        "fusion_memory_bytes",
    )
    if (
        any(
            not isinstance(interval[name], int) or isinstance(interval[name], bool)
            for name in integers
        )
        or interval["fusion_executions"] != expected_observations
        or interval["fusion_class"] != "foreground-bounded"
        or interval["fusion_compute_threads"] != 1
        or interval["fusion_io_slots"] != 0
        or interval["fusion_memory_bytes"] != 0
    ):
        raise GateFailure("G7 hybrid fusion admission is invalid")


def _validate_hybrid_oracle(oracle: Any, view: dict[str, Any]) -> None:
    fields = {
        "status", "method", "root_identity", "snapshot_csn", "rrf_constant",
        "contribution_scale", "lexical_weight", "vector_weight", "result_limit",
        "tie_break", "lexical_ranking", "vector_ranking", "fused_results",
        "result_digest", "oracle_digest",
    }
    if not isinstance(oracle, dict) or set(oracle) != fields:
        raise GateFailure("G7 hybrid oracle fields mismatch")
    integer_fields = (
        "snapshot_csn", "rrf_constant", "contribution_scale", "lexical_weight",
        "vector_weight", "result_limit",
    )
    if (
        any(
            not isinstance(oracle[field], int) or isinstance(oracle[field], bool)
            for field in integer_fields
        )
        or oracle["status"] != "passed"
        or oracle["method"] != "independent-branch-rrf-v1"
        or oracle["root_identity"] != view["root_identity"]
        or oracle["snapshot_csn"] != view["snapshot_csn"]
        or oracle["rrf_constant"] != 60
        or oracle["contribution_scale"] != 1_000_000_000
        or oracle["lexical_weight"] != 1
        or oracle["vector_weight"] != 1
        or oracle["result_limit"] != 10
        or oracle["tie_break"] != "fusion-score-desc-object-id-asc"
    ):
        raise GateFailure("G7 hybrid oracle authority is invalid")
    lexical = _validate_hybrid_ranking(oracle["lexical_ranking"], "lexical", 10)
    vector = _validate_hybrid_ranking(oracle["vector_ranking"], "vector", 10)
    if len(vector) != 10:
        raise GateFailure("G7 hybrid oracle vector ranking is incomplete")
    expected = _fuse_hybrid_oracle(lexical, vector)
    _validate_hybrid_results(oracle["fused_results"])
    if oracle["fused_results"] != expected:
        raise GateFailure("G7 hybrid oracle result or explanation mismatch")
    canonical = json.dumps(
        expected,
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("ascii")
    digest = hashlib.sha256(canonical).hexdigest()
    if (
        not isinstance(oracle["result_digest"], str)
        or HEX64.fullmatch(oracle["result_digest"]) is None
        or not isinstance(oracle["oracle_digest"], str)
        or HEX64.fullmatch(oracle["oracle_digest"]) is None
        or oracle["result_digest"] != digest
        or oracle["oracle_digest"] != digest
    ):
        raise GateFailure("G7 hybrid oracle digest mismatch")


def _validate_hybrid_results(value: Any) -> None:
    fields = {
        "object_id", "lexical_rank", "vector_rank", "lexical_contribution",
        "vector_contribution", "fusion_score", "final_rank",
    }
    if not isinstance(value, list) or len(value) != 10:
        raise GateFailure("G7 hybrid oracle result or explanation mismatch")
    for result in value:
        if not isinstance(result, dict) or set(result) != fields:
            raise GateFailure("G7 hybrid oracle result or explanation mismatch")
        if (
            not isinstance(result["object_id"], str)
            or HEX32.fullmatch(result["object_id"]) is None
            or int(result["object_id"], 16) == 0
        ):
            raise GateFailure("G7 hybrid oracle result or explanation mismatch")
        for name in ("lexical_rank", "vector_rank"):
            rank = result[name]
            if rank is not None and (
                not isinstance(rank, int) or isinstance(rank, bool) or rank <= 0
            ):
                raise GateFailure("G7 hybrid oracle result or explanation mismatch")
        for name in (
            "lexical_contribution", "vector_contribution", "fusion_score", "final_rank",
        ):
            number = result[name]
            if not isinstance(number, int) or isinstance(number, bool) or number < 0:
                raise GateFailure("G7 hybrid oracle result or explanation mismatch")


def _validate_hybrid_ranking(value: Any, label: str, limit: int) -> list[str]:
    if (
        not isinstance(value, list)
        or not value
        or len(value) > limit
        or len(value) != len(set(value))
        or any(
            not isinstance(object_id, str)
            or HEX32.fullmatch(object_id) is None
            or int(object_id, 16) == 0
            for object_id in value
        )
    ):
        raise GateFailure(f"G7 hybrid oracle {label} ranking is invalid")
    return value


def _fuse_hybrid_oracle(lexical: list[str], vector: list[str]) -> list[dict[str, Any]]:
    lexical_ranks = {object_id: rank for rank, object_id in enumerate(lexical, start=1)}
    vector_ranks = {object_id: rank for rank, object_id in enumerate(vector, start=1)}
    results = []
    for object_id in set(lexical) | set(vector):
        lexical_rank = lexical_ranks.get(object_id)
        vector_rank = vector_ranks.get(object_id)
        lexical_contribution = (
            1_000_000_000 // (60 + lexical_rank) if lexical_rank is not None else 0
        )
        vector_contribution = (
            1_000_000_000 // (60 + vector_rank) if vector_rank is not None else 0
        )
        results.append({
            "object_id": object_id,
            "lexical_rank": lexical_rank,
            "vector_rank": vector_rank,
            "lexical_contribution": lexical_contribution,
            "vector_contribution": vector_contribution,
            "fusion_score": lexical_contribution + vector_contribution,
            "final_rank": 0,
        })
    results.sort(key=lambda result: (-result["fusion_score"], result["object_id"]))
    del results[10:]
    for final_rank, result in enumerate(results, start=1):
        result["final_rank"] = final_rank
    return results


def validate_execution_authority_evidence(
    evidence: Any,
    initial_ann_bulk: Any,
    background_mode: str,
) -> None:
    """Validate the single calibrated execution authority used by one G7 cell."""
    fields = {
        "status", "topology_digest", "runner_executable_blake3",
        "calibration_executable_blake3", "installations", "installed_surfaces",
        "registered_pools", "local_dispatches", "stolen_dispatches",
        "completed_jobs", "numa_steal_status",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise GateFailure("G7 execution authority evidence fields mismatch")
    if not isinstance(initial_ann_bulk, dict):
        raise GateFailure("G7 execution authority has no ANN topology authority")
    runner = evidence["runner_executable_blake3"]
    calibration = evidence["calibration_executable_blake3"]
    if (
        evidence["status"] != "measured"
        or not isinstance(evidence["topology_digest"], str)
        or HEX64.fullmatch(evidence["topology_digest"]) is None
        or evidence["topology_digest"] != initial_ann_bulk.get("topology_digest")
        or not isinstance(runner, str)
        or HEX64.fullmatch(runner) is None
        or not isinstance(calibration, str)
        or HEX64.fullmatch(calibration) is None
        or runner != calibration
    ):
        raise GateFailure("G7 execution authority identity differs from calibration")
    surfaces = evidence["installed_surfaces"]
    required_surfaces = {
        "search-fixture", "embedded-structure", "embedded-sql",
        "local-structure-seed", "local-structure-migration",
        "local-structure-daemon", "local-sql-daemon", "indexed-sql",
        "join-sql", "group-commit", "physical-observation",
    }
    if background_mode == "interference":
        required_surfaces.add("background-maintenance")
    allowed_surfaces = required_surfaces | {"search-seed-builder"}
    if (
        not isinstance(surfaces, list)
        or any(not isinstance(surface, str) or not surface for surface in surfaces)
        or surfaces != sorted(set(surfaces))
        or not required_surfaces.issubset(surfaces)
        or not set(surfaces).issubset(allowed_surfaces)
        or not isinstance(evidence["installations"], int)
        or isinstance(evidence["installations"], bool)
        or evidence["installations"] != len(surfaces)
        or not isinstance(evidence["registered_pools"], int)
        or isinstance(evidence["registered_pools"], bool)
        or evidence["registered_pools"] != 1
    ):
        raise GateFailure("G7 execution authority surfaces or pools differ")
    counters = (
        evidence["local_dispatches"],
        evidence["stolen_dispatches"],
        evidence["completed_jobs"],
    )
    if (
        any(
            not isinstance(value, int) or isinstance(value, bool) or value < 0
            for value in counters
        )
        or evidence["completed_jobs"]
        != evidence["local_dispatches"] + evidence["stolen_dispatches"]
    ):
        raise GateFailure("G7 execution authority counters do not reconcile")
    if (
        evidence["numa_steal_status"]
        not in {"calibrated", "disabled", "not-applicable"}
        or (
            evidence["numa_steal_status"] != "calibrated"
            and evidence["stolen_dispatches"] != 0
        )
    ):
        raise GateFailure("G7 execution authority NUMA evidence differs")


def validate_initial_ann_bulk(evidence: Any, receipt: dict[str, Any]) -> None:
    fields = {
        "schema", "source_commit", "dataset_digest", "builder", "partition_policy", "input_identity",
        "aggregate_identity", "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "total_time_nanos",
        "hardware_profile_fingerprint", "governor_policy_schema", "governor_mode",
        "calibration_cache_key", "topology_digest", "topology_workers", "hard_affinity",
        "governor_execution",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise GateFailure("G7 initial ANN bulk evidence fields mismatch")
    if (
        evidence["schema"] != "hyphae-native-g7-initial-ann-bulk-v1"
        or evidence["source_commit"] != receipt["source_commit"]
        or evidence["dataset_digest"] != receipt["dataset"]["digest"]
        or evidence["builder"] != "partitioned-hnsw-v1"
        or evidence["partition_policy"] != G7_ANN_PARTITION_POLICY
        or evidence["governor_mode"] not in {"bulk", "mixed"}
        or not isinstance(evidence["governor_policy_schema"], str)
        or not evidence["governor_policy_schema"]
        or not isinstance(evidence["calibration_cache_key"], str)
        or not evidence["calibration_cache_key"]
    ):
        raise GateFailure("G7 initial ANN bulk evidence identity mismatch")
    for name in (
        "input_identity", "aggregate_identity", "hardware_profile_fingerprint",
        "topology_digest",
    ):
        if not isinstance(evidence[name], str) or HEX64.fullmatch(evidence[name]) is None:
            raise GateFailure(f"G7 initial ANN bulk digest is invalid: {name}")
    for name in (
        "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "topology_workers",
    ):
        value = evidence[name]
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise GateFailure(f"G7 initial ANN bulk resource is invalid: {name}")
    if (
        evidence["planned_vectors"] != receipt["dataset"]["vector_count"]
        or evidence["planned_partitions"]
        != min(G7_LOGICAL_ANN_PARTITIONS, evidence["planned_vectors"])
        or evidence["planned_partitions"] > evidence["planned_vectors"]
        or evidence["planned_partitions"] > MAX_INITIAL_ANN_BULK_PARTITIONS
        or evidence["planned_workers"] > evidence["topology_workers"]
        or evidence["planned_workers"] > evidence["planned_partitions"]
        or (
            min(evidence["topology_workers"], evidence["planned_partitions"]) > 1
            and evidence["planned_workers"] <= 1
        )
        or (
            evidence["planned_workers"] > 1
            and evidence["worker_batches"] <= 1
        )
        or not isinstance(evidence["total_time_nanos"], int)
        or isinstance(evidence["total_time_nanos"], bool)
        or evidence["total_time_nanos"] <= 0
        or not isinstance(evidence["hard_affinity"], bool)
        or (receipt["platform"] == "linux" and evidence["hard_affinity"] is not True)
    ):
        raise GateFailure("G7 initial ANN bulk did not prove governed parallel construction")
    execution = evidence["governor_execution"]
    execution_fields = {
        "class", "compute_threads", "io_slots", "memory_bytes", "queue_ticket",
        "initial_queue_depth", "queue_time_nanos", "execution_time_nanos",
    }
    if (
        not isinstance(execution, dict)
        or set(execution) != execution_fields
        or execution["class"] != "bulk"
        or not isinstance(execution["compute_threads"], int)
        or isinstance(execution["compute_threads"], bool)
        or execution["compute_threads"] <= 0
        or execution["compute_threads"] != evidence["planned_workers"]
        or not isinstance(execution["memory_bytes"], int)
        or isinstance(execution["memory_bytes"], bool)
        or execution["memory_bytes"] <= 0
        or execution["memory_bytes"] != evidence["planned_memory_bytes"]
        or not isinstance(execution["io_slots"], int)
        or isinstance(execution["io_slots"], bool)
        or execution["io_slots"] != 0
        or not isinstance(execution["initial_queue_depth"], int)
        or isinstance(execution["initial_queue_depth"], bool)
        or execution["initial_queue_depth"] < 0
        or not isinstance(execution["queue_time_nanos"], int)
        or isinstance(execution["queue_time_nanos"], bool)
        or execution["queue_time_nanos"] < 0
        or not isinstance(execution["execution_time_nanos"], int)
        or isinstance(execution["execution_time_nanos"], bool)
        or execution["execution_time_nanos"] <= 0
        or (
            execution["queue_ticket"] is not None
            and (
                not isinstance(execution["queue_ticket"], int)
                or isinstance(execution["queue_ticket"], bool)
                or execution["queue_ticket"] < 0
            )
        )
    ):
        raise GateFailure("G7 initial ANN bulk governor execution evidence mismatch")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-tree")
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        result = validate(
            payload,
            arguments.expected_commit,
            expected_tree=arguments.expected_tree,
        )
        arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7 receipt failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
