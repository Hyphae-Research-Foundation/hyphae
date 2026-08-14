#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Evaluate G5 candidate evidence without declaring gate closure."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def _artifact(root: Path, row: dict, label: str) -> dict:
    if not isinstance(row, dict) or set(row) != {"status", "level", "reference", "artifact_sha256"} or row["status"] != "passed" or not HEX64.fullmatch(row.get("artifact_sha256", "")):
        raise GateFailure(f"invalid {label} evidence fields")
    reference = Path(row["reference"])
    artifact = root / reference
    if reference.is_absolute() or ".." in reference.parts or not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["artifact_sha256"]:
        raise GateFailure(f"missing or mismatched {label} artifact")
    return json.loads(artifact.read_text(encoding="utf-8"))


def evaluate(root: Path, profile: dict, evidence: dict, expected_commit: str, suite_digest: str, authority_digest: str, workload_digest: str, predecessor_digest: str) -> dict:
    if profile.get("schema") != "hyphae-native-g5-readiness-profile-v1" or profile.get("gate") != "G5" or evidence.get("schema") != "hyphae-native-g5-readiness-evidence-v1" or evidence.get("gate") != "G5":
        raise GateFailure("unsupported G5 profile or evidence")
    if profile.get("claims") != [] or profile.get("closure_declared") is not False or evidence.get("claims") != [] or evidence.get("closure_declared") is not False:
        raise GateFailure("G5 inputs must remain open and claim-free")
    if not HEX40.fullmatch(expected_commit) or any(not HEX64.fullmatch(value) for value in (suite_digest, authority_digest, workload_digest, predecessor_digest)):
        raise GateFailure("invalid exact identities")
    requirements = profile.get("requirements")
    rows = evidence.get("evidence")
    ids = [row.get("id") for row in requirements] if isinstance(requirements, list) else []
    if len(ids) != 8 or len(set(ids)) != 8 or not isinstance(rows, dict) or set(rows) - set(ids):
        raise GateFailure("G5 requires exactly eight known requirements")
    predecessor_status = "not-configured"
    predecessors_required = 3
    predecessors_passed = 0
    if evidence.get("predecessor") is not None:
        payload = _artifact(root, evidence["predecessor"], "predecessor")
        if evidence["predecessor"]["level"] != "retained" or payload.get("schema") != "hyphae-native-g5-predecessor-audit-v1" or payload.get("status") != "passed" or payload.get("manifest_sha256") != predecessor_digest or payload.get("claims") != [] or payload.get("closure_declared") is not False:
            raise GateFailure("invalid predecessor audit")
        predecessor_status = "passed"
        predecessors = payload.get("predecessors")
        if not isinstance(predecessors, list) or len(predecessors) != predecessors_required:
            raise GateFailure("invalid predecessor cardinality")
        predecessors_passed = len(predecessors)
    results = []
    for requirement in requirements:
        identifier, status = requirement["id"], "not-configured"
        if requirement.get("required_evidence") != "hosted":
            raise GateFailure(f"invalid evidence level for {identifier}")
        if identifier in rows:
            payload = _artifact(root, rows[identifier], identifier)
            expected = {"schema": "hyphae-native-g5-receipt-audit-v1", "gate": "G5", "status": "passed", "evidence_class": "supporting-not-closure", "source_commit": expected_commit, "requirement": identifier, "suite_manifest_sha256": suite_digest, "authority_manifest_sha256": authority_digest, "workload_manifest_sha256": workload_digest, "predecessor_audit_sha256": predecessor_digest, "claims": [], "closure_declared": False}
            if rows[identifier]["level"] != "hosted" or any(payload.get(key) != value for key, value in expected.items()):
                raise GateFailure(f"invalid audit identity for {identifier}")
            if payload.get("suite_count", 0) <= 0 or payload.get("test_count", 0) <= 0 or not isinstance(payload.get("workloads"), list) or len(payload["workloads"]) != 1:
                raise GateFailure(f"incomplete audit for {identifier}")
            status = "passed"
        results.append({"id": identifier, "status": status, "required_evidence": "hosted"})
    passed = sum(row["status"] == "passed" for row in results)
    candidate = predecessor_status == "passed" and passed == 8
    return {"schema": "hyphae-native-g5-readiness-v1", "gate": "G5", "status": "candidate-evidence-complete" if candidate else "not-ready", "predecessor_status": predecessor_status, "predecessors_required": predecessors_required, "predecessors_passed": predecessors_passed, "required": 8, "passed": passed, "requirements": results, "closure_status": "open", "claims": [], "closure_declared": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--authority-manifest-sha256", required=True)
    parser.add_argument("--workload-manifest-sha256", required=True)
    parser.add_argument("--predecessor-manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = evaluate(args.root, json.loads(args.profile.read_text(encoding="utf-8")), json.loads(args.evidence.read_text(encoding="utf-8")), args.expected_commit, args.suite_manifest_sha256, args.authority_manifest_sha256, args.workload_manifest_sha256, args.predecessor_manifest_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G5 readiness failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if result["status"] == "candidate-evidence-complete" else 1


if __name__ == "__main__":
    raise SystemExit(main())
