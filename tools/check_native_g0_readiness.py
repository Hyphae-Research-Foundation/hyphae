#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Fail-closed readiness aggregation for the Hyphae native G0 gate."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

PROFILE_SCHEMA = "hyphae-native-g0-profile-v1"
EVIDENCE_LEVELS = (
    "contract",
    "tiny-executable",
    "local-integration",
    "target-native",
    "hosted",
    "production",
    "external-governance",
)
EVIDENCE_STATUSES = {"passed", "failed", "not-configured", "blocked"}
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(RuntimeError):
    """The readiness profile or supplied evidence is malformed or inconsistent."""


def _mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _requirements(profile: dict[str, Any]) -> list[dict[str, str]]:
    if profile.get("schema") != PROFILE_SCHEMA or profile.get("gate") != "G0":
        raise GateFailure("unsupported G0 readiness profile")
    raw = profile.get("requirements")
    if not isinstance(raw, list) or not raw:
        raise GateFailure("requirements must be a nonempty array")
    requirements: list[dict[str, str]] = []
    seen: set[str] = set()
    for value in raw:
        entry = _mapping(value, "requirement")
        requirement_id = entry.get("id")
        level = entry.get("required_evidence_level")
        if not isinstance(requirement_id, str) or not requirement_id:
            raise GateFailure("requirement id must be a nonempty string")
        if requirement_id in seen:
            raise GateFailure(f"duplicate requirement {requirement_id}")
        if level not in EVIDENCE_LEVELS:
            raise GateFailure(f"invalid evidence level for {requirement_id}")
        if set(entry) != {"id", "required_evidence_level"}:
            raise GateFailure(f"unknown requirement field for {requirement_id}")
        seen.add(requirement_id)
        requirements.append(
            {"id": requirement_id, "required_evidence_level": str(level)}
        )
    return requirements


def evaluate_readiness(
    profile: dict[str, Any], evidence: dict[str, Any]
) -> dict[str, Any]:
    """Evaluate exact evidence without promoting absent or lower-scope results."""

    requirements = _requirements(_mapping(profile, "profile"))
    evidence_map = _mapping(evidence, "evidence")
    requirement_ids = {entry["id"] for entry in requirements}
    unknown = set(evidence_map) - requirement_ids
    if unknown:
        raise GateFailure("unknown evidence: " + ", ".join(sorted(unknown)))

    rows: list[dict[str, Any]] = []
    for requirement in requirements:
        requirement_id = requirement["id"]
        required_level = requirement["required_evidence_level"]
        value = evidence_map.get(requirement_id)
        if value is None:
            rows.append(
                {
                    "id": requirement_id,
                    "required_evidence_level": required_level,
                    "status": "not-configured",
                    "artifact": None,
                }
            )
            continue
        record = _mapping(value, f"evidence {requirement_id}")
        required_fields = {"status", "evidence_level", "artifact"}
        allowed_fields = required_fields | {"artifact_sha256"}
        if not required_fields <= set(record) or not set(record) <= allowed_fields:
            raise GateFailure(f"invalid evidence fields for {requirement_id}")
        status = record.get("status")
        level = record.get("evidence_level")
        artifact = record.get("artifact")
        artifact_sha256 = record.get("artifact_sha256")
        if status not in EVIDENCE_STATUSES:
            raise GateFailure(f"invalid evidence status for {requirement_id}")
        if level not in EVIDENCE_LEVELS:
            raise GateFailure(f"invalid evidence level for {requirement_id}")
        if not isinstance(artifact, str) or not artifact:
            raise GateFailure(f"evidence artifact required for {requirement_id}")
        if status == "passed" and (
            not isinstance(artifact_sha256, str) or SHA256.fullmatch(artifact_sha256) is None
        ):
            raise GateFailure(f"canonical SHA-256 required for passed evidence {requirement_id}")
        if status == "passed" and EVIDENCE_LEVELS.index(level) < EVIDENCE_LEVELS.index(
            required_level
        ):
            row_status = "insufficient-evidence"
        else:
            row_status = status
        row = {
            "id": requirement_id,
            "required_evidence_level": required_level,
            "status": row_status,
            "artifact": artifact,
        }
        if artifact_sha256 is not None:
            row["artifact_sha256"] = artifact_sha256
        rows.append(row)

    statuses = {row["status"] for row in rows}
    if statuses == {"passed"}:
        overall = "passed"
    elif "failed" in statuses or "insufficient-evidence" in statuses:
        overall = "failed"
    elif "blocked" in statuses:
        overall = "blocked"
    else:
        overall = "not-configured"
    return {
        "schema": "hyphae-native-g0-readiness-v1",
        "gate": "G0",
        "status": overall,
        "required": len(rows),
        "passed": sum(row["status"] == "passed" for row in rows),
        "requirements": rows,
    }


def inject_evidence(
    baseline: dict[str, Any],
    requirement_id: str,
    source_artifact: Path,
    evidence_level: str,
    artifact_reference: str,
) -> dict[str, Any]:
    """Return a baseline copy with one validated, content-bound passed receipt."""

    if requirement_id not in baseline:
        raise GateFailure(f"unknown requirement for evidence injection: {requirement_id}")
    if evidence_level not in EVIDENCE_LEVELS:
        raise GateFailure(f"invalid injected evidence level: {evidence_level}")
    if not source_artifact.is_file():
        raise GateFailure(f"missing evidence artifact: {source_artifact}")
    try:
        receipt = json.loads(source_artifact.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GateFailure("injected evidence artifact is not valid JSON") from error
    if not isinstance(receipt, dict) or receipt.get("status") != "passed":
        raise GateFailure("injected evidence receipt is not passed")
    if not artifact_reference or Path(artifact_reference).is_absolute():
        raise GateFailure("injected artifact reference must be a relative path")
    updated = json.loads(json.dumps(baseline))
    updated[requirement_id] = {
        "status": "passed",
        "evidence_level": evidence_level,
        "artifact": artifact_reference,
        "artifact_sha256": hashlib.sha256(source_artifact.read_bytes()).hexdigest(),
    }
    return updated


def validate_passed_artifacts(root: Path, result: dict[str, Any]) -> None:
    """Require every passed artifact to exist under root and match its binding."""

    resolved_root = root.resolve()
    for row in result["requirements"]:
        if row["status"] != "passed":
            continue
        artifact = row["artifact"]
        digest = row.get("artifact_sha256")
        path = (resolved_root / artifact).resolve()
        try:
            path.relative_to(resolved_root)
        except ValueError as error:
            raise GateFailure(f"passed artifact escapes repository root: {artifact}") from error
        if not path.is_file():
            raise GateFailure(f"passed artifact is missing: {artifact}")
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != digest:
            raise GateFailure(f"passed artifact SHA-256 mismatch: {artifact}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--inject-requirement")
    parser.add_argument("--inject-artifact", type=Path)
    parser.add_argument("--inject-level", choices=EVIDENCE_LEVELS)
    parser.add_argument("--inject-reference")
    parser.add_argument("--evidence-output", type=Path)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        evidence = json.loads(args.evidence.read_text(encoding="utf-8"))
        injection = (
            args.inject_requirement,
            args.inject_artifact,
            args.inject_level,
            args.inject_reference,
        )
        if any(value is not None for value in injection):
            if not all(value is not None for value in injection):
                raise GateFailure("evidence injection requires all injection arguments")
            evidence = inject_evidence(
                evidence,
                args.inject_requirement,
                args.inject_artifact,
                args.inject_level,
                args.inject_reference,
            )
        if args.evidence_output is not None:
            args.evidence_output.write_text(
                json.dumps(evidence, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
        result = evaluate_readiness(
            json.loads(args.profile.read_text(encoding="utf-8")),
            evidence,
        )
        validate_passed_artifacts(args.root, result)
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native G0 readiness failed: {error}")
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
