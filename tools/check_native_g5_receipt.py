#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed validation for one exact-SHA G5 supporting receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def validate(payload: dict, commit: str, requirement: str, suite_digest: str, authority_digest: str, workload_digest: str, predecessor_digest: str) -> dict:
    expected = {"schema": "hyphae-native-g5-receipt-v1", "gate": "G5", "status": "passed", "evidence_class": "supporting-not-closure", "source_commit": commit, "requirement": requirement, "suite_manifest_sha256": suite_digest, "authority_manifest_sha256": authority_digest, "workload_manifest_sha256": workload_digest, "predecessor_audit_sha256": predecessor_digest, "claims": [], "closure_declared": False}
    if not HEX40.fullmatch(commit) or any(not HEX64.fullmatch(value) for value in (suite_digest, authority_digest, workload_digest, predecessor_digest)) or any(payload.get(key) != value for key, value in expected.items()):
        raise GateFailure("receipt identity, authority, or open-state mismatch")
    workloads = payload.get("workloads")
    suites = payload.get("suites")
    if not isinstance(workloads, list) or len(workloads) != 1 or not workloads[0] or not isinstance(suites, list) or not suites:
        raise GateFailure("workload or suite evidence is missing")
    count, names = 0, set()
    for suite in suites:
        if not isinstance(suite, dict) or set(suite) != {"name", "test_count", "log_sha256"}:
            raise GateFailure("suite audit fields mismatch")
        name, tests, digest = suite["name"], suite["test_count"], suite["log_sha256"]
        if not isinstance(name, str) or not name or name in names or not isinstance(tests, int) or isinstance(tests, bool) or tests <= 0 or not isinstance(digest, str) or not HEX64.fullmatch(digest):
            raise GateFailure("invalid suite evidence")
        names.add(name)
        count += tests
    if payload.get("test_count") != count:
        raise GateFailure("aggregate test count mismatch")
    return {"schema": "hyphae-native-g5-receipt-audit-v1", "gate": "G5", "status": "passed", "evidence_class": "supporting-not-closure", "source_commit": commit, "requirement": requirement, "suite_manifest_sha256": suite_digest, "authority_manifest_sha256": authority_digest, "workload_manifest_sha256": workload_digest, "predecessor_audit_sha256": predecessor_digest, "workloads": workloads, "suite_count": len(suites), "test_count": count, "claims": [], "closure_declared": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-requirement", required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--authority-manifest-sha256", required=True)
    parser.add_argument("--workload-manifest-sha256", required=True)
    parser.add_argument("--predecessor-audit-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate(json.loads(args.receipt.read_text(encoding="utf-8")), args.expected_commit, args.expected_requirement, args.suite_manifest_sha256, args.authority_manifest_sha256, args.workload_manifest_sha256, args.predecessor_audit_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G5 receipt audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
