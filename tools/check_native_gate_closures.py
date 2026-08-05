#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the retained, ordered Native Phase 1 gate closure prefix."""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
EXPECTED_GATES = [f"G{index}" for index in range(9)]
EXPECTED_SCHEMAS = {
    "G0": "hyphae-native-g0-closure-v1",
    "G1": "hyphae-native-g1-closure-v1",
    "G2": "hyphae-native-g2-readiness-v1",
    "G3": "hyphae-native-g3-readiness-v1",
    "G4": "hyphae-native-g4-closure-v1",
    "G5": "hyphae-native-g5-closure-v1",
}


class GateFailure(ValueError):
    pass


def _object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _load(path: Path, label: str) -> dict[str, Any]:
    try:
        return _object(json.loads(path.read_text(encoding="utf-8")), label)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateFailure(f"cannot load {label}: {path}") from error


def _profile_requirements(root: Path, gate: str) -> list[str]:
    profile = _load(
        root / "config" / f"native-{gate.lower()}-readiness-profile.json",
        f"{gate} readiness profile",
    )
    if profile.get("gate") != gate:
        raise GateFailure(f"{gate} readiness profile identity mismatch")
    requirements = profile.get("requirements")
    if not isinstance(requirements, list) or not requirements:
        raise GateFailure(f"{gate} readiness profile has no requirements")
    identifiers = []
    for row in requirements:
        identifier = _object(row, f"{gate} profile requirement").get("id")
        if not isinstance(identifier, str) or not identifier:
            raise GateFailure(f"{gate} profile has an invalid requirement")
        identifiers.append(identifier)
    if len(identifiers) != len(set(identifiers)):
        raise GateFailure(f"{gate} readiness profile has duplicate requirements")
    return identifiers


def validate(root: Path) -> dict[str, Any]:
    status = _load(root / "config/native-gate-status.json", "native gate status")
    if (
        status.get("schema") != "hyphae-native-gate-status-v1"
        or status.get("program") != "native-local-phase-1"
        or status.get("authority") != "docs/gates/native-local-phase-1.md"
    ):
        raise GateFailure("unsupported native gate status authority")
    rows = status.get("gates")
    if not isinstance(rows, list) or [row.get("id") for row in rows if isinstance(row, dict)] != EXPECTED_GATES:
        raise GateFailure("native gates must be unique and ordered G0 through G8")

    indexes = {
        "docs": (root / "docs/README.md").read_text(encoding="utf-8"),
        "status": (root / "docs/gates/native-gate-status.md").read_text(encoding="utf-8"),
        "evidence": (root / "docs/gates/evidence/README.md").read_text(encoding="utf-8"),
    }
    closed: list[str] = []
    closed_rows: dict[str, dict[str, Any]] = {}
    encountered_open = False
    for raw_row in rows:
        row = _object(raw_row, "native gate row")
        gate = row["id"]
        state = row.get("status")
        if state == "open":
            encountered_open = True
            if set(row) != {"id", "status"}:
                raise GateFailure(f"open {gate} row contains closure fields")
            continue
        if state != "closed":
            raise GateFailure(f"unsupported status for {gate}: {state}")
        if encountered_open:
            raise GateFailure(f"closed {gate} appears after an open predecessor")
        if set(row) != {"id", "status", "source_commit", "evidence", "evidence_sha256"}:
            raise GateFailure(f"closed {gate} row fields mismatch")

        source_commit = row["source_commit"]
        evidence_reference = row["evidence"]
        evidence_digest = row["evidence_sha256"]
        if not isinstance(source_commit, str) or HEX40.fullmatch(source_commit) is None:
            raise GateFailure(f"{gate} source commit is not canonical")
        expected_reference = (
            f"docs/gates/evidence/closures/native-{gate.lower()}-{source_commit[:7]}.json"
        )
        if evidence_reference != expected_reference:
            raise GateFailure(f"{gate} evidence path is not source-bound")
        if not isinstance(evidence_digest, str) or HEX64.fullmatch(evidence_digest) is None:
            raise GateFailure(f"{gate} evidence digest is not canonical")

        evidence_path = root / evidence_reference
        try:
            actual_digest = hashlib.sha256(evidence_path.read_bytes()).hexdigest()
        except OSError as error:
            raise GateFailure(f"{gate} retained evidence is missing") from error
        if actual_digest != evidence_digest:
            raise GateFailure(f"{gate} retained evidence digest mismatch")
        evidence = _load(evidence_path, f"{gate} retained closure")
        if evidence.get("schema") != EXPECTED_SCHEMAS.get(gate):
            raise GateFailure(f"{gate} retained closure schema mismatch")
        if (
            evidence.get("gate") != gate
            or evidence.get("status") != "passed"
            or evidence.get("source_commit") != source_commit
        ):
            raise GateFailure(f"{gate} retained closure identity mismatch")
        required = evidence.get("required")
        passed = evidence.get("passed")
        if (
            not isinstance(required, int)
            or isinstance(required, bool)
            or required <= 0
            or passed != required
        ):
            raise GateFailure(f"{gate} retained closure count mismatch")
        if evidence.get("requirements") != _profile_requirements(root, gate):
            raise GateFailure(f"{gate} retained closure requirements drifted")
        workflow_run = evidence.get("workflow_run")
        if not isinstance(workflow_run, int) or isinstance(workflow_run, bool) or workflow_run <= 0:
            raise GateFailure(f"{gate} retained closure workflow identity is invalid")
        if not isinstance(evidence.get("artifact"), str) or not evidence["artifact"]:
            raise GateFailure(f"{gate} retained closure artifact is missing")
        if evidence.get("production_scale") is not False:
            raise GateFailure(f"{gate} retained closure must not claim production scale")

        if gate == "G1":
            predecessor = _object(evidence.get("predecessor"), "G1 predecessor")
            previous = closed_rows.get("G0")
            if previous is None or predecessor != {
                "gate": "G0",
                "source_commit": previous["source_commit"],
                "evidence": previous["evidence"],
                "evidence_sha256": previous["evidence_sha256"],
            }:
                raise GateFailure("G1 retained closure is not bound to G0")

        docs_target = evidence_reference.removeprefix("docs/")
        status_target = evidence_reference.removeprefix("docs/gates/")
        evidence_target = evidence_reference.removeprefix("docs/gates/evidence/")
        if f"]({docs_target})" not in indexes["docs"]:
            raise GateFailure(f"{gate} closure is absent from docs/README.md")
        if f"]({status_target})" not in indexes["status"] or f"`{source_commit[:7]}`" not in indexes["status"]:
            raise GateFailure(f"{gate} closure is absent from the gate status index")
        if f"]({evidence_target})" not in indexes["evidence"]:
            raise GateFailure(f"{gate} closure is absent from the evidence index")
        closed.append(gate)
        closed_rows[gate] = row

    return {
        "schema": "hyphae-native-gate-closure-audit-v1",
        "status": "passed",
        "closed": closed,
        "open": EXPECTED_GATES[len(closed) :],
    }


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    try:
        result = validate(root)
    except (GateFailure, OSError, UnicodeError) as error:
        print(f"native gate closure validation failed: {error}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
