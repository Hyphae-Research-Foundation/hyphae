#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Validate one suite- and corpus-bound exact-SHA G4 receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def validate(payload: dict[str, Any], commit: str, requirement: str, suite_digest: str, corpus_digest: str) -> dict[str, Any]:
    if payload.get("schema") != "hyphae-native-g4-receipt-v1" or payload.get("status") != "passed":
        raise GateFailure("unsupported or unpassed receipt")
    if HEX40.fullmatch(commit) is None or payload.get("source_commit") != commit:
        raise GateFailure("source commit mismatch")
    if payload.get("requirement") != requirement:
        raise GateFailure("requirement mismatch")
    for field, expected in (("suite_manifest_sha256", suite_digest), ("corpus_manifest_sha256", corpus_digest)):
        if HEX64.fullmatch(expected) is None or payload.get(field) != expected:
            raise GateFailure(f"{field} mismatch")
    if payload.get("scope") != "bounded-correctness" or payload.get("production_scale") is not False:
        raise GateFailure("receipt scope is invalid")
    if payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("receipt must not make claims or declare closure")
    if not isinstance(payload.get("platform"), str) or not payload["platform"] or not isinstance(payload.get("toolchain"), str) or not payload["toolchain"]:
        raise GateFailure("execution identity is missing")
    corpora = payload.get("corpora")
    if not isinstance(corpora, list) or not corpora or len(corpora) != len(set(corpora)):
        raise GateFailure("corpus identities are invalid")
    suites = payload.get("suites")
    if not isinstance(suites, list) or not suites:
        raise GateFailure("suite audits are missing")
    names: set[str] = set()
    count = 0
    for suite in suites:
        if not isinstance(suite, dict) or set(suite) != {"name", "test_count", "log_sha256"}:
            raise GateFailure("suite audit fields mismatch")
        name = suite["name"]
        tests = suite["test_count"]
        if not isinstance(name, str) or not name or name in names or not isinstance(tests, int) or isinstance(tests, bool) or tests <= 0:
            raise GateFailure("suite audit identity or count is invalid")
        if not isinstance(suite["log_sha256"], str) or HEX64.fullmatch(suite["log_sha256"]) is None:
            raise GateFailure("suite log digest is invalid")
        names.add(name)
        count += tests
    if payload.get("test_count") != count:
        raise GateFailure("aggregate test count mismatch")
    return {
        "schema": "hyphae-native-g4-receipt-audit-v1", "status": "passed",
        "source_commit": commit, "requirement": requirement,
        "suite_manifest_sha256": suite_digest, "corpus_manifest_sha256": corpus_digest,
        "corpora": corpora, "platform": payload["platform"], "toolchain": payload["toolchain"],
        "suite_count": len(suites), "test_count": count, "scope": "bounded-correctness",
        "production_scale": False, "claims": [], "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-requirement", required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--corpus-manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate(json.loads(args.receipt.read_text(encoding="utf-8")), args.expected_commit, args.expected_requirement, args.suite_manifest_sha256, args.corpus_manifest_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G4 receipt audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
