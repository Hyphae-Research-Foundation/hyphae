#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Produce a digest-bound audit of retained G2-G4 predecessor evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

from tools.check_native_g5_manifests import GateFailure


def audit(root: Path, manifest: dict, manifest_sha256: str) -> dict:
    if len(manifest_sha256) != 64:
        raise GateFailure("predecessor manifest digest is invalid")
    rows = manifest.get("predecessors")
    if manifest.get("schema") != "hyphae-native-g5-predecessor-manifest-v1" or manifest.get("gate") != "G5" or manifest.get("claims") != [] or manifest.get("closure_declared") is not False:
        raise GateFailure("unsupported predecessor manifest")
    if not isinstance(rows, list) or [row.get("gate") for row in rows] != ["G2", "G3", "G4"]:
        raise GateFailure("predecessor chain mismatch")
    audited = []
    for row in rows:
        reference = Path(row["reference"])
        artifact = root / reference
        if reference.is_absolute() or ".." in reference.parts or not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["sha256"]:
            raise GateFailure(f"predecessor artifact mismatch for {row['gate']}")
        payload = json.loads(artifact.read_text(encoding="utf-8"))
        if payload.get("gate") != row["gate"] or payload.get("status") != "passed" or payload.get("source_commit") != row["source_commit"]:
            raise GateFailure(f"predecessor identity mismatch for {row['gate']}")
        audited.append({"gate": row["gate"], "source_commit": row["source_commit"], "artifact_sha256": row["sha256"]})
    return {"schema": "hyphae-native-g5-predecessor-audit-v1", "gate": "G5", "status": "passed", "manifest_sha256": manifest_sha256, "predecessors": audited, "claims": [], "closure_declared": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        raw = args.manifest.read_bytes()
        if hashlib.sha256(raw).hexdigest() != args.manifest_sha256:
            raise GateFailure("predecessor manifest digest mismatch")
        result = audit(args.root, json.loads(raw), args.manifest_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G5 predecessor audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
