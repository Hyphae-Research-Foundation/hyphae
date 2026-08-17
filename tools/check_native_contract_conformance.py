#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run and validate hosted G0 SQL/structure/search/ANN contract conformance."""

from __future__ import annotations

import argparse
import hashlib
import json
import shlex
import subprocess
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-contract-conformance-v1"
RECEIPT_SCHEMA = "hyphae-native-contract-conformance-receipt-v1"
SURFACES = {
    "sql-semantics",
    "structure-semantics",
    "search-semantics",
    "ann-semantics",
    "cross-engine-atomicity",
}


class GateFailure(RuntimeError):
    """The contract inventory or a required executable surface failed."""


def validate_inventory(root: Path, inventory: dict[str, Any]) -> list[dict[str, Any]]:
    if inventory.get("schema") != SCHEMA or inventory.get("requirement") != "sql-structure-search-ann-contracts":
        raise GateFailure("unsupported native contract inventory")
    surfaces = inventory.get("surfaces")
    if not isinstance(surfaces, list) or {entry.get("id") for entry in surfaces if isinstance(entry, dict)} != SURFACES:
        raise GateFailure("contract inventory must contain the exact required surfaces")
    commands: set[str] = set()
    for surface in surfaces:
        if not isinstance(surface, dict) or set(surface) != {"id", "contract", "tests"}:
            raise GateFailure("invalid contract surface fields")
        contract = surface.get("contract")
        if not isinstance(contract, str) or Path(contract).is_absolute() or not (root / contract).is_file():
            raise GateFailure(f"missing contract for {surface.get('id')}")
        tests = surface.get("tests")
        if not isinstance(tests, list) or not tests:
            raise GateFailure(f"tests required for {surface.get('id')}")
        for command in tests:
            if not isinstance(command, str) or not command.startswith("cargo test ") or command in commands:
                raise GateFailure("test commands must be unique pinned cargo test commands")
            if "--locked" not in shlex.split(command):
                raise GateFailure("contract test command must use --locked")
            commands.add(command)
    return surfaces


def run_inventory(root: Path, inventory_path: Path) -> dict[str, Any]:
    inventory_bytes = inventory_path.read_bytes()
    surfaces = validate_inventory(root, json.loads(inventory_bytes))
    executions = []
    for surface in surfaces:
        for command in surface["tests"]:
            completed = subprocess.run(
                shlex.split(command), cwd=root, text=True, capture_output=True, check=False
            )
            executions.append(
                {
                    "surface": surface["id"],
                    "command": command,
                    "exit_code": completed.returncode,
                }
            )
            if completed.returncode != 0:
                raise GateFailure(f"contract test failed: {command}\n{completed.stderr[-2000:]}")
    contracts = {
        surface["contract"]: hashlib.sha256((root / surface["contract"]).read_bytes()).hexdigest()
        for surface in surfaces
    }
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "source_commit": commit,
        "inventory_sha256": hashlib.sha256(inventory_bytes).hexdigest(),
        "surfaces": sorted(SURFACES),
        "contracts": contracts,
        "executions": executions,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    inventory = args.inventory if args.inventory.is_absolute() else root / args.inventory
    try:
        receipt = run_inventory(root, inventory)
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native contract conformance failed: {error}")
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
