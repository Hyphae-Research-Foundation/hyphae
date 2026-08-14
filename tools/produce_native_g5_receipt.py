#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Produce an exact-SHA G5 supporting receipt from authorized real-suite logs."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
TEST_RESULT = re.compile(r"test result: ok\. ([0-9]+) passed; 0 failed;")


class GateFailure(ValueError):
    pass


def build_receipt(source_commit: str, requirement: str, suite_raw: bytes, suite_sha256: str, authority_sha256: str, workload_sha256: str, predecessor_audit_sha256: str, platform: str, toolchain: str, logs: list[tuple[str, bytes]]) -> dict:
    if not HEX40.fullmatch(source_commit) or any(not HEX64.fullmatch(value) for value in (suite_sha256, authority_sha256, workload_sha256, predecessor_audit_sha256)):
        raise GateFailure("exact identities are invalid")
    if hashlib.sha256(suite_raw).hexdigest() != suite_sha256 or not platform or not toolchain:
        raise GateFailure("suite digest or execution identity mismatch")
    manifest = json.loads(suite_raw)
    if manifest.get("schema") != "hyphae-native-g5-suite-manifest-v1" or manifest.get("gate") != "G5" or manifest.get("claims") != [] or manifest.get("closure_declared") is not False:
        raise GateFailure("unsupported or claiming suite manifest")
    matches = [row for row in manifest.get("requirements", []) if row.get("id") == requirement]
    if len(matches) != 1:
        raise GateFailure("requirement is absent or duplicated")
    row = matches[0]
    expected = {item.get("name"): item.get("command") for item in row.get("suites", [])}
    supplied = {name: raw for name, raw in logs}
    if not expected or None in expected or len(supplied) != len(logs) or set(supplied) != set(expected):
        raise GateFailure("logs do not exactly match authorized suites")
    suites, total = [], 0
    for name in sorted(expected):
        raw = supplied[name]
        text = raw.decode("utf-8")
        marker = "G5_COMMAND: " + json.dumps(expected[name], separators=(",", ":"))
        counts = [int(value) for value in TEST_RESULT.findall(text)]
        if marker not in text or "test result: FAILED" in text or not counts or any(value <= 0 for value in counts):
            raise GateFailure(f"suite {name} has no positive authorized result")
        count = sum(counts)
        total += count
        suites.append({"name": name, "test_count": count, "log_sha256": hashlib.sha256(raw).hexdigest()})
    return {"schema": "hyphae-native-g5-receipt-v1", "gate": "G5", "status": "passed", "evidence_class": "supporting-not-closure", "requirement": requirement, "source_commit": source_commit, "suite_manifest_sha256": suite_sha256, "authority_manifest_sha256": authority_sha256, "workload_manifest_sha256": workload_sha256, "predecessor_audit_sha256": predecessor_audit_sha256, "workloads": row["workloads"], "platform": platform, "toolchain": toolchain, "suites": suites, "test_count": total, "claims": [], "closure_declared": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--suite-manifest", type=Path, required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--authority-manifest-sha256", required=True)
    parser.add_argument("--workload-manifest-sha256", required=True)
    parser.add_argument("--predecessor-audit-sha256", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--suite-log", action="append", nargs=2, metavar=("NAME", "PATH"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = build_receipt(args.source_commit, args.requirement, args.suite_manifest.read_bytes(), args.suite_manifest_sha256, args.authority_manifest_sha256, args.workload_manifest_sha256, args.predecessor_audit_sha256, args.platform, args.toolchain, [(name, Path(path).read_bytes()) for name, path in args.suite_log])
    except (GateFailure, OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"native G5 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
