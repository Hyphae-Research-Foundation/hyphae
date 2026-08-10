#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Validate the complete machine-readable G5 authority set."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
REQUIREMENTS = [
    "all-engine-atomicity", "concurrent-readers", "stable-ids-and-links",
    "structure-source", "search-source", "joins-aggregates-pushdown",
    "checkpoint-complete-state", "backup-restore",
]


class GateFailure(ValueError):
    pass


def _open(payload: dict[str, Any], schema: str) -> None:
    if payload.get("schema") != schema or payload.get("gate") != "G5":
        raise GateFailure(f"unsupported {schema}")
    if payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("G5 authorities must remain open and claim-free")


def validate(
    root: Path,
    profile: dict[str, Any],
    inventory: dict[str, Any],
    authority: dict[str, Any],
    workload: dict[str, Any],
    suite: dict[str, Any],
    predecessor: dict[str, Any],
) -> dict[str, Any]:
    documents = (
        (profile, "hyphae-native-g5-readiness-profile-v1"),
        (inventory, "hyphae-native-g5-inventory-v1"),
        (authority, "hyphae-native-g5-authority-manifest-v1"),
        (workload, "hyphae-native-g5-workload-manifest-v1"),
        (suite, "hyphae-native-g5-suite-manifest-v1"),
        (predecessor, "hyphae-native-g5-predecessor-manifest-v1"),
    )
    for payload, schema in documents:
        _open(payload, schema)
    profile_rows = profile.get("requirements")
    if not isinstance(profile_rows, list) or [row.get("id") for row in profile_rows] != REQUIREMENTS:
        raise GateFailure("profile must define the ordered eight-requirement G5 contract")
    if any(row.get("required_evidence") != "hosted" for row in profile_rows):
        raise GateFailure("every G5 requirement needs hosted evidence")
    if authority.get("requirements") != REQUIREMENTS or authority.get("required_predecessors") != ["G2", "G3", "G4"]:
        raise GateFailure("authority requirement or predecessor contract mismatch")
    if authority.get("evidence_class") != "supporting-not-closure" or authority.get("allowed_commands") != ["cargo", "python3"]:
        raise GateFailure("authority scope is invalid")
    inventory_rows = inventory.get("requirements")
    if not isinstance(inventory_rows, list) or [row.get("id") for row in inventory_rows] != REQUIREMENTS:
        raise GateFailure("inventory coverage mismatch")
    for row in inventory_rows:
        if row.get("status") not in {"open", "partial", "implemented-unhosted"} or not isinstance(row.get("gaps"), list) or not row["gaps"]:
            raise GateFailure("inventory must retain a concrete gap for every requirement")
    workload_rows = workload.get("workloads")
    suite_rows = suite.get("requirements")
    if not isinstance(workload_rows, list) or not isinstance(suite_rows, list):
        raise GateFailure("workload and suite rows are required")
    workload_ids = [row.get("id") for row in workload_rows]
    if len(workload_ids) != 8 or len(set(workload_ids)) != 8 or [row.get("requirement") for row in workload_rows] != REQUIREMENTS:
        raise GateFailure("workloads must map one-to-one to G5 requirements")
    if [row.get("id") for row in suite_rows] != REQUIREMENTS:
        raise GateFailure("suite requirement coverage mismatch")
    for row in suite_rows:
        expected_workload = workload_rows[REQUIREMENTS.index(row["id"])]["id"]
        suites = row.get("suites")
        if row.get("workloads") != [expected_workload] or not isinstance(suites, list) or not suites:
            raise GateFailure(f"invalid suite binding for {row['id']}")
        names: set[str] = set()
        for item in suites:
            name, command = item.get("name"), item.get("command")
            if not isinstance(name, str) or not name or name in names or not isinstance(command, list) or not command:
                raise GateFailure(f"invalid suite identity for {row['id']}")
            if command[0] not in authority["allowed_commands"] or any(not isinstance(part, str) or not part for part in command):
                raise GateFailure(f"unauthorized command for {row['id']}")
            names.add(name)
    predecessor_rows = predecessor.get("predecessors")
    if not isinstance(predecessor_rows, list) or [row.get("gate") for row in predecessor_rows] != ["G2", "G3", "G4"]:
        raise GateFailure("predecessor coverage mismatch")
    for row in predecessor_rows:
        reference = Path(row.get("reference", ""))
        if not HEX40.fullmatch(row.get("source_commit", "")) or not HEX64.fullmatch(row.get("sha256", "")) or reference.is_absolute() or ".." in reference.parts:
            raise GateFailure("invalid predecessor identity")
        artifact = root / reference
        if not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["sha256"]:
            raise GateFailure(f"missing or mismatched predecessor {row['gate']}")
        retained = json.loads(artifact.read_text(encoding="utf-8"))
        if retained.get("gate") != row["gate"] or retained.get("status") != "passed" or retained.get("source_commit") != row["source_commit"]:
            raise GateFailure(f"unpassed predecessor {row['gate']}")
    return {"schema": "hyphae-native-g5-manifest-audit-v1", "gate": "G5", "status": "passed", "requirements": 8, "workloads": 8, "suites": sum(len(row["suites"]) for row in suite_rows), "predecessors": 3, "claims": [], "closure_declared": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    for name in ("profile", "inventory", "authority", "workload", "suite", "predecessor"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        payloads = [json.loads(getattr(args, name).read_text(encoding="utf-8")) for name in ("profile", "inventory", "authority", "workload", "suite", "predecessor")]
        result = validate(args.root, *payloads)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G5 manifest audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
