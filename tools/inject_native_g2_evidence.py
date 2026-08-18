#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Inject one semantically validated exact-SHA audit into a derived G2 map."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path
from typing import Any


class GateFailure(ValueError):
    pass


def inject(
    root: Path,
    baseline: dict[str, Any],
    requirement: str,
    reference: Path,
    level: str,
    expected_commit: str,
) -> dict[str, Any]:
    if baseline.get("schema") != "hyphae-native-g2-readiness-evidence-v1" or baseline.get("gate") != "G2":
        raise GateFailure("unsupported baseline")
    if requirement not in {
        "native-ddl-dml-and-constraints",
        "transactions-and-isolation",
        "indexes-joins-ctes-windows",
        "prepared-plans-and-explain",
        "sqllogictest-conformance",
        "metamorphic-sql-equivalence",
        "tpch-correctness",
        "tpcc-acid",
    }:
        raise GateFailure("unknown requirement")
    if level != "hosted":
        raise GateFailure("G2 injection requires hosted evidence")
    if reference.is_absolute() or ".." in reference.parts:
        raise GateFailure("invalid reference")
    artifact = root / reference
    if not artifact.is_file():
        raise GateFailure("missing artifact")
    payload = json.loads(artifact.read_text(encoding="utf-8"))
    if payload.get("schema") != "hyphae-native-g2-receipt-audit-v1":
        raise GateFailure("invalid audit schema")
    if payload.get("status") != "passed":
        raise GateFailure("audit is not passed")
    if payload.get("source_commit") != expected_commit:
        raise GateFailure("audit commit mismatch")
    if payload.get("requirement") != requirement:
        raise GateFailure("audit requirement mismatch")
    result = copy.deepcopy(baseline)
    evidence = result.setdefault("evidence", {})
    if requirement in evidence:
        raise GateFailure("requirement already injected")
    evidence[requirement] = {
        "status": "passed",
        "level": level,
        "reference": reference.as_posix(),
        "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
    }
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--level", default="hosted")
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = inject(
            args.root,
            json.loads(args.baseline.read_text(encoding="utf-8")),
            args.requirement,
            args.reference,
            args.level,
            args.expected_commit,
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G2 evidence injection failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
