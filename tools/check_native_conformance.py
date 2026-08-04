#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the reviewed native implementation-conformance profile."""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
from collections import Counter
from collections.abc import Callable
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-conformance-profile-v1"
FIELDS = {"id", "command", "required_platforms"}
PLATFORMS = {"linux", "macos", "windows"}


class GateFailure(RuntimeError):
    """The conformance profile is malformed or ambiguous."""


def _mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def validate_profile(profile: dict[str, Any]) -> dict[str, Any]:
    """Return the normalized profile or fail closed on any drift."""

    if profile.get("schema") != SCHEMA or set(profile) != {"schema", "surfaces"}:
        raise GateFailure("unsupported or malformed conformance profile")
    surfaces = profile.get("surfaces")
    if not isinstance(surfaces, list) or not surfaces:
        raise GateFailure("surfaces must be a nonempty array")
    seen: set[str] = set()
    counts: Counter[str] = Counter()
    rows: list[dict[str, Any]] = []
    for value in surfaces:
        surface = _mapping(value, "surface")
        if set(surface) != FIELDS:
            raise GateFailure("unknown surface field or missing required field")
        surface_id = surface.get("id")
        command = surface.get("command")
        platforms = surface.get("required_platforms")
        if not isinstance(surface_id, str) or not surface_id:
            raise GateFailure("surface ID must be a nonempty string")
        if surface_id in seen:
            raise GateFailure(f"duplicate surface {surface_id}")
        seen.add(surface_id)
        if (
            not isinstance(command, str)
            or not command.startswith("cargo test ")
            or " --locked" not in command
        ):
            raise GateFailure(f"surface {surface_id} requires a locked cargo test command")
        if not isinstance(platforms, list) or not platforms:
            raise GateFailure(f"surface {surface_id} requires platforms")
        if len(platforms) != len(set(platforms)):
            raise GateFailure(f"duplicate platform for surface {surface_id}")
        unknown = set(platforms) - PLATFORMS
        if unknown:
            raise GateFailure("unknown platform: " + ", ".join(sorted(unknown)))
        counts.update(platforms)
        rows.append(
            {
                "id": surface_id,
                "command": command,
                "required_platforms": platforms,
            }
        )
    return {
        "schema": "hyphae-native-conformance-profile-audit-v1",
        "status": "passed",
        "surface_count": len(rows),
        "platform_counts": dict(sorted(counts.items())),
        "surfaces": rows,
    }


def run_profile(
    profile: dict[str, Any], platform: str, execute: Callable[[list[str]], int]
) -> dict[str, Any]:
    """Run the reviewed commands required by one platform."""

    if platform not in PLATFORMS:
        raise GateFailure(f"unknown platform: {platform}")
    if profile.get("schema") != "hyphae-native-conformance-profile-audit-v1" or profile.get(
        "status"
    ) != "passed":
        raise GateFailure("runner requires a validated profile")
    surfaces = profile.get("surfaces")
    if not isinstance(surfaces, list):
        raise GateFailure("validated profile has no surfaces")
    results: list[dict[str, Any]] = []
    for value in surfaces:
        surface = _mapping(value, "surface")
        required = surface.get("required_platforms")
        if not isinstance(required, list) or platform not in required:
            continue
        command = surface.get("command")
        surface_id = surface.get("id")
        if not isinstance(command, str) or not isinstance(surface_id, str):
            raise GateFailure("validated profile surface is malformed")
        argv = shlex.split(command)
        exit_code = execute(argv)
        results.append(
            {
                "id": surface_id,
                "command": command,
                "status": "passed" if exit_code == 0 else "failed",
                "exit_code": exit_code,
            }
        )
    if not results:
        raise GateFailure(f"validated profile has no surfaces for {platform}")
    return {
        "schema": "hyphae-native-conformance-receipt-v1",
        "platform": platform,
        "status": "passed" if all(row["status"] == "passed" for row in results) else "failed",
        "required_count": len(results),
        "passed_count": sum(row["status"] == "passed" for row in results),
        "results": results,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--platform", choices=sorted(PLATFORMS))
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        profile = validate_profile(json.loads(args.profile.read_text(encoding="utf-8")))
        if args.run:
            if args.platform is None:
                raise GateFailure("--run requires --platform")
            result = run_profile(
                profile,
                args.platform,
                lambda command: subprocess.run(command, check=False).returncode,
            )
        else:
            result = profile
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native conformance profile failed: {error}")
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
