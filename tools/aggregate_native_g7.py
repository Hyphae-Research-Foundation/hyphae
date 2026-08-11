#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Aggregate one exact-SHA G7 matrix from supported dedicated hardware."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from tools.check_native_g7_matrix import GateFailure, validate_matrix


SUPPORTED_PLATFORMS = ("linux", "darwin")


def aggregate(
    root: Path,
    source_commit: str,
    *,
    expected_tree: str | None = None,
) -> dict:
    platforms = tuple(
        platform
        for platform in SUPPORTED_PLATFORMS
        if (root / platform / "native-g7-matrix.json").is_file()
    )
    if len(platforms) != 1:
        raise GateFailure("G7 aggregate requires exactly one supported dedicated platform")
    matrices = {}
    for platform in platforms:
        path = root / platform / "native-g7-matrix.json"
        payload = json.loads(path.read_text(encoding="utf-8"))
        audit = validate_matrix(
            payload,
            source_commit,
            expected_tree=expected_tree,
        )
        if payload.get("platform") != platform:
            raise GateFailure(f"G7 matrix platform mismatch: {platform}")
        matrices[platform] = {
            "audit": audit,
            "matrix_sha256": __import__("hashlib").sha256(path.read_bytes()).hexdigest(),
            "counter_status": {
                name: (
                    "measured"
                    if all(
                        receipt["counters"][name]["status"] == "measured"
                        for receipt in payload["receipts"]
                    )
                    else "unavailable"
                )
                for name in payload["receipts"][0]["counters"]
            },
        }
    return {
        "schema": "hyphae-native-g7-aggregate-v2",
        "gate": "G7",
        "status": "passed",
        "source_commit": source_commit,
        "platforms": matrices,
        "claims": ["G7"],
        "closure_declared": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--expected-tree")
    parser.add_argument("--platform-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        result = aggregate(
            arguments.platform_root,
            arguments.source_commit,
            expected_tree=arguments.expected_tree,
        )
        arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G7 aggregate failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
