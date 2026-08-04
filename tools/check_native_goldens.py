#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the exact source inventory for native golden encodings."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-golden-inventory-v1"
FIELDS = {"id", "producer", "test", "consumer"}


class GateFailure(RuntimeError):
    """The golden inventory is malformed or does not match repository source."""


def _mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _path_under(root: Path, value: str, label: str) -> Path:
    resolved_root = root.resolve()
    path = (resolved_root / value).resolve()
    try:
        path.relative_to(resolved_root)
    except ValueError as error:
        raise GateFailure(f"{label} escapes repository root: {value}") from error
    if not path.is_file():
        raise GateFailure(f"{label} is missing: {value}")
    return path


def validate_inventory(root: Path, inventory: dict[str, Any]) -> dict[str, Any]:
    """Return a content-bound ordered inventory or fail on any drift."""

    if inventory.get("schema") != SCHEMA or set(inventory) != {"schema", "fixtures"}:
        raise GateFailure("unsupported or malformed golden inventory")
    fixtures = inventory.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        raise GateFailure("fixtures must be a nonempty array")
    seen: set[str] = set()
    rows: list[dict[str, str]] = []
    for value in fixtures:
        fixture = _mapping(value, "fixture")
        if set(fixture) != FIELDS:
            raise GateFailure("unknown fixture field or missing required field")
        for field in FIELDS:
            if not isinstance(fixture[field], str) or not fixture[field]:
                raise GateFailure(f"fixture {field} must be a nonempty string")
        fixture_id = fixture["id"]
        if fixture_id in seen:
            raise GateFailure(f"duplicate fixture {fixture_id}")
        seen.add(fixture_id)
        producer = _path_under(root, fixture["producer"], "producer")
        consumer = _path_under(root, fixture["consumer"], "consumer")
        producer_bytes = producer.read_bytes()
        consumer_bytes = consumer.read_bytes()
        symbol = fixture["test"].encode("utf-8")
        if symbol not in producer_bytes:
            raise GateFailure(f"test symbol {fixture['test']} missing from producer")
        rows.append(
            {
                "id": fixture_id,
                "producer": fixture["producer"],
                "producer_sha256": hashlib.sha256(producer_bytes).hexdigest(),
                "test": fixture["test"],
                "consumer": fixture["consumer"],
                "consumer_sha256": hashlib.sha256(consumer_bytes).hexdigest(),
            }
        )
    return {
        "schema": "hyphae-native-golden-audit-v1",
        "status": "passed",
        "fixture_count": len(rows),
        "fixtures": rows,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        result = validate_inventory(
            args.root,
            json.loads(args.inventory.read_text(encoding="utf-8")),
        )
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native golden inventory failed: {error}")
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
