#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed readiness evaluation for the complete native relational gate."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

PROFILE_SCHEMA = "hyphae-native-g2-readiness-profile-v1"
EVIDENCE_SCHEMA = "hyphae-native-g2-readiness-evidence-v1"
LEVELS = {"local": 0, "hosted": 1, "external-governance": 2}


class GateFailure(ValueError):
    pass


def evaluate(root: Path, profile: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    if profile.get("schema") != PROFILE_SCHEMA or profile.get("gate") != "G2":
        raise GateFailure("unsupported G2 readiness profile")
    if evidence.get("schema") != EVIDENCE_SCHEMA or evidence.get("gate") != "G2":
        raise GateFailure("unsupported G2 evidence map")
    requirements = profile.get("requirements")
    rows = evidence.get("evidence")
    if not isinstance(requirements, list) or not isinstance(rows, dict):
        raise GateFailure("G2 profile and evidence must be structured")
    ids = [row.get("id") for row in requirements]
    if len(ids) != 8 or len(set(ids)) != 8:
        raise GateFailure("G2 requires exactly eight unique requirements")
    unknown = set(rows) - set(ids)
    if unknown:
        raise GateFailure(f"unknown G2 evidence: {sorted(unknown)}")
    results = []
    for requirement in requirements:
        identifier = requirement["id"]
        required_level = requirement["required_evidence"]
        row = rows.get(identifier)
        status = "not-configured"
        if row is not None:
            if set(row) != {"status", "level", "reference", "artifact_sha256"}:
                raise GateFailure(f"invalid evidence fields for {identifier}")
            if row["status"] != "passed":
                status = "failed"
            else:
                if row["level"] not in LEVELS or LEVELS[row["level"]] < LEVELS[required_level]:
                    raise GateFailure(f"insufficient evidence level for {identifier}")
                reference = Path(row["reference"])
                if reference.is_absolute() or ".." in reference.parts:
                    raise GateFailure(f"invalid evidence reference for {identifier}")
                artifact = root / reference
                if not artifact.is_file():
                    raise GateFailure(f"missing evidence artifact for {identifier}")
                digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                if row["artifact_sha256"] != digest:
                    raise GateFailure(f"artifact digest mismatch for {identifier}")
                payload = json.loads(artifact.read_text(encoding="utf-8"))
                if payload.get("status") != "passed":
                    raise GateFailure(f"artifact is not passed for {identifier}")
                status = "passed"
        results.append({"id": identifier, "status": status, "required_evidence": required_level})
    passed = sum(row["status"] == "passed" for row in results)
    return {
        "schema": "hyphae-native-g2-readiness-v1",
        "gate": "G2",
        "status": "passed" if passed == 8 else "failed",
        "required": 8,
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
        print(f"native G2 readiness failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
