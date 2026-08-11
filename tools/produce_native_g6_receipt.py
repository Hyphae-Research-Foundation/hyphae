#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Produce one exact-SHA G6 receipt from allowlisted hosted suite logs."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g6_foundation import (
    PLATFORMS,
    REQUIREMENTS,
    WORKLOAD_ACCEPTANCE,
    GateFailure,
    validate_suite_command,
)
from tools.check_native_g6_manifests import MANIFEST_NAMES, load_exact

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
TOOL_NAME = re.compile(r"[a-z0-9][a-z0-9_.+-]*\Z")
CARGO_RESULT = re.compile(r"test result: ok\. ([0-9]+) passed; 0 failed;")
PYTHON_RESULT = re.compile(r"Ran ([0-9]+) tests?\b")
PYTEST_RESULT = re.compile(r"(?m)^([0-9]+) passed(?:,| in )")
NODE_TESTS = re.compile(r"(?m)^(?:#|ℹ) tests ([0-9]+)\s*$")
NODE_PASS = re.compile(r"(?m)^(?:#|ℹ) pass ([0-9]+)\s*$")
NODE_FAIL = re.compile(r"(?m)^(?:#|ℹ) fail 0\s*$")
NODE_FAILURE = re.compile(r"(?m)^(?:#|ℹ) fail [1-9][0-9]*\s*$")
ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")


def _canonical_sha256(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")).hexdigest()


def _open(payload: dict[str, Any], name: str) -> None:
    if (
        payload.get("schema") != f"hyphae-native-g6-{name.replace('_', '-')}-v1"
        or payload.get("gate") != "G6"
        or payload.get("claims") != []
        or payload.get("closure_declared") is not False
    ):
        raise GateFailure(f"unsupported or claiming G6 {name} manifest")


def _cargo_result(command: list[str], text: str) -> int:
    targets: list[tuple[str, int]] = []
    current = ""
    for line in text.splitlines():
        stripped = ANSI_ESCAPE.sub("", line).strip()
        if "Running " in stripped:
            current = stripped.split("Running ", 1)[1]
        match = CARGO_RESULT.search(stripped)
        if match is not None:
            targets.append((current, int(match.group(1))))
    requested_tests = [command[index + 1] for index, value in enumerate(command[:-1]) if value == "--test"]
    if requested_tests:
        counts = []
        for requested in requested_tests:
            normalized = requested.replace("-", "_")
            matching = [
                count
                for target, count in targets
                if re.search(rf"(?:^|[/\\]){re.escape(normalized)}(?:\.rs)?(?:\s|\(|$)", target.replace("-", "_"))
            ]
            if not matching or sum(matching) <= 0:
                raise GateFailure(f"Cargo suite target {requested} executed no tests")
            counts.extend(matching)
        return sum(counts)
    if "--lib" in command:
        counts = [count for target, count in targets if "unittests src/lib.rs" in target.replace("\\", "/")]
        if not counts or sum(counts) <= 0:
            raise GateFailure("Cargo library suite executed no tests")
        return sum(counts)
    raise GateFailure("Cargo suite must select --lib or at least one --test target")


def _command_result(command: list[str], raw: bytes) -> int:
    text = raw.decode("utf-8")
    marker = "G6_COMMAND: " + json.dumps(command, separators=(",", ":"))
    if text.count(marker) != 1:
        raise GateFailure("suite log does not bind its exact command")
    if text.count("G6_EXIT_CODE: 0") != 1 or re.search(r"(?m)^G6_EXIT_CODE: (?!0$).+$", text):
        raise GateFailure("suite log does not bind one successful command result")
    if command[0] == "cargo":
        if "test result: FAILED" in text:
            raise GateFailure("suite log contains a failed or invalid test result")
        return _cargo_result(command, text)
    elif command[0] in {"python", "python3"}:
        if command[1:3] == ["-m", "unittest"]:
            counts = [int(value) for value in PYTHON_RESULT.findall(text)] if re.search(r"(?m)^OK\s*$", text) else []
            failed = "FAILED (" in text or re.search(r"(?m)^FAILED\s*$", text) is not None
        elif command[1:3] == ["-m", "pytest"]:
            counts = [int(value) for value in PYTEST_RESULT.findall(text)]
            failed = re.search(r"(?m)(?:^| )([1-9][0-9]*) failed(?:,| in )", text) is not None
        else:
            raise GateFailure("Python suite is not a test runner")
    else:
        if command[0] == "npm":
            counts = [int(value) for value in NODE_TESTS.findall(text)]
            passed = [int(value) for value in NODE_PASS.findall(text)]
            failed = NODE_FAILURE.search(text) is not None
            if NODE_FAIL.search(text) and counts and counts == passed:
                total = sum(counts)
                if total > 0:
                    return total
            if failed:
                raise GateFailure("suite log contains a failed or invalid test result")
            raise GateFailure("npm suite executed no tests")
        if command[1] != "--test":
            return 0
        tests = [int(value) for value in NODE_TESTS.findall(text)]
        passed = [int(value) for value in NODE_PASS.findall(text)]
        counts = tests if NODE_FAIL.search(text) and tests and tests == passed else []
        failed = NODE_FAILURE.search(text) is not None
    if failed or not counts or sum(counts) <= 0:
        raise GateFailure("suite log contains a failed or invalid test result")
    return sum(counts)


def _coverage(suites: list[dict[str, Any]]) -> tuple[list[str], list[str]]:
    sdks = {value for suite in suites for value in suite["coverage"]["sdks"]}
    transports = {value for suite in suites for value in suite["coverage"]["transports"]}
    return (
        [value for value in ("rust", "python", "typescript") if value in sdks],
        [value for value in ("embedded", "native-local", "http-v2") if value in transports],
    )


def build_receipt(
    source_commit: str,
    requirement: str,
    manifest_raw: dict[str, bytes],
    manifest_sha256: dict[str, str],
    platform: str,
    tool_versions: dict[str, str],
    logs: list[tuple[str, bytes]],
) -> dict[str, Any]:
    if HEX40.fullmatch(source_commit) is None or requirement not in REQUIREMENTS:
        raise GateFailure("G6 source commit or requirement is invalid")
    payloads = load_exact(manifest_raw, manifest_sha256)
    documents = dict(zip(MANIFEST_NAMES, payloads, strict=True))
    expected_fields = {
        "profile": {"schema", "gate", "scope", "requirements", "required_platforms", "required_sdks", "required_transports", "claims", "closure_declared"},
        "evidence": {"schema", "gate", "predecessor", "evidence", "claims", "closure_declared"},
        "inventory": {"schema", "gate", "requirements", "claims", "closure_declared"},
        "authority": {"schema", "gate", "scope", "evidence_class", "requirements", "required_predecessors", "required_platforms", "required_sdks", "required_transports", "contracts", "claims", "closure_declared"},
        "workload": {"schema", "gate", "required_platforms", "workloads", "claims", "closure_declared"},
        "suite": {"schema", "gate", "requirements", "claims", "closure_declared"},
        "predecessor": {"schema", "gate", "predecessors", "claims", "closure_declared"},
    }
    schema_names = {
        "profile": "readiness-profile",
        "evidence": "readiness-evidence",
        "inventory": "inventory",
        "authority": "authority-manifest",
        "workload": "workload-manifest",
        "suite": "suite-manifest",
        "predecessor": "predecessor-manifest",
    }
    for name, payload in documents.items():
        if set(payload) != expected_fields[name]:
            raise GateFailure(f"G6 {name} manifest fields mismatch")
        _open(payload, schema_names[name])
    profile_matches = [row for row in documents["profile"].get("requirements", []) if isinstance(row, dict) and row.get("id") == requirement]
    if len(profile_matches) != 1 or profile_matches[0] != {"id": requirement, "required_evidence": "hosted"}:
        raise GateFailure("G6 requirement is not a hosted profile row")
    if documents["evidence"].get("predecessor") is not None or documents["evidence"].get("evidence") != {}:
        raise GateFailure("G6 receipt must bind the open evidence baseline")
    if platform not in PLATFORMS:
        raise GateFailure("G6 platform is not required")
    if (
        not isinstance(tool_versions, dict)
        or not tool_versions
        or any(
            not isinstance(name, str)
            or TOOL_NAME.fullmatch(name) is None
            or not isinstance(version, str)
            or not version.strip()
            for name, version in tool_versions.items()
        )
    ):
        raise GateFailure("G6 tool versions are invalid")
    authority = documents["authority"]
    if requirement not in authority.get("requirements", []):
        raise GateFailure("G6 requirement is outside authority")
    workload_matches = [row for row in documents["workload"].get("workloads", []) if isinstance(row, dict) and row.get("requirement") == requirement]
    suite_matches = [row for row in documents["suite"].get("requirements", []) if isinstance(row, dict) and row.get("id") == requirement]
    inventory_matches = [row for row in documents["inventory"].get("requirements", []) if isinstance(row, dict) and row.get("id") == requirement]
    if len(workload_matches) != 1 or len(suite_matches) != 1 or len(inventory_matches) != 1:
        raise GateFailure("G6 requirement authority is absent or duplicated")
    workload, suite_row, inventory_row = workload_matches[0], suite_matches[0], inventory_matches[0]
    if (
        set(workload) != {"id", "requirement", "oracle", "acceptance"}
        or not isinstance(workload["oracle"], str)
        or not workload["oracle"]
        or workload["acceptance"] != list(dict.fromkeys(workload["acceptance"]))
        or set(workload["acceptance"]) != WORKLOAD_ACCEPTANCE[requirement]
        or suite_row.get("workloads") != [workload["id"]]
    ):
        raise GateFailure("G6 workload identity or acceptance mismatch")
    expected_inventory_status = {
        "implemented-unhosted": "implemented-unhosted",
        "partial-unhosted": "partial",
    }.get(suite_row.get("status"))
    uncovered = suite_row.get("uncovered_acceptance")
    if (
        expected_inventory_status is None
        or inventory_row.get("status") != expected_inventory_status
        or not isinstance(uncovered, list)
        or len(uncovered) != len(set(uncovered))
        or not set(uncovered).issubset(WORKLOAD_ACCEPTANCE[requirement])
        or (suite_row["status"] == "implemented-unhosted" and uncovered)
        or (suite_row["status"] == "partial-unhosted" and not uncovered)
    ):
        raise GateFailure("G6 requirement implementation status is inconsistent")
    suite_items = suite_row.get("suites")
    if not isinstance(suite_items, list) or not suite_items:
        raise GateFailure("G6 implemented requirement has no suites")
    expected: dict[str, list[str]] = {}
    for item in suite_items:
        required_fields = {"name", "acceptance", "coverage", "command"}
        if not isinstance(item, dict) or not required_fields.issubset(item) or not set(item).issubset(required_fields | {"platform_commands", "platforms"}) or not isinstance(item.get("name"), str) or not item["name"] or item["name"] in expected:
            raise GateFailure("invalid or duplicate G6 suite identity")
        suite_platforms = item.get("platforms", PLATFORMS)
        if not isinstance(suite_platforms, list) or suite_platforms != [value for value in PLATFORMS if value in suite_platforms] or not suite_platforms:
            raise GateFailure("invalid G6 suite platforms")
        platform_commands = item.get("platform_commands", {})
        if not isinstance(platform_commands, dict) or not set(platform_commands).issubset(suite_platforms):
            raise GateFailure("invalid G6 platform suite command")
        covered = item["acceptance"]
        if (
            not isinstance(covered, list)
            or len(covered) != len(set(covered))
            or not set(covered).issubset(WORKLOAD_ACCEPTANCE[requirement])
        ):
            raise GateFailure("invalid G6 suite acceptance coverage")
        coverage = item["coverage"]
        if (
            not isinstance(coverage, dict)
            or set(coverage) != {"sdks", "transports"}
            or not isinstance(coverage["sdks"], list)
            or coverage["sdks"] != [value for value in ("rust", "python", "typescript") if value in coverage["sdks"]]
            or not isinstance(coverage["transports"], list)
            or coverage["transports"] != [value for value in ("embedded", "native-local", "http-v2") if value in coverage["transports"]]
        ):
            raise GateFailure("invalid G6 suite surface coverage")
        if not covered and not coverage["sdks"] and not coverage["transports"]:
            raise GateFailure("G6 suite has no acceptance or surface coverage")
        validate_suite_command(item["command"])
        for platform_command in platform_commands.values():
            validate_suite_command(platform_command)
        if platform not in suite_platforms:
            continue
        command = platform_commands.get(platform, item["command"])
        expected[item["name"]] = command
    covered_values = [value for item in suite_items for value in item["acceptance"]]
    covered_acceptance = set(covered_values)
    if len(covered_values) != len(covered_acceptance) or covered_acceptance | set(uncovered) != WORKLOAD_ACCEPTANCE[requirement] or covered_acceptance & set(uncovered):
        raise GateFailure("incomplete G6 suite acceptance coverage")
    supplied = {name: raw for name, raw in logs}
    if len(supplied) != len(logs) or set(supplied) != set(expected):
        raise GateFailure("logs do not exactly match authorized G6 suites")
    required_tools = {command[0] for command in expected.values()}
    if not required_tools.issubset(tool_versions):
        raise GateFailure("G6 suite executable versions are incomplete")
    results: list[dict[str, Any]] = []
    total = 0
    for name in sorted(expected):
        command = expected[name]
        raw = supplied[name]
        count = _command_result(command, raw)
        total += count
        results.append(
            {
                "name": name,
                "command": command,
                "command_sha256": _canonical_sha256(command),
                "status": "passed",
                "exit_code": 0,
                "test_count": count,
                "log_sha256": hashlib.sha256(raw).hexdigest(),
            }
        )
    executed_suites = [item for item in suite_items if item["name"] in expected]
    sdks, transports = _coverage(executed_suites)
    return {
        "schema": "hyphae-native-g6-receipt-v1",
        "gate": "G6",
        "status": "passed",
        "evidence_class": "supporting-not-closure",
        "source_commit": source_commit,
        "requirement": requirement,
        "manifest_sha256": dict(manifest_sha256),
        "authority": {
            "scope": authority.get("scope"),
            "evidence_class": authority.get("evidence_class"),
            "identity_sha256": _canonical_sha256(authority),
        },
        "workload": {
            "id": workload["id"],
            "oracle": workload["oracle"],
            "acceptance": workload["acceptance"],
            "identity_sha256": _canonical_sha256(workload),
        },
        "suite_identity_sha256": _canonical_sha256(suite_row),
        "platform": platform,
        "tool_versions": dict(sorted(tool_versions.items())),
        "sdks": sdks,
        "transports": transports,
        "command_results": results,
        "test_count": total,
        "implementation_status": suite_row["status"],
        "uncovered_acceptance": uncovered,
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--requirement", required=True)
    for name in MANIFEST_NAMES:
        parser.add_argument(f"--{name}", type=Path, required=True)
        parser.add_argument(f"--{name}-sha256", required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--tool-version", action="append", nargs=2, metavar=("TOOL", "VERSION"), required=True)
    parser.add_argument("--suite-log", action="append", nargs=2, metavar=("NAME", "PATH"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        raw = {name: getattr(args, name).read_bytes() for name in MANIFEST_NAMES}
        digests = {name: getattr(args, f"{name}_sha256") for name in MANIFEST_NAMES}
        tool_versions = dict(args.tool_version)
        if len(tool_versions) != len(args.tool_version):
            raise GateFailure("duplicate G6 tool version")
        result = build_receipt(
            args.source_commit,
            args.requirement,
            raw,
            digests,
            args.platform,
            tool_versions,
            [(name, Path(path).read_bytes()) for name, path in args.suite_log],
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError, TypeError) as error:
        print(f"native G6 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
