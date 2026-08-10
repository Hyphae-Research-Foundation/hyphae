#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Inject one validated exact-SHA G4 audit into derived evidence."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")


class GateFailure(ValueError):
    pass


def inject(root: Path, baseline: dict[str, Any], requirement: str, reference: Path, expected_commit: str) -> dict[str, Any]:
    if baseline.get("schema") != "hyphae-native-g4-readiness-evidence-v1" or baseline.get("gate") != "G4":
        raise GateFailure("unsupported baseline")
    if baseline.get("claims") != [] or baseline.get("closure_declared") is not False:
        raise GateFailure("baseline must not make claims or declare closure")
    if HEX40.fullmatch(expected_commit) is None or reference.is_absolute() or ".." in reference.parts:
        raise GateFailure("invalid commit or reference")
    artifact = root / reference
    if not artifact.is_file():
        raise GateFailure("missing audit")
    payload = json.loads(artifact.read_text(encoding="utf-8"))
    if payload.get("schema") != "hyphae-native-g4-receipt-audit-v1" or payload.get("status") != "passed":
        raise GateFailure("invalid audit")
    if payload.get("source_commit") != expected_commit or payload.get("requirement") != requirement:
        raise GateFailure("audit identity mismatch")
    if payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("audit makes a claim or declares closure")
    result = copy.deepcopy(baseline)
    evidence = result.get("evidence")
    if not isinstance(evidence, dict) or requirement in evidence:
        raise GateFailure("invalid evidence map or duplicate requirement")
    evidence[requirement] = {
        "status": "passed", "level": "hosted", "reference": reference.as_posix(),
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
        print(f"native G4 evidence injection failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
