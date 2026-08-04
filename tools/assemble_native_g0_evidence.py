#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Assemble the exact eight-row native G0 evidence chain."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from tools.check_native_g0_readiness import GateFailure, inject_evidence

INJECTIONS = [
    ("architecture-and-versioned-specifications", "hosted", "native-specification-receipt.json"),
    ("canonical-type-goldens-and-properties", "hosted", "native-types-audit.json"),
    ("page-row-blob-wal-mvcc-goldens", "hosted", "native-golden-audit.json"),
    ("sql-structure-search-ann-contracts", "hosted", "native-contract-conformance.json"),
    ("local-protocol-goldens-and-conformance", "hosted", "native-conformance-aggregate.json"),
    ("benchmark-and-quality-corpus", "hosted", "native-quality-aggregate.json"),
    ("native-dependency-license-unsafe-audit", "external-governance", "native-dependency-audit.json"),
    ("clean-room-porting-ledger-review", "external-governance", "native-clean-room-receipt.json"),
]


def validate_receipt_commit_identity(root: Path, expected_commit: str) -> None:
    """Reject any receipt whose explicit source commit differs from the target."""

    for _, _, artifact in INJECTIONS:
        path = root / artifact
        if not path.is_file():
            raise GateFailure(f"injected artifact is missing: {artifact}")
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise GateFailure(f"receipt must be valid JSON: {artifact}") from error
        actual = payload.get("source_commit")
        if actual is not None and actual != expected_commit:
            raise GateFailure(
                f"receipt source commit mismatch for {artifact}: {actual} != {expected_commit}"
            )


def assemble(root: Path, profile: dict, evidence: dict) -> dict:
    """Inject all exact artifacts or fail closed at the first absent/red input."""

    current = evidence
    for requirement, level, artifact in INJECTIONS:
        current = inject_evidence(root, current, profile, requirement, level, artifact)
    return current


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        profile = json.loads(args.profile.read_text(encoding="utf-8"))
        evidence = json.loads(args.evidence.read_text(encoding="utf-8"))
        validate_receipt_commit_identity(args.root, args.expected_commit)
        result = assemble(args.root, profile, evidence)
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native G0 evidence assembly failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
