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


def validate_receipt_set(
    profile: dict[str, Any], receipts: list[dict[str, Any]]
) -> dict[str, Any]:
    """Validate one exact receipt for every platform required by the profile."""

    surfaces = profile.get("surfaces")
    if not isinstance(surfaces, list):
        raise GateFailure("validated profile has no surfaces")
    required_platforms: set[str] = set()
    for value in surfaces:
        surface = _mapping(value, "surface")
        platforms = surface.get("required_platforms")
        if not isinstance(platforms, list):
            raise GateFailure("profile surface has no required platforms")
        required_platforms.update(platforms)
    seen: set[str] = set()
    passed_platforms = 0
    passed_surfaces = 0
    summaries: list[dict[str, Any]] = []
    for receipt in receipts:
        platform = receipt.get("platform")
        if not isinstance(platform, str):
            raise GateFailure("receipt platform must be a string")
        if platform in seen:
            raise GateFailure(f"duplicate platform receipt: {platform}")
        seen.add(platform)
        validate_receipt(profile, receipt)
        passed_platforms += receipt["status"] == "passed"
        passed_surfaces += receipt["passed_count"]
        summaries.append(
            {
                "platform": platform,
                "status": receipt["status"],
                "passed_count": receipt["passed_count"],
                "required_count": receipt["required_count"],
            }
        )
    missing = required_platforms - seen
    if missing:
        raise GateFailure("missing platform receipt: " + ", ".join(sorted(missing)))
    extra = seen - required_platforms
    if extra:
        raise GateFailure("unexpected platform receipt: " + ", ".join(sorted(extra)))
    return {
        "schema": "hyphae-native-conformance-aggregate-v1",
        "status": "passed" if passed_platforms == len(required_platforms) else "failed",
        "platform_count": len(required_platforms),
        "passed_platforms": passed_platforms,
        "passed_surfaces": passed_surfaces,
        "platforms": sorted(summaries, key=lambda row: row["platform"]),
    }


def validate_receipt(profile: dict[str, Any], receipt: dict[str, Any]) -> None:
    """Validate exact platform coverage and receipt arithmetic."""

    receipt_fields = {
        "schema",
        "platform",
        "status",
        "required_count",
        "passed_count",
        "results",
    }
    if set(receipt) != receipt_fields:
        raise GateFailure("unknown receipt field or missing required field")
    if receipt.get("schema") != "hyphae-native-conformance-receipt-v1":
        raise GateFailure("unsupported conformance receipt")
    platform = receipt.get("platform")
    if platform not in PLATFORMS:
        raise GateFailure("unknown receipt platform")
    if profile.get("schema") != "hyphae-native-conformance-profile-audit-v1" or profile.get(
        "status"
    ) != "passed":
        raise GateFailure("receipt requires a validated profile")
    surfaces = profile.get("surfaces")
    results = receipt.get("results")
    if not isinstance(surfaces, list) or not isinstance(results, list):
        raise GateFailure("profile surfaces and receipt results must be arrays")
    expected = {
        surface["id"]
        for surface in surfaces
        if isinstance(surface, dict)
        and isinstance(surface.get("required_platforms"), list)
        and platform in surface["required_platforms"]
    }
    seen: set[str] = set()
    passed = 0
    result_fields = {"id", "command", "status", "exit_code"}
    for value in results:
        result = _mapping(value, "receipt result")
        if set(result) != result_fields:
            raise GateFailure("unknown result field or missing required field")
        result_id = result.get("id")
        if not isinstance(result_id, str):
            raise GateFailure("result ID must be a string")
        if result_id in seen:
            raise GateFailure(f"duplicate result {result_id}")
        seen.add(result_id)
        status = result.get("status")
        exit_code = result.get("exit_code")
        if status not in {"passed", "failed"} or not isinstance(exit_code, int):
            raise GateFailure(f"result {result_id} is malformed")
        if (status == "passed") != (exit_code == 0):
            raise GateFailure(f"result {result_id} status and exit code disagree")
        passed += status == "passed"
    if seen != expected:
        raise GateFailure("receipt coverage differs from required platform surfaces")
    required_count = receipt.get("required_count")
    passed_count = receipt.get("passed_count")
    status = receipt.get("status")
    expected_status = "passed" if passed == len(expected) else "failed"
    if (
        required_count != len(expected)
        or passed_count != passed
        or status != expected_status
    ):
        raise GateFailure("receipt summary is inconsistent with results")


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
    parser.add_argument("--receipt", type=Path, action="append", default=[])
    parser.add_argument("--aggregate", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        profile = validate_profile(json.loads(args.profile.read_text(encoding="utf-8")))
        if args.run and args.aggregate:
            raise GateFailure("--run and --aggregate are mutually exclusive")
        if args.aggregate:
            if not args.receipt:
                raise GateFailure("--aggregate requires --receipt")
            result = validate_receipt_set(
                profile,
                [json.loads(path.read_text(encoding="utf-8")) for path in args.receipt],
            )
        elif args.run:
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
