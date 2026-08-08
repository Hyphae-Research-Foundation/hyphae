#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed validation for one controlled Native G7 receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
CELLS = {
    "embedded-structure-point-get",
    "embedded-prepared-sql-primary-key",
    "local-structure-point-get",
    "local-prepared-sql-primary-key",
    "indexed-sql-bounded-read",
    "bm25-top10",
    "filtered-bm25-top10",
    "ann-top10-recall-095",
    "hybrid-top10",
    "strict-group-commit",
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
        "state", "concurrency", "dataset", "cells", "counters", "saturation",
        "background_interference", "claims", "closure_declared", "physical_observation",
    }
    if set(payload) != required:
        raise GateFailure("G7 receipt fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-receipt-v1"
        or payload["gate"] != "G7"
        or payload["status"] != "passed"
        or payload["evidence_class"] != "supporting-not-closure"
        or payload["source_commit"] != expected_commit
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("G7 receipt identity or open state mismatch")
    if payload["state"] not in {"warm", "cold"} or payload["concurrency"] not in {1, 8, 32}:
        raise GateFailure("G7 state or concurrency is invalid")
    saturation = payload["saturation"]
    if not isinstance(saturation, dict) or saturation.get("status") != "measured":
        raise GateFailure("G7 saturation evidence is incomplete")
    background = payload["background_interference"]
    if not isinstance(background, dict) or background.get("status") not in {"measured", "control"}:
        raise GateFailure("G7 background-interference evidence is incomplete")
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
        if cell.get("status") == "measured" and payload["dataset"].get("observations", 0) < 100_000:
            raise GateFailure("G7 receipt has fewer than one hundred thousand observations")
    counters = payload["counters"]
    if set(counters) != COUNTERS:
        raise GateFailure("G7 counters are incomplete")
    for name, counter in counters.items():
        if not isinstance(counter, dict) or counter.get("status") not in {"measured", "unavailable"}:
            raise GateFailure(f"G7 counter status is invalid: {name}")
        if counter["status"] == "unavailable" and counter.get("value") is not None:
            raise GateFailure(f"unavailable G7 counter has a value: {name}")
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
