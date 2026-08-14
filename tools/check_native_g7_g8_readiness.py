#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation for the independent G7/G8 evidence authorities."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g7_receipt import (
    GateFailure as ReceiptGateFailure,
    profile_authority,
)


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
G8_SBOM_AUTHORITY = {
    "id": "sbom-signatures-provenance",
    "status": "implemented-unhosted",
    "platforms": ["release"],
    "runner": "python packaging/g8_release_verification.py",
    "acceptance": [
        "spdx",
        "cyclonedx",
        "manifest-license-authority",
        "identity-completeness",
        "checksums",
        "cosign",
        "provenance",
    ],
}


class GateFailure(ValueError):
    pass


def load(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise GateFailure(f"{path} must contain an object")
    return value


def validate_g7_execution_workflow(workflow: str) -> None:
    qualification_marker = "\n  g7_qualification:\n"
    matrix_marker = "\n  g7-matrix:\n"
    aggregate_marker = "\n  g7-aggregate:\n"
    try:
        qualification_start = workflow.index(qualification_marker)
        matrix_start = workflow.index(matrix_marker)
        aggregate_start = workflow.index(aggregate_marker)
    except ValueError as error:
        raise GateFailure("G7 workflow lacks a separate pre-execution qualification job") from error
    if not qualification_start < matrix_start < aggregate_start:
        raise GateFailure("G7 qualification must precede dedicated execution")
    qualification = workflow[qualification_start:matrix_start]
    matrix = workflow[matrix_start:aggregate_start]
    for required in (
        "needs: [authority]",
        "runs-on: ubuntu-24.04",
        "needs.authority.result == 'success'",
        "tools/run_native_ann_durable_qualification.py",
        '--expected-commit "${{ github.sha }}"',
    ):
        if required not in qualification:
            raise GateFailure("G7 qualification is not a clean exact-SHA hosted prerequisite")
    for required in (
        "needs: [authority, g7_qualification]",
        "runs-on: [self-hosted, hyphae-g7, dedicated",
        "needs.authority.result == 'success'",
        "needs.g7_qualification.result == 'success'",
    ):
        if required not in matrix:
            raise GateFailure("dedicated G7 execution is not gated by successful qualification")
    lowered = workflow.lower()
    for forbidden in (
        "aws ec2 run-instances",
        "aws cloudformation deploy",
        "terraform apply",
        "pulumi up",
    ):
        if forbidden in lowered:
            raise GateFailure("G7 readiness workflow must not provision infrastructure")


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
    try:
        g7_authority = profile_authority(g7)
    except ReceiptGateFailure as error:
        raise GateFailure(str(error)) from error
    if (
        g7.get("required_background_modes") != ["control", "interference"]
        or g7_authority.observations != 1_000_000
        or g7_authority.warmup != 100_000
        or (
            g7_authority.documents,
            g7_authority.vectors,
            g7_authority.vector_dimension,
        ) != (1_000_000, 1_000_000, 384)
        or g7.get("required_hardware") != {"dedicated": True, "virtualization": "none"}
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
    sbom = next(
        (row for row in rows if row.get("id") == "sbom-signatures-provenance"),
        None,
    )
    if sbom != G8_SBOM_AUTHORITY:
        raise GateFailure("G8 SBOM authority drifted")
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
    for required in ("check_native_g8_receipts.py", '"${{ inputs.source_commit }}"'):
        if required not in closure_workflow:
            raise GateFailure("G8 closure workflow does not enforce exact-SHA receipts")
    for forbidden in ("native-g7-aggregate.json", "check_native_g7_matrix.py"):
        if forbidden in closure_workflow:
            raise GateFailure("G8 closure workflow must remain independent from G7")
    validate_g7_execution_workflow(
        (root / ".github/workflows/native-g7-g8-readiness.yml").read_text(
            encoding="utf-8"
        )
    )
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
