#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Build one semantic exact-SHA G2 receipt from Cargo test logs."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

REQUIREMENTS = {
    "native-ddl-dml-and-constraints",
    "transactions-and-isolation",
    "indexes-joins-ctes-windows",
    "prepared-plans-and-explain",
    "sqllogictest-conformance",
    "metamorphic-sql-equivalence",
    "tpch-correctness",
    "tpcc-acid",
}
HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")
TEST_RESULT = re.compile(r"test result: ok\. ([0-9]+) passed; 0 failed;")


class GateFailure(ValueError):
    pass


def parse_test_count(output: str) -> int:
    matches = TEST_RESULT.findall(output)
    if len(matches) != 1:
        raise GateFailure("test output does not contain one exact successful result")
    count = int(matches[0])
    if count <= 0:
        raise GateFailure("test output contains zero passing tests")
    return count


def build_receipt(
    source_commit: str,
    requirement: str,
    suites: list[tuple[str, str]],
    corpus_sha256: str,
) -> dict[str, Any]:
    if not HEX40.fullmatch(source_commit) or requirement not in REQUIREMENTS:
        raise GateFailure("commit or requirement is invalid")
    if not HEX64.fullmatch(corpus_sha256):
        raise GateFailure("corpus digest is invalid")
    names = [name for name, _ in suites]
    if not names or len(names) != len(set(names)) or any(not name for name in names):
        raise GateFailure("suite set is invalid")
    return {
        "schema": "hyphae-native-g2-receipt-v1",
        "status": "passed",
        "source_commit": source_commit,
        "requirement": requirement,
        "test_suites": names,
        "test_count": sum(parse_test_count(output) for _, output in suites),
        "corpus_sha256": corpus_sha256,
        "scope": "bounded-correctness",
        "production_scale": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--suite-log", action="append", nargs=2, metavar=("NAME", "PATH"), required=True)
    parser.add_argument("--corpus-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        receipt = build_receipt(
            args.source_commit,
            args.requirement,
            [(name, Path(path).read_text(encoding="utf-8")) for name, path in args.suite_log],
            args.corpus_sha256,
        )
    except (GateFailure, OSError) as error:
        print(f"native G2 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
