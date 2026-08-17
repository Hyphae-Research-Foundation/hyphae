#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the exact Hyphae Native G1 substrate dependency closure."""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path
from typing import Any

KERNEL_CRATES = {
    "hyphae-native-pages",
    "hyphae-native-blobs",
    "hyphae-native-wal",
    "hyphae-native-catalog",
    "hyphae-native-mvcc",
}
FORBIDDEN_ENGINES = {"redb", "rocksdb", "sled", "lmdb", "heed"}


class GateFailure(ValueError):
    pass


def validate_metadata(metadata: dict[str, Any]) -> dict[str, Any]:
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise GateFailure("cargo metadata packages are missing")
    by_name = {package.get("name"): package for package in packages}
    present_kernel = KERNEL_CRATES & set(by_name)
    if present_kernel != KERNEL_CRATES:
        raise GateFailure(
            f"native kernel crate set mismatch: {sorted(present_kernel)}"
        )
    runtime = by_name.get("hyphae-native-runtime")
    if runtime is None or runtime.get("source") is not None:
        raise GateFailure("native runtime workspace package is missing")
    runtime_dependencies = {
        dependency.get("name") for dependency in runtime.get("dependencies", [])
    }
    reachable = set()
    pending = list(runtime_dependencies)
    while pending:
        name = pending.pop()
        if name in reachable:
            continue
        reachable.add(name)
        package = by_name.get(name)
        if package is not None:
            pending.extend(
                dependency.get("name") for dependency in package.get("dependencies", [])
            )
    native_dependencies = sorted(runtime_dependencies & KERNEL_CRATES)
    if set(native_dependencies) != KERNEL_CRATES:
        raise GateFailure("native runtime does not bind every kernel crate")
    forbidden = sorted(reachable & FORBIDDEN_ENGINES)
    if forbidden:
        label = "redb" if "redb" in forbidden else "forbidden storage engine"
        raise GateFailure(f"{label} reachable from native runtime: {forbidden}")
    return {
        "schema": "hyphae-native-g1-substrate-audit-v1",
        "status": "passed",
        "kernel_crates": len(KERNEL_CRATES),
        "runtime_native_dependencies": native_dependencies,
        "redb_reachable": False,
        "forbidden_engines": [],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        metadata = json.loads(
            subprocess.check_output(
                ["cargo", "metadata", "--locked", "--format-version", "1"],
                cwd=args.root,
                text=True,
            )
        )
        result = validate_metadata(metadata)
        result["source_commit"] = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=args.root, text=True
        ).strip()
    except (GateFailure, OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"native G1 substrate audit failed: {error}")
        return 1
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
