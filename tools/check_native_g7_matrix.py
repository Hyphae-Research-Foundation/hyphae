#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed validation for the complete controlled G7 matrix."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g7_receipt import CELLS, COUNTERS, GateFailure, validate


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
STATES = {"warm", "cold"}
CONCURRENCIES = {1, 8, 32}


def validate_matrix(payload: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    if set(payload) != {
        "schema", "gate", "status", "source_commit", "platform", "states",
        "concurrency", "receipts", "claims", "closure_declared",
    }:
        raise GateFailure("G7 matrix fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-matrix-v1"
        or payload["gate"] != "G7"
        or payload["source_commit"] != expected_commit
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("G7 matrix identity or open state mismatch")
    if set(payload["states"]) != STATES or set(payload["concurrency"]) != CONCURRENCIES:
        raise GateFailure("G7 matrix dimensions mismatch")
    receipts = payload["receipts"]
    if not isinstance(receipts, list) or len(receipts) != 6:
        raise GateFailure("G7 matrix must contain six state/concurrency receipts")
    seen: set[tuple[str, int]] = set()
    for receipt in receipts:
        audit = validate(receipt, expected_commit)
        identity = (audit["state"], audit["concurrency"])
        if identity in seen:
            raise GateFailure("G7 matrix has duplicate state/concurrency receipt")
        seen.add(identity)
        if set(receipt["cells"]) != CELLS:
            raise GateFailure("G7 matrix receipt is missing a required cell")
        if set(receipt["counters"]) != COUNTERS:
            raise GateFailure("G7 matrix receipt is missing a required counter")
    if seen != {(state, concurrency) for state in STATES for concurrency in CONCURRENCIES}:
        raise GateFailure("G7 matrix coverage is incomplete")
    return {
        "schema": "hyphae-native-g7-matrix-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "platform": payload["platform"],
        "receipts": len(receipts),
        "cells_per_receipt": len(CELLS),
        "claims": [],
        "closure_declared": False,
    }


def validate_closure_aggregate(payload: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    if set(payload) != {"schema", "gate", "status", "source_commit", "platforms", "claims", "closure_declared"}:
        raise GateFailure("G7 aggregate fields mismatch")
    if (
        payload.get("schema") != "hyphae-native-g7-aggregate-v1"
        or payload.get("gate") != "G7"
        or payload.get("source_commit") != expected_commit
        or payload.get("claims") != []
        or payload.get("closure_declared") is not False
    ):
        raise GateFailure("G7 aggregate identity or closure state mismatch")
    platforms = payload.get("platforms")
    if not isinstance(platforms, dict) or set(platforms) != {"linux", "macos", "windows"}:
        raise GateFailure("G7 aggregate does not contain all required platforms")
    for platform, row in platforms.items():
        audit = row.get("audit") if isinstance(row, dict) else None
        if not isinstance(audit, dict) or audit.get("status") != "passed":
            raise GateFailure(f"G7 platform audit is not passed: {platform}")
        counter_status = row.get("counter_status")
        if not isinstance(counter_status, dict) or set(counter_status) != COUNTERS:
            raise GateFailure(f"G7 platform counters are incomplete: {platform}")
        if any(status != "measured" for status in counter_status.values()):
            raise GateFailure(f"G7 platform has unavailable counters: {platform}")
    return {
        "schema": "hyphae-native-g7-closure-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "platforms": ["linux", "macos", "windows"],
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--closure", action="store_true")
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.matrix.read_text(encoding="utf-8"))
        result = validate_closure_aggregate(payload, arguments.expected_commit) if arguments.closure else validate_matrix(payload, arguments.expected_commit)
        arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7 matrix failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
