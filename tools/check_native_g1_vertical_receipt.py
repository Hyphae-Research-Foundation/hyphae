#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Semantic validation for the G1 three-engine minimal vertical."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

TEST_GROUPS = {
    "sql-primary-key-insert-read",
    "structure-ttl-point-read",
    "lexical-match",
    "all-engine-single-csn",
}
ENGINES = {"relational", "structure", "search"}


class GateFailure(ValueError):
    pass


def validate_receipt(receipt: dict[str, Any], expected_commit: str) -> dict[str, Any]:
    if receipt.get("schema") != "hyphae-native-g1-vertical-v1":
        raise GateFailure("unsupported vertical receipt")
    if receipt.get("status") != "passed":
        raise GateFailure("vertical receipt did not pass")
    if receipt.get("source_commit") != expected_commit:
        raise GateFailure("vertical source commit mismatch")
    tests = receipt.get("tests")
    if not isinstance(tests, dict) or set(tests) != TEST_GROUPS:
        raise GateFailure("vertical test group set mismatch")
    for name, result in tests.items():
        if not isinstance(result, dict) or result.get("status") != "passed":
            raise GateFailure(f"vertical test group {name} did not pass")
        count = result.get("test_count")
        if not isinstance(count, int) or count <= 0:
            raise GateFailure(f"vertical test count is invalid for {name}")
    engines = receipt.get("engines")
    if not isinstance(engines, list) or set(engines) != ENGINES or len(engines) != 3:
        raise GateFailure("vertical engine set mismatch")
    if receipt.get("single_csn") != 1:
        raise GateFailure("vertical did not commit under the single CSN fixture")
    if receipt.get("reopen_equivalent") is not True:
        raise GateFailure("vertical reopen equivalence is not proven")
    return {
        "schema": "hyphae-native-g1-vertical-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "test_groups": len(tests),
        "test_count": sum(result["test_count"] for result in tests.values()),
        "engines": len(engines),
        "single_csn": 1,
        "reopen_equivalent": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate_receipt(
            json.loads(args.receipt.read_text(encoding="utf-8")), args.expected_commit
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G1 vertical receipt failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
