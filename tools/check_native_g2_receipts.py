#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Semantic validator for one bounded, exact-commit G2 receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-g2-receipt-v1"
FIELDS = {
    "schema",
    "status",
    "source_commit",
    "requirement",
    "test_suites",
    "test_count",
    "corpus_sha256",
    "scope",
    "production_scale",
}
HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")


class GateFailure(ValueError):
    pass


def validate_receipt(payload: dict[str, Any], expected_commit: str, expected_requirement: str) -> dict[str, Any]:
    if set(payload) != FIELDS:
        raise GateFailure("receipt fields mismatch")
    if payload.get("schema") != SCHEMA or payload.get("status") != "passed":
        raise GateFailure("unsupported or unpassed receipt")
    commit = payload.get("source_commit")
    if not isinstance(commit, str) or not HEX40.fullmatch(commit) or commit != expected_commit:
        raise GateFailure("source commit mismatch")
    if payload.get("requirement") != expected_requirement:
        raise GateFailure("requirement mismatch")
    suites = payload.get("test_suites")
    if not isinstance(suites, list) or not suites or not all(isinstance(item, str) and item for item in suites):
        raise GateFailure("test suite set is empty or invalid")
    if len(suites) != len(set(suites)):
        raise GateFailure("test suite set contains duplicates")
    count = payload.get("test_count")
    if not isinstance(count, int) or isinstance(count, bool) or count <= 0:
        raise GateFailure("test count must be positive")
    digest = payload.get("corpus_sha256")
    if not isinstance(digest, str) or not HEX64.fullmatch(digest):
        raise GateFailure("corpus digest is invalid")
    if payload.get("scope") != "bounded-correctness":
        raise GateFailure("receipt scope is not bounded correctness")
    if payload.get("production_scale") is not False:
        raise GateFailure("G2 receipt must not claim production scale")
    return {
        "schema": "hyphae-native-g2-receipt-audit-v1",
        "status": "passed",
        "source_commit": commit,
        "requirement": expected_requirement,
        "test_count": count,
        "suite_count": len(suites),
        "corpus_sha256": digest,
        "scope": "bounded-correctness",
        "production_scale": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-requirement", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate_receipt(
            json.loads(args.receipt.read_text(encoding="utf-8")),
            args.expected_commit,
            args.expected_requirement,
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G2 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
