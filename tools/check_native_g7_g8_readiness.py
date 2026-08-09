#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed validation for the open G7/G8 evidence authority spine."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def load(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise GateFailure(f"{path} must contain an object")
    return value


def validate(root: Path, expected_commit: str) -> dict[str, Any]:
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("expected commit is not a canonical SHA-1")
    g7 = load(root / "config/native-g7-readiness-profile.json")
    g8 = load(root / "config/native-g8-readiness-profile.json")
    suites = load(root / "config/native-g8-suite-manifest.json")
    if g7.get("schema") != "hyphae-native-g7-readiness-profile-v3" or g7.get("gate") != "G7":
        raise GateFailure("invalid G7 profile")
    if g8.get("schema") != "hyphae-native-g8-readiness-profile-v2" or g8.get("gate") != "G8":
        raise GateFailure("invalid G8 profile")
    if suites.get("schema") != "hyphae-native-g8-suite-manifest-v2" or suites.get("gate") != "G8":
        raise GateFailure("invalid G8 suite manifest")
    for payload in (g7, g8, suites):
        if payload.get("claims") != [] or payload.get("closure_declared") is not False:
            raise GateFailure("G7/G8 authority must remain open and claim-free")
    cells = g7.get("required_cells")
    if not isinstance(cells, list) or not cells or len(cells) != len(set(cells)):
        raise GateFailure("G7 cells are invalid")
    if g7.get("required_states") != ["warm"] or g7.get("required_concurrency") != [1, 8, 32]:
        raise GateFailure("G7 state or concurrency matrix drifted")
    if g7.get("cold_diagnostics") != {
        "closure_claim": False,
        "method": "separate-first-touch-observations",
        "reason": "cold I/O has no universal latency target and cannot be represented by a million repeated accesses",
    }:
        raise GateFailure("G7 cold diagnostic boundary drifted")
    if (
        g7.get("required_background_modes") != ["control", "interference"]
        or g7.get("minimum_hot_observations") != 1_000_000
        or g7.get("required_dataset") != {"documents": 1_000_000, "vectors": 1_000_000, "vector_dimension": 384}
        or g7.get("required_hardware") != {"dedicated": True, "virtualization": "none"}
        or set(g7.get("warm_targets_nanoseconds", {})) != set(cells)
    ):
        raise GateFailure("G7 normative measurement authority drifted")
    counters = g7.get("required_counters")
    if not isinstance(counters, list) or not counters or len(counters) != len(set(counters)):
        raise GateFailure("G7 counters are invalid")
    requirements = g8.get("required_requirements")
    rows = suites.get("requirements")
    if not isinstance(requirements, list) or not isinstance(rows, list) or [row.get("id") for row in rows] != requirements:
        raise GateFailure("G8 requirement ordering or identity drifted")
    if any(row.get("status") != "implemented-unhosted" for row in rows):
        raise GateFailure("G8 foundation is not completely implemented for hosted execution")
    for row in rows:
        platforms = row.get("platforms")
        acceptance = row.get("acceptance")
        if (
            not isinstance(platforms, list)
            or not platforms
            or len(platforms) != len(set(platforms))
            or not isinstance(acceptance, list)
            or not acceptance
            or len(acceptance) != len(set(acceptance))
            or not isinstance(row.get("runner"), str)
            or not row["runner"]
        ):
            raise GateFailure(f"invalid G8 suite definition: {row.get('id')}")
    packaging = next((row for row in rows if row.get("id") == "multiplatform-packaging"), None)
    if (
        g8.get("required_platforms") != ["linux", "macos", "windows"]
        or g8.get("required_release_targets") != [
            "x86_64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ]
        or packaging is None
        or packaging.get("platforms") != g8.get("required_release_targets")
    ):
        raise GateFailure("G8 release target coverage drifted")
    closure_workflow = (root / ".github/workflows/native-g8-closure.yml").read_text(
        encoding="utf-8"
    )
    for required in (
        "native-g7-aggregate.json",
        "check_native_g7_matrix.py",
        "--closure",
        "--receipts",
        "check_native_g8_receipts.py",
        '"${{ inputs.source_commit }}"',
    ):
        if required not in closure_workflow:
            raise GateFailure("G8 closure workflow does not enforce its G7 predecessor")
    return {
        "schema": "hyphae-native-g7-g8-readiness-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "g7": {"status": "open", "required_cells": len(cells), "required_counters": len(counters)},
        "g8": {"status": "open", "required_requirements": len(requirements), "planned": 0},
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate(Path(__file__).resolve().parents[1], args.expected_commit)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7/G8 readiness failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
