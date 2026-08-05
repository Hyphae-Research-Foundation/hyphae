#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate bounded embedded and local-protocol G1 latency observations."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any

EMBEDDED_OPERATIONS = {
    "embedded_structure_get_64b",
    "embedded_prepared_sql_pk_materialized_scaled_snapshot",
    "buffered_inverted_btree_bm25_match_top1_rare_term",
}
PROTOCOL_OPERATIONS = {
    "persistent_ping_round_trip_32b",
    "persistent_transaction_sql_stage_round_trip",
    "persistent_transaction_structure_stage_round_trip",
    "persistent_transaction_search_stage_round_trip",
    "persistent_transaction_memory_commit_round_trip",
    "persistent_transaction_strict_commit_round_trip",
}


class GateFailure(ValueError):
    pass


def _metric_rows(payload: dict[str, Any], required: set[str], label: str) -> None:
    operations = payload.get("operations")
    if not isinstance(operations, dict) or not required <= set(operations):
        raise GateFailure(f"{label} operation set mismatch")
    for name in required:
        row = operations[name]
        if not isinstance(row, dict):
            raise GateFailure(f"{label} metric row is invalid for {name}")
        for metric in ("p50_nanos", "p99_nanos"):
            value = row.get(metric)
            if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) or value < 0:
                raise GateFailure(f"{label} metric {metric} must be finite and nonnegative")
        throughput = row.get("throughput_per_second")
        if not isinstance(throughput, (int, float)) or isinstance(throughput, bool) or not math.isfinite(throughput) or throughput <= 0:
            raise GateFailure(f"{label} throughput must be finite and positive")
        if row["p50_nanos"] > row["p99_nanos"]:
            raise GateFailure(f"{label} percentile order is invalid")


def validate_receipts(
    embedded: dict[str, Any], protocol: dict[str, Any], expected_commit: str
) -> dict[str, Any]:
    if embedded.get("schema") != "hyphae.native.microsecond-smoke.v16":
        raise GateFailure("unsupported embedded receipt")
    if embedded.get("status") != "observation-not-gate":
        raise GateFailure("embedded receipt must remain a bounded observation")
    if embedded.get("commit") != expected_commit:
        raise GateFailure("embedded commit mismatch")
    if embedded.get("profile") != "release":
        raise GateFailure("embedded receipt is not release-profile")
    if embedded.get("observations_per_operation", 0) < 1_000_000:
        raise GateFailure("embedded observation count is too small")
    _metric_rows(embedded, EMBEDDED_OPERATIONS, "embedded")

    if protocol.get("schema") != "hyphae.native.local-all-engine-transaction-smoke.v1":
        raise GateFailure("unsupported protocol receipt")
    if protocol.get("status") != "observation-not-regression-gate":
        raise GateFailure("protocol receipt must remain a bounded observation")
    if protocol.get("implementation_commit") != expected_commit or protocol.get("harness_commit") != expected_commit:
        raise GateFailure("protocol commit mismatch")
    if protocol.get("profile") != "release" or protocol.get("concurrency") != 1:
        raise GateFailure("protocol receipt profile is invalid")
    if protocol.get("warm_state") is not True or protocol.get("staged_operations_per_transaction") != 3:
        raise GateFailure("protocol transaction fixture is invalid")
    _metric_rows(protocol, PROTOCOL_OPERATIONS, "protocol")

    return {
        "schema": "hyphae-native-g1-latency-aggregate-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "scope": "bounded-observation",
        "production_scale": False,
        "embedded_operations": len(EMBEDDED_OPERATIONS),
        "protocol_operations": len(PROTOCOL_OPERATIONS),
        "profile": "release",
        "concurrency": 1,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--embedded", type=Path, required=True)
    parser.add_argument("--protocol", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate_receipts(
            json.loads(args.embedded.read_text(encoding="utf-8")),
            json.loads(args.protocol.read_text(encoding="utf-8")),
            args.expected_commit,
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G1 latency receipt failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
