#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Produce one suite-bound exact-SHA G3 receipt from authorized Cargo logs."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")
TEST_RESULT = re.compile(r"test result: ok\. ([0-9]+) passed; 0 failed;")


class GateFailure(ValueError):
    pass


def manifest_requirement(manifest: dict[str, Any], requirement: str) -> dict[str, Any]:
    if manifest.get("schema") != "hyphae-native-g3-suite-manifest-v1" or manifest.get("gate") != "G3":
        raise GateFailure("unsupported suite manifest")
    rows = manifest.get("requirements")
    if not isinstance(rows, list):
        raise GateFailure("suite manifest requirements are invalid")
    matches = [row for row in rows if row.get("id") == requirement]
    if len(matches) != 1:
        raise GateFailure("requirement is absent or duplicated in suite manifest")
    return matches[0]


def build_receipt(
    source_commit: str,
    requirement: str,
    manifest: dict[str, Any],
    manifest_sha256: str,
    platform: str,
    toolchain: str,
    logs: list[tuple[str, bytes]],
    peak_rss_kib: int | None = None,
) -> dict[str, Any]:
    if not HEX40.fullmatch(source_commit) or not HEX64.fullmatch(manifest_sha256):
        raise GateFailure("commit or manifest digest is invalid")
    if not platform or not toolchain:
        raise GateFailure("platform and toolchain are required")
    row = manifest_requirement(manifest, requirement)
    suites = row.get("suites")
    if not isinstance(suites, list) or not suites:
        raise GateFailure("authorized suite set is empty")
    expected = {suite.get("name"): suite.get("command") for suite in suites}
    if None in expected or any(not isinstance(command, list) or not command for command in expected.values()):
        raise GateFailure("authorized suite definition is invalid")
    supplied = {name: raw for name, raw in logs}
    if len(supplied) != len(logs) or set(supplied) != set(expected):
        raise GateFailure("supplied logs do not exactly match authorized suites")
    audits = []
    total = 0
    for name in sorted(expected):
        raw = supplied[name]
        text = raw.decode("utf-8")
        marker = "G3_COMMAND: " + json.dumps(expected[name], separators=(",", ":"))
        if marker not in text or "test result: FAILED" in text:
            raise GateFailure(f"suite {name} does not prove its authorized command")
        counts = [int(value) for value in TEST_RESULT.findall(text)]
        if not counts or any(value <= 0 for value in counts):
            raise GateFailure(f"suite {name} has no positive successful test result")
        count = sum(counts)
        total += count
        audits.append({"name": name, "test_count": count, "log_sha256": hashlib.sha256(raw).hexdigest()})
    if requirement == "memory-amplification":
        metrics = row.get("hosted_metrics")
        if not isinstance(metrics, dict) or set(metrics) != {"max_peak_rss_kib"}:
            raise GateFailure("memory amplification hosted metric contract is invalid")
        maximum = metrics["max_peak_rss_kib"]
        if (
            not isinstance(maximum, int)
            or isinstance(maximum, bool)
            or maximum <= 0
            or not isinstance(peak_rss_kib, int)
            or isinstance(peak_rss_kib, bool)
            or peak_rss_kib <= 0
            or peak_rss_kib > maximum
        ):
            raise GateFailure("peak RSS is absent, invalid, or above its hosted bound")
    elif peak_rss_kib is not None:
        raise GateFailure("peak RSS is only valid for memory amplification")
    payload = {
        "schema": "hyphae-native-g3-receipt-v2",
        "status": "passed",
        "requirement": requirement,
        "source_commit": source_commit,
        "manifest_sha256": manifest_sha256,
        "platform": platform,
        "toolchain": toolchain,
        "suites": audits,
        "test_count": total,
        "scope": "bounded-correctness",
        "production_scale": False,
    }
    if peak_rss_kib is not None:
        payload["peak_rss_kib"] = peak_rss_kib
        payload["max_peak_rss_kib"] = row["hosted_metrics"]["max_peak_rss_kib"]
    return payload


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--suite-log", action="append", nargs=2, metavar=("NAME", "PATH"), required=True)
    parser.add_argument("--peak-rss-kib", type=int)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        payload = build_receipt(
            args.source_commit,
            args.requirement,
            json.loads(args.manifest.read_text(encoding="utf-8")),
            args.manifest_sha256,
            args.platform,
            args.toolchain,
            [(name, Path(path).read_bytes()) for name, path in args.suite_log],
            args.peak_rss_kib,
        )
    except (GateFailure, OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"native G3 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
