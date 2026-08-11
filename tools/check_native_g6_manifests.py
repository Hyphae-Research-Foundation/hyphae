#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Audit the seven exact-SHA G6 manifests without advancing gate closure."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

from tools.check_native_g6_foundation import (
    GateFailure,
    PREDECESSORS,
    REQUIREMENTS,
    validate as validate_foundation,
)

MANIFEST_NAMES = ("profile", "evidence", "inventory", "authority", "workload", "suite", "predecessor")


def load_exact(raw: dict[str, bytes], expected_sha256: dict[str, str]) -> list[dict[str, Any]]:
    if set(raw) != set(MANIFEST_NAMES) or set(expected_sha256) != set(MANIFEST_NAMES):
        raise GateFailure("G6 requires all seven named manifests")
    payloads: list[dict[str, Any]] = []
    for name in MANIFEST_NAMES:
        if hashlib.sha256(raw[name]).hexdigest() != expected_sha256[name]:
            raise GateFailure(f"G6 {name} manifest digest mismatch")
        payload = json.loads(raw[name])
        if not isinstance(payload, dict):
            raise GateFailure(f"G6 {name} manifest must be an object")
        payloads.append(payload)
    return payloads


def _safe_reference(root: Path, value: object, label: str) -> None:
    if not isinstance(value, str) or not value:
        raise GateFailure(f"invalid G6 {label} reference")
    reference = Path(value)
    if reference.is_absolute() or ".." in reference.parts:
        raise GateFailure(f"unsafe G6 {label} reference")
    resolved_root = root.resolve()
    try:
        (root / reference).resolve().relative_to(resolved_root)
    except ValueError as error:
        raise GateFailure(f"G6 {label} reference escapes the root") from error


def validate(
    root: Path,
    profile: dict[str, Any],
    evidence: dict[str, Any],
    inventory: dict[str, Any],
    authority: dict[str, Any],
    workload: dict[str, Any],
    suite: dict[str, Any],
    predecessor: dict[str, Any],
    expected_commit: str,
    manifest_sha256: dict[str, str],
) -> dict[str, Any]:
    for row in authority.get("contracts", []):
        if not isinstance(row, dict):
            raise GateFailure("invalid G6 contract authority")
        _safe_reference(root, row.get("reference"), "contract")
    for row in predecessor.get("predecessors", []):
        if not isinstance(row, dict):
            raise GateFailure("invalid G6 predecessor authority")
        _safe_reference(root, row.get("reference"), "predecessor")
    foundation = validate_foundation(
        root,
        profile,
        evidence,
        inventory,
        authority,
        workload,
        suite,
        predecessor,
        expected_commit,
        manifest_sha256,
    )
    predecessor_rows = predecessor["predecessors"]
    return {
        "schema": "hyphae-native-g6-manifest-audit-v1",
        "gate": "G6",
        "status": "passed",
        "evidence_class": "authority-not-closure",
        "source_commit": expected_commit,
        "manifest_sha256": dict(manifest_sha256),
        "requirements": len(REQUIREMENTS),
        "implemented_requirements": foundation["implemented_requirements"],
        "partial_requirements": foundation["partial_requirements"],
        "planned_requirements": foundation["planned_requirements"],
        "predecessors": [
            {
                "gate": row["gate"],
                "source_commit": row["source_commit"],
                "artifact_sha256": row["sha256"],
            }
            for row in predecessor_rows
        ],
        "predecessor_count": len(PREDECESSORS),
        "closure_status": "open",
        "claims": [],
        "closure_declared": False,
    }


def validate_raw(
    root: Path,
    raw: dict[str, bytes],
    expected_commit: str,
    manifest_sha256: dict[str, str],
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    payloads = load_exact(raw, manifest_sha256)
    return validate(root, *payloads, expected_commit, manifest_sha256), payloads


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--expected-commit", required=True)
    for name in MANIFEST_NAMES:
        parser.add_argument(f"--{name}", type=Path, required=True)
        parser.add_argument(f"--{name}-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        raw = {name: getattr(args, name).read_bytes() for name in MANIFEST_NAMES}
        digests = {name: getattr(args, f"{name}_sha256") for name in MANIFEST_NAMES}
        result, _ = validate_raw(args.root, raw, args.expected_commit, digests)
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G6 manifest audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
