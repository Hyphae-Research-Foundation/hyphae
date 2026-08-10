#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Build retained exact-SHA G2/G3 closure status from hosted aggregates."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}")


class GateFailure(ValueError):
    pass


def close(status: dict[str, Any], commit: str, g2: Path, g3: Path) -> dict[str, Any]:
    if status.get("schema") != "hyphae-native-gate-status-v1" or not HEX40.fullmatch(commit):
        raise GateFailure("invalid status or commit")
    artifacts = {"G2": g2, "G3": g3}
    result = copy.deepcopy(status)
    rows = {row.get("id"): row for row in result.get("gates", [])}
    if set(artifacts) - set(rows):
        raise GateFailure("missing gate rows")
    for gate, path in artifacts.items():
        payload = json.loads(path.read_text(encoding="utf-8"))
        if payload.get("gate") != gate or payload.get("status") != "passed":
            raise GateFailure(f"{gate} aggregate is not passed")
        if payload.get("required") != payload.get("passed"):
            raise GateFailure(f"{gate} aggregate is incomplete")
        if payload.get("source_commit", commit) != commit:
            raise GateFailure(f"{gate} aggregate commit mismatch")
        rows[gate].clear()
        rows[gate].update({
            "id": gate,
            "status": "closed",
            "source_commit": commit,
            "evidence": path.as_posix(),
            "evidence_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        })
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--status", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--g2", type=Path, required=True)
    parser.add_argument("--g3", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = close(
            json.loads(args.status.read_text(encoding="utf-8")),
            args.source_commit,
            args.g2,
            args.g3,
        )
    except (GateFailure, OSError, json.JSONDecodeError) as error:
        print(f"native gate closure failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
