#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the hosted G0 architecture and versioned specification set."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-specification-profile-v1"
RECEIPT_SCHEMA = "hyphae-native-specification-receipt-v1"
REQUIRED_SPECS = {
    "types-v1.md",
    "page-row-blob-format-v1.md",
    "wal-format-v1.md",
    "mvcc-commit-v1.md",
    "catalog-v1.md",
    "sql-semantics-v1.md",
    "structures-semantics-v1.md",
    "search-semantics-v1.md",
    "ann-semantics-v1.md",
    "local-protocol-v1.md",
}


class GateFailure(RuntimeError):
    """The architecture/specification profile is incomplete or inconsistent."""


def _leading_heading(text: str) -> str | None:
    lines = iter(text.splitlines())
    in_comment = False
    for line in lines:
        stripped = line.strip()
        if in_comment:
            if "-->" in stripped:
                if stripped.split("-->", 1)[1].strip():
                    return None
                in_comment = False
            continue
        if not stripped:
            continue
        if stripped.startswith("<!--"):
            if "-->" in stripped and stripped.split("-->", 1)[1].strip():
                return None
            in_comment = "-->" not in stripped
            continue
        return line
    return None


def validate_profile(root: Path, profile: dict[str, Any]) -> dict[str, Any]:
    if set(profile) != {"schema", "architecture", "specifications"} or profile.get("schema") != SCHEMA:
        raise GateFailure("unsupported specification profile")
    architecture = profile.get("architecture")
    specs = profile.get("specifications")
    if not isinstance(architecture, str) or Path(architecture).is_absolute():
        raise GateFailure("architecture must be repository-relative")
    if not isinstance(specs, list) or len(specs) != len(set(specs)):
        raise GateFailure("specifications must be a unique array")
    if {Path(value).name for value in specs if isinstance(value, str)} != REQUIRED_SPECS:
        raise GateFailure("specification profile must contain the exact G0 contract set")
    paths = [architecture, *specs]
    digests: dict[str, str] = {}
    for value in paths:
        path = Path(value)
        if path.is_absolute() or ".." in path.parts:
            raise GateFailure("specification path escapes repository")
        resolved = root / path
        if not resolved.is_file():
            raise GateFailure(f"specification is missing: {value}")
        text = resolved.read_text(encoding="utf-8")
        heading = _leading_heading(text)
        if (
            heading is None
            or not heading.startswith("# ")
            or (value != architecture and re.search(r"\bv1\b", heading, re.IGNORECASE) is None)
        ):
            raise GateFailure(f"specification lacks versioned heading: {value}")
        if value == architecture and "Status: accepted target architecture" not in text:
            raise GateFailure("architecture is not explicitly accepted")
        digests[value] = hashlib.sha256(resolved.read_bytes()).hexdigest()
    return {"documents": digests, "document_count": len(digests)}


def build_receipt(root: Path, profile_path: Path) -> dict[str, Any]:
    profile_bytes = profile_path.read_bytes()
    summary = validate_profile(root, json.loads(profile_bytes))
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "source_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=root, text=True
        ).strip(),
        "profile_sha256": hashlib.sha256(profile_bytes).hexdigest(),
        **summary,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    profile = args.profile if args.profile.is_absolute() else root / args.profile
    try:
        receipt = build_receipt(root, profile)
    except (OSError, json.JSONDecodeError, subprocess.CalledProcessError, GateFailure) as error:
        print(f"native specification gate failed: {error}")
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
