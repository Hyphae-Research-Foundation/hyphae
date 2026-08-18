#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Inject exact-SHA G6 audits into derived, still-open readiness evidence."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g6_foundation import GateFailure
from tools.check_native_g6_manifests import MANIFEST_NAMES

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
BASELINE_FIELDS = {"schema", "gate", "predecessor", "evidence", "claims", "closure_declared"}


def _artifact(root: Path, reference: Path) -> Path:
    if reference.is_absolute() or not reference.parts or ".." in reference.parts:
        raise GateFailure("invalid G6 evidence reference")
    resolved_root = root.resolve()
    artifact = (root / reference).resolve()
    try:
        artifact.relative_to(resolved_root)
    except ValueError as error:
        raise GateFailure("G6 evidence reference escapes the root") from error
    if not artifact.is_file():
        raise GateFailure("missing G6 evidence audit")
    return artifact


def inject(
    root: Path,
    baseline: dict[str, Any],
    kind: str,
    reference: Path,
    expected_commit: str,
    manifest_sha256: dict[str, str],
    requirement: str | None = None,
    platform: str | None = None,
) -> dict[str, Any]:
    if (
        set(baseline) != BASELINE_FIELDS
        or baseline.get("schema") != "hyphae-native-g6-readiness-evidence-v1"
        or baseline.get("gate") != "G6"
        or not isinstance(baseline.get("evidence"), dict)
        or baseline.get("claims") != []
        or baseline.get("closure_declared") is not False
    ):
        raise GateFailure("unsupported or claiming G6 evidence baseline")
    if (
        HEX40.fullmatch(expected_commit) is None
        or set(manifest_sha256) != set(MANIFEST_NAMES)
        or any(HEX64.fullmatch(value) is None for value in manifest_sha256.values())
    ):
        raise GateFailure("invalid G6 exact identities")
    artifact = _artifact(root, reference)
    raw = artifact.read_bytes()
    payload = json.loads(raw)
    if not isinstance(payload, dict) or payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("G6 audit makes a claim or declares closure")
    if payload.get("source_commit") != expected_commit or payload.get("manifest_sha256") != manifest_sha256:
        raise GateFailure("G6 audit exact identity mismatch")
    result = copy.deepcopy(baseline)
    row = {
        "status": "passed",
        "level": "hosted",
        "reference": reference.as_posix(),
        "artifact_sha256": hashlib.sha256(raw).hexdigest(),
    }
    if kind == "predecessor":
        if (
            requirement is not None
            or result.get("predecessor") is not None
            or payload.get("schema") != "hyphae-native-g6-manifest-audit-v1"
            or payload.get("gate") != "G6"
            or payload.get("status") != "passed"
            or payload.get("predecessor_count") != 6
        ):
            raise GateFailure("invalid or duplicate G6 predecessor audit")
        row["level"] = "retained"
        result["predecessor"] = row
    elif kind == "requirement":
        if (
            not isinstance(requirement, str)
            or not requirement
            or not isinstance(platform, str)
            or platform not in {"linux", "macos", "windows"}
            or not isinstance(result["evidence"].get(requirement, {}), dict)
            or platform in result["evidence"].get(requirement, {})
            or payload.get("schema") != "hyphae-native-g6-receipt-audit-v1"
            or payload.get("gate") != "G6"
            or payload.get("status") != "passed"
            or payload.get("requirement") != requirement
            or payload.get("platform") != platform
        ):
            raise GateFailure("invalid or duplicate G6 requirement audit")
        result["evidence"].setdefault(requirement, {})[platform] = row
    else:
        raise GateFailure("unknown G6 evidence kind")
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--kind", choices=("predecessor", "requirement"), required=True)
    parser.add_argument("--requirement")
    parser.add_argument("--platform", choices=("linux", "macos", "windows"))
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    for name in MANIFEST_NAMES:
        parser.add_argument(f"--{name}-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        digests = {name: getattr(args, f"{name}_sha256") for name in MANIFEST_NAMES}
        result = inject(
            args.root,
            json.loads(args.baseline.read_text(encoding="utf-8")),
            args.kind,
            args.reference,
            args.expected_commit,
            digests,
            args.requirement,
            args.platform,
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G6 evidence injection failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
