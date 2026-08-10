#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Fail-closed validation for one controlled Native G7 receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
CELLS = {
    "embedded-structure-point-get",
    "embedded-prepared-sql-primary-key",
    "local-structure-point-get",
    "local-prepared-sql-primary-key",
    "indexed-sql-bounded-read",
    "two-index-join-bounded-read",
    "bm25-top10",
    "filtered-bm25-top10",
    "ann-top10-recall-095",
    "hybrid-top10",
    "strict-group-commit",
}
TARGETS_NS = {
    "embedded-structure-point-get": (2_000, 10_000),
    "local-structure-point-get": (25_000, 100_000),
    "embedded-prepared-sql-primary-key": (5_000, 25_000),
    "local-prepared-sql-primary-key": (35_000, 150_000),
    "indexed-sql-bounded-read": (50_000, 250_000),
    "two-index-join-bounded-read": (75_000, 400_000),
    "bm25-top10": (100_000, 500_000),
    "filtered-bm25-top10": (200_000, 750_000),
    "ann-top10-recall-095": (250_000, 900_000),
    "hybrid-top10": (400_000, 950_000),
}
COUNTERS = {
    "allocations",
    "rss",
    "cpu_cycles",
    "cache_misses",
    "page_faults",
    "bytes_read",
    "bytes_written",
}


class GateFailure(ValueError):
    pass


def validate(payload: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("source commit is not canonical SHA-1")
    required = {
        "schema", "gate", "status", "evidence_class", "source_commit", "platform",
        "state", "concurrency", "background_mode", "dataset", "hardware", "cells", "counters", "saturation",
        "background_interference", "claims", "closure_declared", "physical_observation",
        "build", "workload", "durability", "proofs_included", "correctness",
    }
    if set(payload) != required:
        raise GateFailure("G7 receipt fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-receipt-v2"
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
        or dataset.get("observations", 0) < 1_000_000
        or dataset.get("search_documents", 0) < 1_000_000
        or dataset.get("vector_count", 0) < 1_000_000
        or dataset.get("vector_dimension") != 384
        or not isinstance(dataset.get("generator"), str)
        or HEX64.fullmatch(dataset.get("digest", "")) is None
    ):
        raise GateFailure("G7 dataset does not meet the normative corpus")
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
        or HEX40.fullmatch(str(build.get("source_tree", ""))) is None
    ):
        raise GateFailure("G7 build identity is incomplete")
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
        or set(saturation.get("throughput_per_second", {})) != CELLS
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
        or set(background.get("p99_ratio_by_cell", {})) != CELLS
    ):
        raise GateFailure("G7 background control comparison is incomplete")
    cells = payload["cells"]
    if not isinstance(cells, dict) or set(cells) != CELLS:
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
            and name in TARGETS_NS
        ):
            target_p50, target_p99 = TARGETS_NS[name]
            if cell["p50"] > target_p50 or cell["p99"] > target_p99:
                raise GateFailure(f"G7 latency target missed: {name}")
    ann_recall = cells["ann-top10-recall-095"].get("recall_at_10")
    if not isinstance(ann_recall, (int, float)) or ann_recall < 0.95:
        raise GateFailure("G7 ANN recall floor was not met")
    counters = payload["counters"]
    if set(counters) != COUNTERS:
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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        result = validate(payload, arguments.expected_commit)
        arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7 receipt failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
