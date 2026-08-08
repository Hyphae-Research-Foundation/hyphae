#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Evaluate complete G6 hosted evidence while keeping the candidate open."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from tools.check_native_g6_foundation import PLATFORMS, PREDECESSORS, REQUIREMENTS, SDKS, TRANSPORTS, WORKLOAD_ACCEPTANCE, GateFailure, validate_suite_command
from tools.check_native_g6_manifests import MANIFEST_NAMES
from tools.check_native_g6_receipt import AUDIT_FIELDS
from tools.produce_native_g6_receipt import TOOL_NAME, _canonical_sha256, _coverage

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
PROFILE_FIELDS = {"schema", "gate", "scope", "requirements", "required_platforms", "required_sdks", "required_transports", "claims", "closure_declared"}
EVIDENCE_FIELDS = {"schema", "gate", "predecessor", "evidence", "claims", "closure_declared"}
EVIDENCE_ROW_FIELDS = {"status", "level", "reference", "artifact_sha256"}
MANIFEST_AUDIT_FIELDS = {
    "schema", "gate", "status", "evidence_class", "source_commit", "manifest_sha256",
    "requirements", "implemented_requirements", "partial_requirements", "planned_requirements",
    "predecessors", "predecessor_count", "closure_status", "claims", "closure_declared",
}


def _artifact(root: Path, row: object, label: str) -> dict[str, Any]:
    if (
        not isinstance(row, dict)
        or set(row) != EVIDENCE_ROW_FIELDS
        or row.get("status") != "passed"
        or row.get("level") not in {"hosted", "retained"}
        or not isinstance(row.get("reference"), str)
        or not row["reference"]
        or HEX64.fullmatch(row.get("artifact_sha256", "")) is None
    ):
        raise GateFailure(f"invalid G6 {label} evidence fields")
    reference = Path(row["reference"])
    if reference.is_absolute() or ".." in reference.parts:
        raise GateFailure(f"unsafe G6 {label} evidence reference")
    resolved_root = root.resolve()
    artifact = (root / reference).resolve()
    try:
        artifact.relative_to(resolved_root)
    except ValueError as error:
        raise GateFailure(f"G6 {label} evidence escapes the root") from error
    if not artifact.is_file():
        raise GateFailure(f"missing G6 {label} evidence")
    raw = artifact.read_bytes()
    if hashlib.sha256(raw).hexdigest() != row["artifact_sha256"]:
        raise GateFailure(f"mismatched G6 {label} evidence")
    payload = json.loads(raw)
    if not isinstance(payload, dict):
        raise GateFailure(f"G6 {label} evidence must be an object")
    return payload


def evaluate(
    root: Path,
    profile: dict[str, Any],
    evidence: dict[str, Any],
    expected_commit: str,
    manifest_sha256: dict[str, str],
    suite_manifest: dict[str, Any],
) -> dict[str, Any]:
    if (
        set(profile) != PROFILE_FIELDS
        or profile.get("schema") != "hyphae-native-g6-readiness-profile-v1"
        or profile.get("gate") != "G6"
        or profile.get("scope") != "competitive-local-product"
        or set(evidence) != EVIDENCE_FIELDS
        or evidence.get("schema") != "hyphae-native-g6-readiness-evidence-v1"
        or evidence.get("gate") != "G6"
        or profile.get("claims") != []
        or profile.get("closure_declared") is not False
        or evidence.get("claims") != []
        or evidence.get("closure_declared") is not False
    ):
        raise GateFailure("unsupported, claiming, or closed G6 readiness inputs")
    if (
        HEX40.fullmatch(expected_commit) is None
        or set(manifest_sha256) != set(MANIFEST_NAMES)
        or any(HEX64.fullmatch(value) is None for value in manifest_sha256.values())
    ):
        raise GateFailure("invalid G6 exact identities")
    profile_rows = profile.get("requirements")
    if (
        not isinstance(profile_rows, list)
        or len(profile_rows) != len(REQUIREMENTS)
        or [row.get("id") for row in profile_rows if isinstance(row, dict)] != REQUIREMENTS
        or any(set(row) != {"id", "required_evidence"} or row["required_evidence"] != "hosted" for row in profile_rows)
    ):
        raise GateFailure("G6 readiness requires all fourteen ordered hosted rows")
    if profile.get("required_platforms") != PLATFORMS or profile.get("required_sdks") != SDKS or profile.get("required_transports") != TRANSPORTS:
        raise GateFailure("G6 readiness platform, SDK, or transport contract mismatch")
    evidence_rows = evidence.get("evidence")
    if not isinstance(evidence_rows, dict) or set(evidence_rows) - set(REQUIREMENTS):
        raise GateFailure("G6 readiness contains unknown requirement evidence")
    suite_rows = suite_manifest.get("requirements")
    if (
        set(suite_manifest) != {"schema", "gate", "requirements", "claims", "closure_declared"}
        or suite_manifest.get("schema") != "hyphae-native-g6-suite-manifest-v1"
        or suite_manifest.get("gate") != "G6"
        or suite_manifest.get("claims") != []
        or suite_manifest.get("closure_declared") is not False
        or not isinstance(suite_rows, list)
        or [row.get("id") for row in suite_rows if isinstance(row, dict)] != REQUIREMENTS
    ):
        raise GateFailure("invalid G6 suite manifest for readiness")
    suites_by_requirement = {row["id"]: row for row in suite_rows}

    predecessor_status = "not-configured"
    predecessors_passed = 0
    predecessor_row = evidence.get("predecessor")
    if predecessor_row is not None:
        payload = _artifact(root, predecessor_row, "predecessor")
        if (
            predecessor_row["level"] != "retained"
            or set(payload) != MANIFEST_AUDIT_FIELDS
            or payload.get("schema") != "hyphae-native-g6-manifest-audit-v1"
            or payload.get("gate") != "G6"
            or payload.get("status") != "passed"
            or payload.get("evidence_class") != "authority-not-closure"
            or payload.get("source_commit") != expected_commit
            or payload.get("manifest_sha256") != manifest_sha256
            or payload.get("requirements") != len(REQUIREMENTS)
            or not isinstance(payload.get("implemented_requirements"), int)
            or isinstance(payload.get("implemented_requirements"), bool)
            or not isinstance(payload.get("partial_requirements"), int)
            or isinstance(payload.get("partial_requirements"), bool)
            or payload["implemented_requirements"] + payload["partial_requirements"] != len(REQUIREMENTS)
            or payload.get("planned_requirements") != 0
            or payload.get("predecessor_count") != len(PREDECESSORS)
            or payload.get("closure_status") != "open"
            or payload.get("claims") != []
            or payload.get("closure_declared") is not False
        ):
            raise GateFailure("invalid G6 predecessor manifest audit")
        predecessors = payload.get("predecessors")
        if (
            not isinstance(predecessors, list)
            or [row.get("gate") for row in predecessors if isinstance(row, dict)] != PREDECESSORS
            or any(
                set(row) != {"gate", "source_commit", "artifact_sha256"}
                or HEX40.fullmatch(row.get("source_commit", "")) is None
                or HEX64.fullmatch(row.get("artifact_sha256", "")) is None
                for row in predecessors
            )
        ):
            raise GateFailure("invalid G6 retained predecessor identities")
        predecessor_status = "passed"
        predecessors_passed = len(predecessors)

    implemented_platforms: set[str] = set()
    sdks: set[str] = set()
    transports: set[str] = set()
    matrix_cells_passed = 0
    results: list[dict[str, str]] = []
    for requirement in REQUIREMENTS:
        status = "not-configured"
        if requirement in evidence_rows:
            matrix_row = evidence_rows[requirement]
            if not isinstance(matrix_row, dict) or set(matrix_row) != set(PLATFORMS):
                raise GateFailure(f"incomplete G6 platform matrix for {requirement}")
            implementation_status: str | None = None
            uncovered_acceptance: list[str] | None = None
            identity: tuple[object, object, object] | None = None
            requirement_sdks: set[str] = set()
            requirement_transports: set[str] = set()
            suite_row = suites_by_requirement[requirement]
            if not isinstance(suite_row.get("suites"), list) or not suite_row["suites"]:
                raise GateFailure(f"invalid G6 suite authority for {requirement}")
            for platform in PLATFORMS:
                row = matrix_row[platform]
                payload = _artifact(root, row, f"{requirement}/{platform}")
                expected_suites = [
                    item
                    for item in suite_row["suites"]
                    if isinstance(item, dict) and platform in item.get("platforms", PLATFORMS)
                ]
                expected_commands = {
                    item.get("name"): item.get("platform_commands", {}).get(platform, item.get("command"))
                    for item in expected_suites
                }
                expected_sdks, expected_transports = _coverage(expected_suites)
                if (
                    row["level"] != "hosted"
                    or set(payload) != AUDIT_FIELDS
                    or payload.get("schema") != "hyphae-native-g6-receipt-audit-v1"
                    or payload.get("gate") != "G6"
                    or payload.get("status") != "passed"
                    or payload.get("evidence_class") != "supporting-not-closure"
                    or payload.get("source_commit") != expected_commit
                    or payload.get("requirement") != requirement
                    or payload.get("manifest_sha256") != manifest_sha256
                    or payload.get("claims") != []
                    or payload.get("closure_declared") is not False
                    or payload.get("suite_identity_sha256") != _canonical_sha256(suite_row)
                    or payload.get("implementation_status") != suite_row.get("status")
                    or payload.get("uncovered_acceptance") != suite_row.get("uncovered_acceptance")
                    or not isinstance(payload.get("suite_count"), int)
                    or isinstance(payload.get("suite_count"), bool)
                    or payload["suite_count"] != len(expected_suites)
                    or not isinstance(payload.get("test_count"), int)
                    or isinstance(payload.get("test_count"), bool)
                    or payload["test_count"] <= 0
                    or payload.get("platform") != platform
                    or payload.get("sdks") != expected_sdks
                    or payload.get("transports") != expected_transports
                ):
                    raise GateFailure(f"invalid G6 hosted audit for {requirement}/{platform}")
                authority = payload.get("authority")
                workload = payload.get("workload")
                tools = payload.get("tool_versions")
                command_results = payload.get("command_results")
                if (
                    not isinstance(authority, dict)
                    or set(authority) != {"scope", "evidence_class", "identity_sha256"}
                    or not isinstance(authority["scope"], str)
                    or not authority["scope"]
                    or not isinstance(authority["evidence_class"], str)
                    or not authority["evidence_class"]
                    or HEX64.fullmatch(authority["identity_sha256"]) is None
                    or not isinstance(workload, dict)
                    or set(workload) != {"id", "oracle", "acceptance", "identity_sha256"}
                    or not isinstance(workload["id"], str)
                    or not workload["id"]
                    or not isinstance(workload["oracle"], str)
                    or not workload["oracle"]
                    or not isinstance(workload["acceptance"], list)
                    or not workload["acceptance"]
                    or len(workload["acceptance"]) != len(set(workload["acceptance"]))
                    or set(workload["acceptance"]) != WORKLOAD_ACCEPTANCE[requirement]
                    or HEX64.fullmatch(workload["identity_sha256"]) is None
                    or not isinstance(tools, dict)
                    or not tools
                    or any(TOOL_NAME.fullmatch(name) is None or not isinstance(version, str) or not version.strip() for name, version in tools.items())
                    or not isinstance(command_results, list)
                    or len(command_results) != payload["suite_count"]
                ):
                    raise GateFailure(f"incomplete G6 hosted audit for {requirement}/{platform}")
                names: set[str] = set()
                test_count = 0
                for command_result in command_results:
                    if not isinstance(command_result, dict) or set(command_result) != {"name", "command", "command_sha256", "status", "exit_code", "test_count", "log_sha256"}:
                        raise GateFailure(f"invalid G6 command audit for {requirement}/{platform}")
                    name = command_result["name"]
                    command = command_result["command"]
                    tests = command_result["test_count"]
                    try:
                        validate_suite_command(command)
                    except GateFailure as error:
                        raise GateFailure(f"invalid G6 command audit for {requirement}/{platform}: {error}") from error
                    if (
                        not isinstance(name, str)
                        or not name
                        or name in names
                        or name not in expected_commands
                        or command != expected_commands[name]
                        or command_result["command_sha256"] != _canonical_sha256(command)
                        or command_result["status"] != "passed"
                        or command_result["exit_code"] != 0
                        or not isinstance(tests, int)
                        or isinstance(tests, bool)
                        or tests <= 0
                        or HEX64.fullmatch(command_result.get("log_sha256", "")) is None
                    ):
                        raise GateFailure(f"invalid G6 command audit for {requirement}/{platform}")
                    names.add(name)
                    test_count += tests
                if (
                    names != set(expected_commands)
                    or not {result["command"][0] for result in command_results}.issubset(tools)
                    or payload["test_count"] != test_count
                ):
                    raise GateFailure(f"mismatched G6 command audit for {requirement}/{platform}")
                current_identity = (payload["authority"], payload["workload"], payload["suite_identity_sha256"])
                if identity is not None and current_identity != identity:
                    raise GateFailure(f"mismatched G6 matrix identity for {requirement}")
                if implementation_status is not None and (
                    payload["implementation_status"] != implementation_status
                    or payload["uncovered_acceptance"] != uncovered_acceptance
                ):
                    raise GateFailure(f"mismatched G6 matrix coverage for {requirement}")
                identity = current_identity
                implementation_status = payload["implementation_status"]
                uncovered_acceptance = payload["uncovered_acceptance"]
                requirement_sdks.update(payload["sdks"])
                requirement_transports.update(payload["transports"])
                matrix_cells_passed += 1
            if implementation_status == "implemented-unhosted":
                implemented_platforms.update(PLATFORMS)
                sdks.update(requirement_sdks)
                transports.update(requirement_transports)
                status = "passed"
            else:
                status = "implementation-incomplete"
        results.append({"id": requirement, "status": status, "required_evidence": "hosted"})
    passed = sum(row["status"] == "passed" for row in results)
    coverage_complete = implemented_platforms == set(PLATFORMS) and sdks == set(SDKS) and transports == set(TRANSPORTS)
    candidate = predecessor_status == "passed" and passed == len(REQUIREMENTS) and matrix_cells_passed == len(REQUIREMENTS) * len(PLATFORMS) and coverage_complete
    return {
        "schema": "hyphae-native-g6-readiness-v1",
        "gate": "G6",
        "status": "candidate-evidence-complete" if candidate else "not-ready",
        "source_commit": expected_commit,
        "manifest_sha256": dict(manifest_sha256),
        "predecessor_status": predecessor_status,
        "predecessors_required": len(PREDECESSORS),
        "predecessors_passed": predecessors_passed,
        "required": len(REQUIREMENTS),
        "passed": passed,
        "matrix_cells_required": len(REQUIREMENTS) * len(PLATFORMS),
        "matrix_cells_passed": matrix_cells_passed,
        "platforms_required": PLATFORMS,
        "platforms_passed": [value for value in PLATFORMS if value in implemented_platforms],
        "sdks_required": SDKS,
        "sdks_passed": [value for value in SDKS if value in sdks],
        "transports_required": TRANSPORTS,
        "transports_passed": [value for value in TRANSPORTS if value in transports],
        "requirements": results,
        "closure_status": "open",
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--manifest-root", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    for name in MANIFEST_NAMES:
        parser.add_argument(f"--{name}-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        digests = {name: getattr(args, f"{name}_sha256") for name in MANIFEST_NAMES}
        files = {
            "profile": "native-g6-readiness-profile.json",
            "evidence": "native-g6-readiness-evidence.json",
            "inventory": "native-g6-inventory.json",
            "authority": "native-g6-authority-manifest.json",
            "workload": "native-g6-workload-manifest.json",
            "suite": "native-g6-suite-manifest.json",
            "predecessor": "native-g6-predecessor-manifest.json",
        }
        manifest_raw = {name: (args.manifest_root / filename).read_bytes() for name, filename in files.items()}
        for name, raw in manifest_raw.items():
            if hashlib.sha256(raw).hexdigest() != digests[name]:
                raise GateFailure(f"G6 {name} manifest digest mismatch")
        if args.profile.read_bytes() != manifest_raw["profile"]:
            raise GateFailure("G6 readiness profile differs from exact manifest")
        result = evaluate(
            args.root,
            json.loads(manifest_raw["profile"]),
            json.loads(args.evidence.read_text(encoding="utf-8")),
            args.expected_commit,
            digests,
            json.loads(manifest_raw["suite"]),
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError, TypeError) as error:
        print(f"native G6 readiness failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if result["status"] == "candidate-evidence-complete" else 1


if __name__ == "__main__":
    raise SystemExit(main())
