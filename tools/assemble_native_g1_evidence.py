#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Assemble seven exact-SHA hosted artifacts into a G1 evidence map."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

ASSEMBLY = [
    ("native-page-blob-wal-catalog-mvcc", "hosted", "native-g1-kernel-audit.json"),
    ("partitioned-memory-and-scheduler", "hosted", "native-g1-scheduler-audit.json"),
    ("no-redb-on-native-target-path", "hosted", "native-g1-substrate-audit.json"),
    ("three-engine-minimal-vertical", "hosted", "native-g1-vertical-audit.json"),
    ("single-csn-all-engine-commit", "hosted", "native-g1-all-engine-audit.json"),
    ("commit-checkpoint-crash-matrix", "hosted", "native-g1-crash-audit.json"),
    ("embedded-and-local-protocol-latency", "hosted", "native-g1-latency-aggregate.json"),
]


def assemble(
    root: Path,
    profile: dict[str, Any],
    baseline: dict[str, Any],
    expected_commit: str,
) -> dict[str, Any]:
    profile_ids = [row["id"] for row in profile["requirements"]]
    if [row[0] for row in ASSEMBLY] != profile_ids:
        raise ValueError("G1 assembly does not match the exact profile")
    evidence = json.loads(json.dumps(baseline))
    rows = {}
    for requirement, level, reference in ASSEMBLY:
        artifact = root / reference
        if not artifact.is_file():
            raise ValueError(f"missing G1 artifact: {reference}")
        payload = json.loads(artifact.read_text(encoding="utf-8"))
        if payload.get("status") != "passed":
            raise ValueError(f"G1 artifact did not pass: {reference}")
        if payload.get("source_commit") != expected_commit:
            raise ValueError(f"G1 artifact commit mismatch: {reference}")
        rows[requirement] = {
            "status": "passed",
            "level": level,
            "reference": reference,
            "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        }
    evidence["evidence"] = rows
    return evidence


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = assemble(
            args.root,
            json.loads(args.profile.read_text(encoding="utf-8")),
            json.loads(args.evidence.read_text(encoding="utf-8")),
            args.expected_commit,
        )
    except (ValueError, OSError, json.JSONDecodeError) as error:
        print(f"native G1 evidence assembly failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
