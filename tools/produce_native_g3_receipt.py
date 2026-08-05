#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Produce one content-bound G3 audit from a successful Cargo test log."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

REQUIREMENTS = {
    "strings-and-counters",
    "hashes",
    "lists",
    "sets",
    "sorted-sets",
    "streams",
    "ttl-and-controlled-expiry",
    "atomic-batches-and-conflicts",
    "model-based-randomized-equivalence",
    "restart-equivalence",
    "memory-amplification",
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--requirement", required=True, choices=sorted(REQUIREMENTS))
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        parser.error("source commit must be a lowercase full SHA")
    raw = args.log.read_bytes()
    text = raw.decode("utf-8")
    counts = [int(value) for value in re.findall(r"test result: ok\. (\d+) passed; 0 failed", text)]
    if not counts or sum(counts) <= 0 or "test result: FAILED" in text:
        parser.error("log does not prove a positive successful test run")
    payload = {
        "schema": "hyphae-native-g3-receipt-audit-v1",
        "requirement": args.requirement,
        "status": "passed",
        "scope": "bounded-correctness",
        "production_scale": False,
        "source_commit": args.source_commit,
        "test_count": sum(counts),
        "log_sha256": hashlib.sha256(raw).hexdigest(),
    }
    args.output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
