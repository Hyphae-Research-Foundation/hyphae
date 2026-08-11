#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation for one exact-SHA G6 supporting receipt."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g6_foundation import PLATFORMS, REQUIREMENTS, WORKLOAD_ACCEPTANCE, GateFailure
from tools.check_native_g6_manifests import MANIFEST_NAMES
from tools.produce_native_g6_receipt import TOOL_NAME, _canonical_sha256, _coverage

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
RECEIPT_FIELDS = {
    "schema", "gate", "status", "evidence_class", "source_commit", "requirement",
    "manifest_sha256", "authority", "workload", "suite_identity_sha256", "platform",
    "tool_versions", "sdks", "transports", "command_results", "test_count",
    "implementation_status", "uncovered_acceptance", "claims", "closure_declared",
}
AUDIT_FIELDS = RECEIPT_FIELDS | {"suite_count"}


def validate(
    payload: dict[str, Any],
    expected_commit: str,
    expected_requirement: str,
    manifest_sha256: dict[str, str],
    authority_manifest: dict[str, Any],
    workload_manifest: dict[str, Any],
    suite_manifest: dict[str, Any],
    inventory_manifest: dict[str, Any] | None = None,
) -> dict[str, Any]:
    if set(payload) != RECEIPT_FIELDS:
        raise GateFailure("G6 receipt fields mismatch")
    identity = {
        "schema": "hyphae-native-g6-receipt-v1",
        "gate": "G6",
        "status": "passed",
        "evidence_class": "supporting-not-closure",
        "source_commit": expected_commit,
        "requirement": expected_requirement,
        "manifest_sha256": manifest_sha256,
        "claims": [],
        "closure_declared": False,
    }
    if (
        HEX40.fullmatch(expected_commit) is None
        or expected_requirement not in REQUIREMENTS
        or set(manifest_sha256) != set(MANIFEST_NAMES)
        or any(HEX64.fullmatch(value) is None for value in manifest_sha256.values())
        or any(payload.get(key) != value for key, value in identity.items())
    ):
        raise GateFailure("G6 receipt exact identity or open state mismatch")
    if (
        set(authority_manifest) != {"schema", "gate", "scope", "evidence_class", "requirements", "required_predecessors", "required_platforms", "required_sdks", "required_transports", "contracts", "claims", "closure_declared"}
        or
        authority_manifest.get("schema") != "hyphae-native-g6-authority-manifest-v1"
        or authority_manifest.get("gate") != "G6"
        or authority_manifest.get("claims") != []
        or authority_manifest.get("closure_declared") is not False
        or expected_requirement not in authority_manifest.get("requirements", [])
    ):
        raise GateFailure("G6 receipt authority is invalid")
    if (
        set(workload_manifest) != {"schema", "gate", "required_platforms", "workloads", "claims", "closure_declared"}
        or workload_manifest.get("schema") != "hyphae-native-g6-workload-manifest-v1"
        or workload_manifest.get("gate") != "G6"
        or workload_manifest.get("claims") != []
        or workload_manifest.get("closure_declared") is not False
        or set(suite_manifest) != {"schema", "gate", "requirements", "claims", "closure_declared"}
        or suite_manifest.get("schema") != "hyphae-native-g6-suite-manifest-v1"
        or suite_manifest.get("gate") != "G6"
        or suite_manifest.get("claims") != []
        or suite_manifest.get("closure_declared") is not False
    ):
        raise GateFailure("G6 receipt workload or suite authority is invalid")
    if inventory_manifest is not None and (
        set(inventory_manifest) != {"schema", "gate", "requirements", "claims", "closure_declared"}
        or inventory_manifest.get("schema") != "hyphae-native-g6-inventory-v1"
        or inventory_manifest.get("gate") != "G6"
        or inventory_manifest.get("claims") != []
        or inventory_manifest.get("closure_declared") is not False
    ):
        raise GateFailure("G6 receipt inventory authority is invalid")
    authority = payload["authority"]
    expected_authority = {
        "scope": authority_manifest.get("scope"),
        "evidence_class": authority_manifest.get("evidence_class"),
        "identity_sha256": _canonical_sha256(authority_manifest),
    }
    if authority != expected_authority:
        raise GateFailure("G6 authority identity mismatch")
    workload_matches = [row for row in workload_manifest.get("workloads", []) if isinstance(row, dict) and row.get("requirement") == expected_requirement]
    suite_matches = [row for row in suite_manifest.get("requirements", []) if isinstance(row, dict) and row.get("id") == expected_requirement]
    if len(workload_matches) != 1 or len(suite_matches) != 1:
        raise GateFailure("G6 workload or suite identity is absent or duplicated")
    workload, suite_row = workload_matches[0], suite_matches[0]
    if inventory_manifest is not None:
        inventory_matches = [row for row in inventory_manifest.get("requirements", []) if isinstance(row, dict) and row.get("id") == expected_requirement]
        if len(inventory_matches) != 1:
            raise GateFailure("G6 receipt inventory is absent or duplicated")
    expected_workload = {
        "id": workload.get("id"),
        "oracle": workload.get("oracle"),
        "acceptance": workload.get("acceptance"),
        "identity_sha256": _canonical_sha256(workload),
    }
    if (
        payload["workload"] != expected_workload
        or not isinstance(workload.get("acceptance"), list)
        or len(workload["acceptance"]) != len(set(workload["acceptance"]))
        or set(workload["acceptance"]) != WORKLOAD_ACCEPTANCE[expected_requirement]
        or suite_row.get("workloads") != [workload.get("id")]
        or payload["implementation_status"] != suite_row.get("status")
        or payload["uncovered_acceptance"] != suite_row.get("uncovered_acceptance")
        or payload["suite_identity_sha256"] != _canonical_sha256(suite_row)
    ):
        raise GateFailure("G6 workload acceptance or suite identity mismatch")
    expected_inventory_status = {
        "implemented-unhosted": "implemented-unhosted",
        "partial-unhosted": "partial",
    }.get(payload["implementation_status"])
    if (
        expected_inventory_status is None
        or inventory_manifest is not None and inventory_matches[0].get("status") != expected_inventory_status
        or not isinstance(payload["uncovered_acceptance"], list)
        or len(payload["uncovered_acceptance"]) != len(set(payload["uncovered_acceptance"]))
        or not set(payload["uncovered_acceptance"]).issubset(WORKLOAD_ACCEPTANCE[expected_requirement])
        or (payload["implementation_status"] == "implemented-unhosted" and payload["uncovered_acceptance"])
        or (payload["implementation_status"] == "partial-unhosted" and not payload["uncovered_acceptance"])
    ):
        raise GateFailure("G6 receipt implementation coverage mismatch")
    suite_items = suite_row.get("suites", [])
    expected_suites = [
        item
        for item in suite_items
        if isinstance(item, dict) and payload["platform"] in item.get("platforms", PLATFORMS)
    ]
    expected_commands = {
        item.get("name"): item.get("platform_commands", {}).get(payload["platform"], item.get("command"))
        for item in expected_suites
    }
    covered_values = [
        value
        for item in suite_items
        if isinstance(item, dict)
        for value in item.get("acceptance", [])
    ]
    covered_acceptance = set(covered_values)
    if (
        len(covered_values) != len(covered_acceptance)
        or covered_acceptance | set(payload["uncovered_acceptance"]) != WORKLOAD_ACCEPTANCE[expected_requirement]
        or covered_acceptance & set(payload["uncovered_acceptance"])
    ):
        raise GateFailure("G6 receipt acceptance coverage mismatch")
    results = payload["command_results"]
    if not expected_commands or None in expected_commands or not isinstance(results, list) or len(results) != len(expected_commands):
        raise GateFailure("G6 command result coverage mismatch")
    names: set[str] = set()
    count = 0
    for result in results:
        if not isinstance(result, dict) or set(result) != {"name", "command", "command_sha256", "status", "exit_code", "test_count", "log_sha256"}:
            raise GateFailure("G6 command result fields mismatch")
        name, command = result["name"], result["command"]
        tests = result["test_count"]
        if (
            name in names
            or name not in expected_commands
            or command != expected_commands[name]
            or result["command_sha256"] != _canonical_sha256(command)
            or result["status"] != "passed"
            or result["exit_code"] != 0
            or not isinstance(tests, int)
            or isinstance(tests, bool)
            or tests <= 0
            or not isinstance(result["log_sha256"], str)
            or HEX64.fullmatch(result["log_sha256"]) is None
        ):
            raise GateFailure("invalid G6 command result")
        names.add(name)
        count += tests
    sdks, transports = _coverage(expected_suites)
    tools = payload["tool_versions"]
    if (
        set(names) != set(expected_commands)
        or payload["test_count"] != count
        or payload["platform"] not in PLATFORMS
        or not isinstance(tools, dict)
        or not tools
        or any(TOOL_NAME.fullmatch(name) is None or not isinstance(version, str) or not version.strip() for name, version in tools.items())
        or not {command[0] for command in expected_commands.values()}.issubset(tools)
        or payload["sdks"] != sdks
        or payload["transports"] != transports
    ):
        raise GateFailure("G6 execution identity or aggregate result mismatch")
    audit = dict(payload)
    audit["schema"] = "hyphae-native-g6-receipt-audit-v1"
    audit["suite_count"] = len(results)
    return audit


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-requirement", required=True)
    for name in MANIFEST_NAMES:
        parser.add_argument(f"--{name}-sha256", required=True)
    parser.add_argument("--authority-manifest", type=Path, required=True)
    parser.add_argument("--workload-manifest", type=Path, required=True)
    parser.add_argument("--suite-manifest", type=Path, required=True)
    parser.add_argument("--inventory-manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        digests = {name: getattr(args, f"{name}_sha256") for name in MANIFEST_NAMES}
        authority_raw = args.authority_manifest.read_bytes()
        workload_raw = args.workload_manifest.read_bytes()
        suite_raw = args.suite_manifest.read_bytes()
        inventory_raw = args.inventory_manifest.read_bytes()
        for name, raw in (("authority", authority_raw), ("workload", workload_raw), ("suite", suite_raw), ("inventory", inventory_raw)):
            if hashlib.sha256(raw).hexdigest() != digests[name]:
                raise GateFailure(f"G6 {name} manifest digest mismatch")
        result = validate(
            json.loads(args.receipt.read_text(encoding="utf-8")),
            args.expected_commit,
            args.expected_requirement,
            digests,
            json.loads(authority_raw),
            json.loads(workload_raw),
            json.loads(suite_raw),
            json.loads(inventory_raw),
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError, TypeError) as error:
        print(f"native G6 receipt audit failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
