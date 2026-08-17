#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Inject predecessor or requirement audits into derived, still-open G5 evidence."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
from pathlib import Path

HEX40 = re.compile(r"[0-9a-f]{40}\Z")


class GateFailure(ValueError):
    pass


def inject(root: Path, baseline: dict, kind: str, reference: Path, expected_commit: str, requirement: str | None = None) -> dict:
    if baseline.get("schema") != "hyphae-native-g5-readiness-evidence-v1" or baseline.get("gate") != "G5" or baseline.get("claims") != [] or baseline.get("closure_declared") is not False:
        raise GateFailure("unsupported or claiming baseline")
    if not HEX40.fullmatch(expected_commit) or reference.is_absolute() or ".." in reference.parts:
        raise GateFailure("invalid commit or reference")
    artifact = root / reference
    if not artifact.is_file():
        raise GateFailure("missing audit")
    payload = json.loads(artifact.read_text(encoding="utf-8"))
    result = copy.deepcopy(baseline)
    row = {"status": "passed", "level": "hosted", "reference": reference.as_posix(), "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest()}
    if kind == "predecessor":
        if requirement is not None or result.get("predecessor") is not None or payload.get("schema") != "hyphae-native-g5-predecessor-audit-v1" or payload.get("status") != "passed":
            raise GateFailure("invalid or duplicate predecessor audit")
        row["level"] = "retained"
        result["predecessor"] = row
    elif kind == "requirement":
        evidence = result.get("evidence")
        if not requirement or not isinstance(evidence, dict) or requirement in evidence or payload.get("schema") != "hyphae-native-g5-receipt-audit-v1" or payload.get("source_commit") != expected_commit or payload.get("requirement") != requirement:
            raise GateFailure("invalid or duplicate requirement audit")
        evidence[requirement] = row
    else:
        raise GateFailure("unknown evidence kind")
    if payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("audit makes a claim or declares closure")
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--kind", choices=("predecessor", "requirement"), required=True)
    parser.add_argument("--requirement")
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = inject(args.root, json.loads(args.baseline.read_text(encoding="utf-8")), args.kind, args.reference, args.expected_commit, args.requirement)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G5 evidence injection failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
