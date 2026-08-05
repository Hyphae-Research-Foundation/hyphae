#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate one suite-bound exact-SHA G3 receipt."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")


class GateFailure(ValueError):
    pass


def validate(payload: dict[str, Any], expected_commit: str, expected_requirement: str, manifest_sha256: str) -> dict[str, Any]:
    if payload.get("schema") != "hyphae-native-g3-receipt-v2" or payload.get("status") != "passed":
        raise GateFailure("unsupported or unpassed receipt")
    commit = payload.get("source_commit")
    if not isinstance(commit, str) or not HEX40.fullmatch(commit) or commit != expected_commit:
        raise GateFailure("source commit mismatch")
    if payload.get("requirement") != expected_requirement:
        raise GateFailure("requirement mismatch")
    if not HEX64.fullmatch(manifest_sha256) or payload.get("manifest_sha256") != manifest_sha256:
        raise GateFailure("suite manifest digest mismatch")
    if not isinstance(payload.get("platform"), str) or not payload["platform"]:
        raise GateFailure("platform is missing")
    if not isinstance(payload.get("toolchain"), str) or not payload["toolchain"]:
        raise GateFailure("toolchain is missing")
    suites = payload.get("suites")
    if not isinstance(suites, list) or not suites:
        raise GateFailure("suite audits are missing")
    names = []
    count = 0
    for suite in suites:
        if set(suite) != {"name", "test_count", "log_sha256"}:
            raise GateFailure("suite audit fields mismatch")
        if not isinstance(suite["name"], str) or not suite["name"] or suite["name"] in names:
            raise GateFailure("suite audit identity is invalid")
        if not isinstance(suite["test_count"], int) or isinstance(suite["test_count"], bool) or suite["test_count"] <= 0:
            raise GateFailure("suite test count is invalid")
        if not isinstance(suite["log_sha256"], str) or not HEX64.fullmatch(suite["log_sha256"]):
            raise GateFailure("suite log digest is invalid")
        names.append(suite["name"])
        count += suite["test_count"]
    if payload.get("test_count") != count:
        raise GateFailure("aggregate test count mismatch")
    if payload.get("scope") != "bounded-correctness" or payload.get("production_scale") is not False:
        raise GateFailure("receipt scope is invalid")
    audit = {
        "schema": "hyphae-native-g3-receipt-audit-v2",
        "status": "passed",
        "source_commit": commit,
        "requirement": expected_requirement,
        "manifest_sha256": manifest_sha256,
        "platform": payload["platform"],
        "toolchain": payload["toolchain"],
        "suite_count": len(suites),
        "test_count": count,
        "scope": "bounded-correctness",
        "production_scale": False,
    }
    if expected_requirement == "memory-amplification":
        peak = payload.get("peak_rss_kib")
        maximum = payload.get("max_peak_rss_kib")
        if (
            not isinstance(peak, int)
            or isinstance(peak, bool)
            or not isinstance(maximum, int)
            or isinstance(maximum, bool)
            or peak <= 0
            or maximum <= 0
            or peak > maximum
        ):
            raise GateFailure("memory amplification RSS bound is invalid")
        audit["peak_rss_kib"] = peak
        audit["max_peak_rss_kib"] = maximum
    elif "peak_rss_kib" in payload or "max_peak_rss_kib" in payload:
        raise GateFailure("unexpected RSS metrics")
    return audit


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-requirement", required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate(json.loads(args.receipt.read_text(encoding="utf-8")), args.expected_commit, args.expected_requirement, args.manifest_sha256)
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native G3 receipt audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
