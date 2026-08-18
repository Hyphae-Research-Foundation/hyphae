#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed validation for the complete controlled G7 matrix."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g7_receipt import (
    CELLS,
    COUNTERS,
    GateFailure,
    resolve_expected_tree,
    validate,
)


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
STATES = {"warm"}
CONCURRENCIES = {1, 8, 32}


def validate_matrix(
    payload: dict[str, Any],
    expected_commit: str,
    *,
    expected_tree: str | None = None,
) -> dict[str, Any]:
    source_tree = (
        resolve_expected_tree(expected_commit)
        if expected_tree is None
        else expected_tree
    )
    if set(payload) != {
        "schema", "gate", "status", "source_commit", "platform", "states",
        "concurrency", "background_modes", "receipts", "claims", "closure_declared",
    }:
        raise GateFailure("G7 matrix fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-matrix-v4"
        or payload["gate"] != "G7"
        or payload["status"] != "closure-candidate"
        or payload["source_commit"] != expected_commit
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("G7 matrix identity or open state mismatch")
    if (
        set(payload["states"]) != STATES
        or set(payload["concurrency"]) != CONCURRENCIES
        or set(payload["background_modes"]) != {"control", "interference"}
    ):
        raise GateFailure("G7 matrix dimensions mismatch")
    receipts = payload["receipts"]
    if not isinstance(receipts, list) or len(receipts) != 6:
        raise GateFailure("G7 matrix must contain six warm concurrency/background receipts")
    seen: set[tuple[str, int, str]] = set()
    build_identities: set[str] = set()
    initial_ann_bulk_identities: set[str] = set()
    dataset_digests: set[str] = set()
    recovered_group_state_digests: set[str] = set()
    for receipt in receipts:
        audit = validate(
            receipt,
            expected_commit,
            expected_tree=source_tree,
        )
        identity = (audit["state"], audit["concurrency"], receipt["background_mode"])
        if identity in seen:
            raise GateFailure("G7 matrix has duplicate state/concurrency receipt")
        seen.add(identity)
        if set(receipt["cells"]) != CELLS:
            raise GateFailure("G7 matrix receipt is missing a required cell")
        if set(receipt["counters"]) != COUNTERS:
            raise GateFailure("G7 matrix receipt is missing a required counter")
        build_identities.add(json.dumps(receipt["build"], sort_keys=True))
        initial_ann_bulk_identities.add(
            json.dumps(receipt["initial_ann_bulk"], sort_keys=True)
        )
        dataset_digests.add(receipt["dataset"]["digest"])
        recovered_group_state_digests.add(
            receipt["cells"]["strict-group-commit"]["group_commit_evidence"][
                "reopen"
            ]["recovered_state_digest"]
        )
    if (
        len(build_identities) != 1
        or len(initial_ann_bulk_identities) != 1
        or len(dataset_digests) != 1
        or len(recovered_group_state_digests) != 1
    ):
        raise GateFailure(
            "G7 matrix receipts do not share one build, ANN generation, dataset, "
            "and recovered group-commit state"
        )
    if seen != {
        (state, concurrency, background)
        for state in STATES
        for concurrency in CONCURRENCIES
        for background in ("control", "interference")
    }:
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
        payload.get("schema") != "hyphae-native-g7-aggregate-v2"
        or payload.get("gate") != "G7"
        or payload.get("source_commit") != expected_commit
        or payload.get("status") != "passed"
        or payload.get("claims") != ["G7"]
        or payload.get("closure_declared") is not True
    ):
        raise GateFailure("G7 aggregate identity or closure state mismatch")
    platforms = payload.get("platforms")
    if (
        not isinstance(platforms, dict)
        or len(platforms) != 1
        or not set(platforms).issubset({"linux", "darwin"})
    ):
        raise GateFailure("G7 aggregate does not contain one supported dedicated platform")
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
        "platforms": sorted(platforms),
        "claims": ["G7"],
        "closure_declared": True,
    }


def validate_closure_bundle(
    payload: dict[str, Any],
    evidence_root: Path,
    expected_commit: str,
    *,
    expected_tree: str | None = None,
) -> dict[str, Any]:
    result = validate_closure_aggregate(payload, expected_commit)
    matrix_paths = sorted(evidence_root.rglob("native-g7-matrix.json"))
    if len(matrix_paths) != 1:
        raise GateFailure("G7 closure bundle must contain exactly one raw dedicated matrix")
    path = matrix_paths[0]
    if path.is_symlink() or not path.is_file():
        raise GateFailure("G7 raw matrix must be one regular file")
    matrix = json.loads(path.read_text(encoding="utf-8"))
    audit = validate_matrix(
        matrix,
        expected_commit,
        expected_tree=expected_tree,
    )
    platform = matrix.get("platform")
    if platform not in {"linux", "darwin"} or set(payload["platforms"]) != {platform}:
        raise GateFailure("G7 raw closure matrix platform differs from the aggregate")
    row = payload["platforms"][platform]
    expected_counters = {
        name: (
            "measured"
            if all(receipt["counters"][name]["status"] == "measured" for receipt in matrix["receipts"])
            else "unavailable"
        )
        for name in matrix["receipts"][0]["counters"]
    }
    if (
        row.get("matrix_sha256") != hashlib.sha256(path.read_bytes()).hexdigest()
        or row.get("audit") != audit
        or row.get("counter_status") != expected_counters
    ):
        raise GateFailure("G7 aggregate does not bind the supplied raw matrix")
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-tree")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--closure", action="store_true")
    parser.add_argument("--receipts", type=Path)
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.matrix.read_text(encoding="utf-8"))
        if arguments.closure:
            if arguments.receipts is None:
                raise GateFailure("--closure requires --receipts")
            result = validate_closure_bundle(
                payload,
                arguments.receipts,
                arguments.expected_commit,
                expected_tree=arguments.expected_tree,
            )
        else:
            result = validate_matrix(
                payload,
                arguments.expected_commit,
                expected_tree=arguments.expected_tree,
            )
        arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7 matrix failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
