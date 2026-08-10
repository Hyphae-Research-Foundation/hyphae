#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Fail-closed readiness evaluation for the Hyphae Native G1 substrate gate."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

PROFILE_SCHEMA = "hyphae-native-g1-readiness-profile-v1"
EVIDENCE_SCHEMA = "hyphae-native-g1-readiness-evidence-v1"
LEVELS = {"local": 0, "hosted": 1, "external-governance": 2}


class GateFailure(ValueError):
    pass


def evaluate(root: Path, profile: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    if profile.get("schema") != PROFILE_SCHEMA or profile.get("gate") != "G1":
        raise GateFailure("unsupported G1 readiness profile")
    if evidence.get("schema") != EVIDENCE_SCHEMA or evidence.get("gate") != "G1":
        raise GateFailure("unsupported G1 evidence map")
    requirements = profile.get("requirements")
    rows = evidence.get("evidence")
    if not isinstance(requirements, list) or not isinstance(rows, dict):
        raise GateFailure("G1 profile and evidence must be structured")
    ids = [row.get("id") for row in requirements]
    if len(ids) != 7 or len(set(ids)) != 7:
        raise GateFailure("G1 requires exactly seven unique requirements")
    unknown = set(rows) - set(ids)
    if unknown:
        raise GateFailure(f"unknown G1 evidence: {sorted(unknown)}")

    results = []
    for requirement in requirements:
        requirement_id = requirement["id"]
        required_level = requirement["required_evidence"]
        row = rows.get(requirement_id)
        status = "not-configured"
        if row is not None:
            required = {"status", "level", "reference", "artifact_sha256"}
            if set(row) != required:
                raise GateFailure(f"invalid evidence fields for {requirement_id}")
            if row["status"] != "passed":
                status = "failed"
            else:
                if row["level"] not in LEVELS or LEVELS[row["level"]] < LEVELS[required_level]:
                    raise GateFailure(f"insufficient evidence level for {requirement_id}")
                reference = Path(row["reference"])
                if reference.is_absolute() or ".." in reference.parts:
                    raise GateFailure(f"invalid evidence reference for {requirement_id}")
                artifact = root / reference
                if not artifact.is_file():
                    raise GateFailure(f"missing evidence artifact for {requirement_id}")
                digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                if row["artifact_sha256"] != digest:
                    raise GateFailure(f"artifact digest mismatch for {requirement_id}")
                payload = json.loads(artifact.read_text(encoding="utf-8"))
                if payload.get("status") != "passed":
                    raise GateFailure(f"artifact is not passed for {requirement_id}")
                status = "passed"
        results.append({"id": requirement_id, "status": status, "required_evidence": required_level})

    passed = sum(row["status"] == "passed" for row in results)
    return {
        "schema": "hyphae-native-g1-readiness-v1",
        "gate": "G1",
        "status": "passed" if passed == 7 else "failed",
        "required": 7,
        "passed": passed,
        "requirements": results,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = evaluate(
            args.root,
            json.loads(args.profile.read_text(encoding="utf-8")),
            json.loads(args.evidence.read_text(encoding="utf-8")),
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G1 readiness failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
