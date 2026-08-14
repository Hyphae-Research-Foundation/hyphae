#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Semantic validation for the G1 process-crash boundary receipt."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

COMMIT_BOUNDARIES = {
    "blob-staged": ("prior-empty", None, 0),
    "blob-promoted": ("prior-empty", None, 1),
    "page-appended": ("prior-empty", None, 1),
    "page-synchronized": ("prior-empty", None, 1),
    "wal-appended": ("complete-csn-1", 1, 1),
    "wal-synchronized": ("complete-csn-1", 1, 1),
    "root-published": ("complete-csn-1", 1, 1),
}
CHECKPOINT_BOUNDARIES = {
    "manifest-staged",
    "manifest-published",
    "wal-appended",
    "wal-synchronized",
}
SNAPSHOT_PIN_BOUNDARIES = {"record-synchronized", "record-published"}


class GateFailure(ValueError):
    pass


def _exact_rows(rows: Any, expected: set[str], label: str) -> dict[str, dict[str, Any]]:
    if not isinstance(rows, list):
        raise GateFailure(f"{label} rows are missing")
    by_name: dict[str, dict[str, Any]] = {}
    for row in rows:
        if not isinstance(row, dict):
            raise GateFailure(f"{label} row is not an object")
        name = row.get("boundary")
        if not isinstance(name, str) or name in by_name:
            raise GateFailure(f"{label} boundary set mismatch")
        by_name[name] = row
    if len(by_name) != len(rows) or set(by_name) != expected:
        raise GateFailure(f"{label} boundary set mismatch")
    return by_name


def _hard_killed(row: dict[str, Any]) -> bool:
    termination = row.get("termination")
    return termination in {"signal-9", "exit-code--9", "terminated-without-exit-code"}


def validate_receipt(receipt: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    if receipt.get("schema") not in {
        "hyphae.native.process-crash-matrix.v3",
        "hyphae.native.process-crash-matrix.v4",
    }:
        raise GateFailure("unsupported crash receipt schema")
    if receipt.get("status") != "process-crash-not-power-loss":
        raise GateFailure("receipt scope is not process crash")
    if receipt.get("source_commit") != expected_commit:
        raise GateFailure("receipt source commit mismatch")
    if receipt.get("all_engine_csn") != 1 or receipt.get("durability") != "strict":
        raise GateFailure("receipt is not the strict all-engine CSN fixture")

    commits = _exact_rows(
        receipt.get("commit_boundaries"), set(COMMIT_BOUNDARIES), "commit"
    )
    for name, (state, csn, blobs) in COMMIT_BOUNDARIES.items():
        row = commits[name]
        if not _hard_killed(row):
            raise GateFailure(f"commit boundary {name} was not hard-killed")
        if row.get("expected_state") != state:
            raise GateFailure(f"commit boundary {name} expectation mismatch")
        if state == "prior-empty" and row.get("recovered_csn") is not None:
            raise GateFailure(f"commit boundary {name} did not recover prior state")
        if state == "prior-empty" and row.get("recovered_blob_count") != blobs:
            raise GateFailure(f"commit boundary {name} blob staging mismatch")
        if state == "complete-csn-1" and (
            row.get("recovered_csn") != csn
            or row.get("recovered_blob_count") != blobs
        ):
            raise GateFailure(f"commit boundary {name} did not recover complete state")

    checkpoints = _exact_rows(
        receipt.get("checkpoint_boundaries"), CHECKPOINT_BOUNDARIES, "checkpoint"
    )
    for name, row in checkpoints.items():
        if not _hard_killed(row):
            raise GateFailure(f"checkpoint boundary {name} was not hard-killed")
        if name != "manifest-staged" and row.get("recovered_temporary_manifests") != 0:
            raise GateFailure(f"checkpoint boundary {name} leaked a staged manifest")
        if name == "manifest-staged" and row.get("recovered_temporary_manifests") != 1:
            raise GateFailure("manifest-staged boundary did not preserve exactly one staged file")
        if name.startswith("wal-") and (
            row.get("checkpoint_count") != 1
            or row.get("unanchored_manifest_suffix") != 0
        ):
            raise GateFailure(f"checkpoint boundary {name} did not recover anchored state")

    pins = _exact_rows(
        receipt.get("snapshot_pin_boundaries"),
        SNAPSHOT_PIN_BOUNDARIES,
        "snapshot-pin",
    )
    for name, row in pins.items():
        if not _hard_killed(row):
            raise GateFailure(f"snapshot-pin boundary {name} was not hard-killed")
    if pins["record-synchronized"].get("recovered_pin_count") != 0:
        raise GateFailure("unpublished snapshot pin became visible")
    if pins["record-published"].get("recovered_pin_count") != 1:
        raise GateFailure("published snapshot pin was not recovered")

    return {
        "schema": "hyphae-native-g1-crash-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "commit_boundaries": len(commits),
        "checkpoint_boundaries": len(checkpoints),
        "snapshot_pin_boundaries": len(pins),
        "scope": "process-crash-not-power-loss",
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate_receipt(
            json.loads(args.receipt.read_text(encoding="utf-8")), args.expected_commit
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G1 crash receipt failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
