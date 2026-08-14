#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Inject one validated exact-SHA G3 audit into a derived evidence map."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path
from typing import Any


class GateFailure(ValueError):
    pass


def inject(root: Path, baseline: dict[str, Any], requirement: str, reference: Path, expected_commit: str) -> dict[str, Any]:
    if baseline.get("schema") != "hyphae-native-g3-readiness-evidence-v1" or baseline.get("gate") != "G3":
        raise GateFailure("unsupported baseline")
    if reference.is_absolute() or ".." in reference.parts:
        raise GateFailure("invalid reference")
    artifact = root / reference
    if not artifact.is_file():
        raise GateFailure("missing audit")
    payload = json.loads(artifact.read_text(encoding="utf-8"))
    if payload.get("schema") != "hyphae-native-g3-receipt-audit-v2" or payload.get("status") != "passed":
        raise GateFailure("invalid audit")
    if payload.get("source_commit") != expected_commit or payload.get("requirement") != requirement:
        raise GateFailure("audit identity mismatch")
    result = copy.deepcopy(baseline)
    evidence = result.setdefault("evidence", {})
    if requirement in evidence:
        raise GateFailure("requirement already injected")
    evidence[requirement] = {
        "status": "passed",
        "level": "hosted",
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
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = inject(args.root, json.loads(args.baseline.read_text(encoding="utf-8")), args.requirement, args.reference, args.expected_commit)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G3 evidence injection failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
