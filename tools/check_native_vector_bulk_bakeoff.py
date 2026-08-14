#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation for the experimental P4 vector bulk bakeoff."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
MISSING_GATE_EVIDENCE = {
    "peak-rss",
    "write-amplification",
    "checkpoint-restart",
    "durable-publication-and-reopen",
    "update-delete-consolidation",
    "accepted-corpus-matrix",
}


class GateFailure(ValueError):
    pass


def _object(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise GateFailure(f"{label} fields mismatch")
    return value


def _integer(value: Any, label: str, minimum: int = 0) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise GateFailure(f"{label} is invalid")
    return value


def _digest(value: Any, pattern: re.Pattern[str], label: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise GateFailure(f"{label} is not canonical")
    return value


def validate(payload: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    _digest(expected_commit, HEX40, "expected commit")
    _object(
        payload,
        {
            "schema",
            "status",
            "source_commit",
            "hardware_fingerprint",
            "governor_calibration_cache_key",
            "dataset",
            "build",
            "quality",
            "missing_gate_evidence",
            "claims",
            "closure_declared",
        },
        "bakeoff receipt",
    )
    if (
        payload["schema"] != "hyphae-native-vector-bulk-bakeoff-v1"
        or payload["status"] != "diagnostic"
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("bakeoff receipt identity or open state mismatch")
    if _digest(payload["source_commit"], HEX40, "source commit") != expected_commit:
        raise GateFailure("bakeoff source commit mismatch")
    _digest(payload["hardware_fingerprint"], HEX64, "hardware fingerprint")
    _digest(payload["governor_calibration_cache_key"], HEX64, "calibration key")
    dataset = _object(
        payload["dataset"],
        {"generator", "digest", "vectors", "dimension", "metric", "corpus_construction_nanos"},
        "bakeoff dataset",
    )
    if dataset["generator"] != "hyphae-partitioned-hnsw-bakeoff-v1" or dataset["metric"] != "squared-l2":
        raise GateFailure("bakeoff dataset identity mismatch")
    vectors = _integer(dataset["vectors"], "vector count", 1)
    dimension = _integer(dataset["dimension"], "vector dimension", 1)
    if dimension > 65_535:
        raise GateFailure("vector dimension exceeds the native field")
    _integer(dataset["corpus_construction_nanos"], "corpus time")
    _digest(dataset["digest"], HEX64, "dataset digest")
    build = _object(
        payload["build"],
        {
            "requested_partitions",
            "effective_partitions",
            "serial_hnsw_nanos",
            "serial_partitioned_nanos",
            "parallel_partitioned_nanos",
            "planned_compute_threads",
            "planned_memory_bytes",
            "worker_batches",
            "single_build_identity",
            "partitioned_build_identity",
            "deterministic_across_serial_and_parallel",
            "durable_publication",
        },
        "bakeoff build",
    )
    requested = _integer(build["requested_partitions"], "requested partitions", 1)
    effective = _integer(build["effective_partitions"], "effective partitions", 1)
    workers = _integer(build["planned_compute_threads"], "planned workers", 1)
    batches = _integer(build["worker_batches"], "worker batches", 1)
    if effective > requested or effective > vectors or workers > effective or batches > effective:
        raise GateFailure("bakeoff partition or worker bounds mismatch")
    for field in ("serial_hnsw_nanos", "serial_partitioned_nanos", "parallel_partitioned_nanos"):
        _integer(build[field], field)
    _integer(build["planned_memory_bytes"], "planned memory", 1)
    _digest(build["single_build_identity"], HEX64, "single build identity")
    _digest(build["partitioned_build_identity"], HEX64, "partitioned build identity")
    if build["deterministic_across_serial_and_parallel"] is not True or build["durable_publication"] is not False:
        raise GateFailure("bakeoff determinism or durability boundary mismatch")
    quality = _object(
        payload["quality"],
        {
            "queries",
            "k",
            "ef_search",
            "selected_partitions",
            "single_hnsw_recall_ppm",
            "partitioned_hnsw_recall_ppm",
            "selected_partition_recall_ppm",
            "minimum_single_query_recall_ppm",
            "minimum_partitioned_query_recall_ppm",
            "minimum_selected_query_recall_ppm",
            "single_query_batch_nanos",
            "partitioned_query_batch_nanos",
            "selected_query_batch_nanos",
            "oracle",
        },
        "bakeoff quality",
    )
    queries = _integer(quality["queries"], "query count", 1)
    k = _integer(quality["k"], "quality k", 1)
    ef_search = _integer(quality["ef_search"], "quality ef_search", 1)
    selected = _integer(quality["selected_partitions"], "selected partitions", 1)
    if queries > vectors or k > vectors or ef_search < k or ef_search > 256 or selected > effective:
        raise GateFailure("bakeoff quality bounds mismatch")
    for aggregate, minimum in (
        ("single_hnsw_recall_ppm", "minimum_single_query_recall_ppm"),
        ("partitioned_hnsw_recall_ppm", "minimum_partitioned_query_recall_ppm"),
        ("selected_partition_recall_ppm", "minimum_selected_query_recall_ppm"),
    ):
        aggregate_value = _integer(quality[aggregate], aggregate)
        minimum_value = _integer(quality[minimum], minimum)
        if aggregate_value > 1_000_000 or minimum_value > aggregate_value:
            raise GateFailure("bakeoff recall bounds mismatch")
    _integer(quality["single_query_batch_nanos"], "single query time")
    _integer(quality["partitioned_query_batch_nanos"], "partitioned query time")
    _integer(quality["selected_query_batch_nanos"], "selected query time")
    if quality["oracle"] != "partitioned-exact-flat-canonical-top-k-v1":
        raise GateFailure("bakeoff oracle mismatch")
    missing = payload["missing_gate_evidence"]
    if not isinstance(missing, list) or set(missing) != MISSING_GATE_EVIDENCE or len(missing) != len(set(missing)):
        raise GateFailure("bakeoff missing-evidence disclosure mismatch")
    return {
        "status": "passed",
        "source_commit": expected_commit,
        "vectors": vectors,
        "dimension": dimension,
        "effective_partitions": effective,
        "planned_compute_threads": workers,
        "partitioned_recall_ppm": quality["partitioned_hnsw_recall_ppm"],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("receipt", type=Path)
    parser.add_argument("expected_commit")
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        audit = validate(payload, arguments.expected_commit)
    except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
        parser.error(str(error))
    print(json.dumps(audit, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
