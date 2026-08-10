#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Fail-closed readiness evaluation for native search gate G4."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
LEVELS = {"local": 0, "hosted": 1, "external-governance": 2}


class GateFailure(ValueError):
    pass


def evaluate(root: Path, profile: dict[str, Any], evidence: dict[str, Any], expected_commit: str, suite_digest: str, corpus_digest: str) -> dict[str, Any]:
    if profile.get("schema") != "hyphae-native-g4-readiness-profile-v1" or profile.get("gate") != "G4":
        raise GateFailure("unsupported G4 profile")
    if evidence.get("schema") != "hyphae-native-g4-readiness-evidence-v1" or evidence.get("gate") != "G4":
        raise GateFailure("unsupported G4 evidence")
    if evidence.get("claims") != [] or evidence.get("closure_declared") is not False:
        raise GateFailure("G4 evidence must not make claims or declare closure")
    if HEX40.fullmatch(expected_commit) is None or HEX64.fullmatch(suite_digest) is None or HEX64.fullmatch(corpus_digest) is None:
        raise GateFailure("expected identities are invalid")
    requirements = profile.get("requirements")
    rows = evidence.get("evidence")
    if not isinstance(requirements, list) or not isinstance(rows, dict):
        raise GateFailure("G4 profile and evidence must be structured")
    ids = [row.get("id") for row in requirements]
    if len(ids) != 12 or len(set(ids)) != 12:
        raise GateFailure("G4 requires exactly twelve unique requirements")
    if set(rows) - set(ids):
        raise GateFailure("unknown G4 evidence")
    results = []
    for requirement in requirements:
        identifier = requirement["id"]
        required_level = requirement.get("required_evidence")
        if required_level not in LEVELS:
            raise GateFailure(f"invalid required level for {identifier}")
        row = rows.get(identifier)
        status = "not-configured"
        if row is not None:
            if not isinstance(row, dict) or set(row) != {"status", "level", "reference", "artifact_sha256"}:
                raise GateFailure(f"invalid evidence fields for {identifier}")
            if row["status"] != "passed":
                status = "failed"
            else:
                if row["level"] not in LEVELS or LEVELS[row["level"]] < LEVELS[required_level]:
                    raise GateFailure(f"insufficient evidence level for {identifier}")
                reference = Path(row["reference"])
                if reference.is_absolute() or ".." in reference.parts:
                    raise GateFailure(f"invalid reference for {identifier}")
                artifact = root / reference
                if not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["artifact_sha256"]:
                    raise GateFailure(f"missing or mismatched artifact for {identifier}")
                payload = json.loads(artifact.read_text(encoding="utf-8"))
                expected = {
                    "schema": "hyphae-native-g4-receipt-audit-v1", "status": "passed",
                    "requirement": identifier, "source_commit": expected_commit,
                    "suite_manifest_sha256": suite_digest, "corpus_manifest_sha256": corpus_digest,
                    "scope": "bounded-correctness", "production_scale": False,
                    "claims": [], "closure_declared": False,
                }
                if any(payload.get(key) != value for key, value in expected.items()):
                    raise GateFailure(f"invalid audit identity or scope for {identifier}")
                if not isinstance(payload.get("corpora"), list) or not payload["corpora"]:
                    raise GateFailure(f"missing corpus binding for {identifier}")
                for count in ("suite_count", "test_count"):
                    if not isinstance(payload.get(count), int) or isinstance(payload[count], bool) or payload[count] <= 0:
                        raise GateFailure(f"invalid {count} for {identifier}")
                status = "passed"
        results.append({"id": identifier, "status": status, "required_evidence": required_level})
    passed = sum(row["status"] == "passed" for row in results)
    return {
        "schema": "hyphae-native-g4-readiness-v1", "gate": "G4",
        "status": "ready" if passed == 12 else "not-ready", "required": 12,
        "passed": passed, "requirements": results, "claims": [], "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--corpus-manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = evaluate(args.root, json.loads(args.profile.read_text(encoding="utf-8")), json.loads(args.evidence.read_text(encoding="utf-8")), args.expected_commit, args.suite_manifest_sha256, args.corpus_manifest_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G4 readiness failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if result["status"] == "ready" else 1


if __name__ == "__main__":
    raise SystemExit(main())
