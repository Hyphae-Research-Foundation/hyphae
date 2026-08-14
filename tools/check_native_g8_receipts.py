#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation and aggregation of exact-SHA G8 receipts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def load(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise GateFailure(f"{path} must contain one JSON object")
    return value


def authority(root: Path) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    profile = load(root / "config/native-g8-readiness-profile.json")
    manifest = load(root / "config/native-g8-suite-manifest.json")
    if profile.get("schema") != "hyphae-native-g8-readiness-profile-v2":
        raise GateFailure("invalid G8 readiness profile")
    if manifest.get("schema") != "hyphae-native-g8-suite-manifest-v2":
        raise GateFailure("invalid G8 suite manifest")
    if (
        profile.get("gate") != "G8"
        or profile.get("claims") != []
        or profile.get("closure_declared") is not False
        or profile.get("artifact_digest") != "sha256"
        or profile.get("receipt_schema") != "hyphae-native-g8-receipt-v1"
        or manifest.get("gate") != "G8"
        or manifest.get("claims") != []
        or manifest.get("closure_declared") is not False
    ):
        raise GateFailure("G8 authority is not open and fail-closed")
    requirements = profile.get("required_requirements")
    rows = manifest.get("requirements")
    if not isinstance(requirements, list) or not isinstance(rows, list):
        raise GateFailure("G8 authority requirements are malformed")
    if [row.get("id") for row in rows] != requirements:
        raise GateFailure("G8 authority requirement identities drifted")
    for row in rows:
        if (
            not isinstance(row, dict)
            or set(row) != {"id", "status", "platforms", "runner", "acceptance"}
            or row.get("status") != "implemented-unhosted"
            or not isinstance(row.get("runner"), str)
            or not row["runner"]
            or not isinstance(row.get("platforms"), list)
            or not row["platforms"]
            or len(row["platforms"]) != len(set(row["platforms"]))
            or not isinstance(row.get("acceptance"), list)
            or not row["acceptance"]
            or len(row["acceptance"]) != len(set(row["acceptance"]))
        ):
            raise GateFailure("G8 suite authority row is malformed")
    return profile, {row["id"]: row for row in rows}


def validate_receipt(
    receipt: dict[str, Any], expected_commit: str, row: dict[str, Any]
) -> dict[str, Any]:
    expected_fields = {
        "schema", "gate", "status", "evidence_class", "source_commit",
        "requirement", "platform", "acceptance", "artifacts", "claims",
        "closure_declared",
    }
    if set(receipt) != expected_fields:
        raise GateFailure(f"G8 receipt fields differ for {row['id']}")
    if (
        receipt["schema"] != "hyphae-native-g8-receipt-v1"
        or receipt["gate"] != "G8"
        or receipt["status"] != "passed"
        or receipt["evidence_class"] != "closure-candidate"
        or receipt["source_commit"] != expected_commit
        or receipt["requirement"] != row["id"]
        or receipt["claims"] != []
        or receipt["closure_declared"] is not False
    ):
        raise GateFailure(f"invalid G8 receipt identity for {row['id']}")
    if receipt["platform"] not in row["platforms"]:
        raise GateFailure(f"unexpected G8 platform for {row['id']}")
    artifacts = receipt["artifacts"]
    if not isinstance(artifacts, list) or len(artifacts) != 1:
        raise GateFailure(f"G8 receipt must bind one raw suite artifact for {row['id']}")
    names: set[str] = set()
    for artifact in artifacts:
        if (
            not isinstance(artifact, dict)
            or set(artifact) != {"name", "sha256"}
            or not isinstance(artifact["name"], str)
            or not artifact["name"]
            or Path(artifact["name"]).name != artifact["name"]
            or artifact["name"] in names
            or HEX64.fullmatch(artifact["sha256"]) is None
        ):
            raise GateFailure(f"malformed G8 artifact for {row['id']}")
        names.add(artifact["name"])
    artifact_digests = {artifact["sha256"] for artifact in artifacts}
    acceptance = receipt["acceptance"]
    if not isinstance(acceptance, dict) or set(acceptance) != set(row["acceptance"]):
        raise GateFailure(f"incomplete G8 acceptance for {row['id']}")
    for name, evidence in acceptance.items():
        if (
            not isinstance(evidence, dict)
            or evidence.get("status") != "passed"
            or evidence.get("artifact_sha256") not in artifact_digests
            or len(evidence) < 3
        ):
            raise GateFailure(f"unbound G8 acceptance evidence: {row['id']}/{name}")
    return {
        "schema": "hyphae-native-g8-receipt-audit-v1",
        "status": "passed",
        "source_commit": expected_commit,
        "requirement": row["id"],
        "platform": receipt["platform"],
    }


def aggregate(repository: Path, receipts: Path, expected_commit: str) -> dict[str, Any]:
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("expected commit is not a canonical SHA-1")
    profile, rows = authority(repository)
    seen: dict[str, dict[str, dict[str, Any]]] = {}
    receipt_files = sorted(receipts.rglob("*-receipt.json"))
    if not receipt_files:
        raise GateFailure("no G8 receipts were supplied")
    for path in receipt_files:
        if path.is_symlink() or not path.is_file():
            raise GateFailure(f"G8 receipt is not a regular file: {path}")
        payload = load(path)
        requirement = payload.get("requirement")
        if requirement not in rows:
            raise GateFailure(f"unknown G8 requirement in {path}")
        audit = validate_receipt(payload, expected_commit, rows[requirement])
        artifact = payload["artifacts"][0]
        artifact_path = path.parent / artifact["name"]
        if artifact_path.is_symlink() or not artifact_path.is_file():
            raise GateFailure(f"missing bound G8 artifact: {artifact_path}")
        actual_digest = hashlib.sha256(artifact_path.read_bytes()).hexdigest()
        if actual_digest != artifact["sha256"]:
            raise GateFailure(f"G8 artifact digest mismatch: {artifact_path}")
        from tools.produce_native_g8_receipt import observations

        raw_artifact = load(artifact_path)
        verified = observations(
            requirement, raw_artifact, expected_commit, audit["platform"]
        )
        for name, evidence in payload["acceptance"].items():
            if evidence.get("observation") != verified.get(name):
                raise GateFailure(
                    f"G8 semantic observation mismatch: {requirement}/{name}"
                )
        platform = audit["platform"]
        if platform in seen.setdefault(requirement, {}):
            raise GateFailure(f"duplicate G8 receipt for {requirement}/{platform}")
        seen[requirement][platform] = {
            "receipt_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "audit": audit,
        }
    for requirement, row in rows.items():
        expected_platforms = set(row["platforms"])
        actual_platforms = set(seen.get(requirement, {}))
        if actual_platforms != expected_platforms:
            raise GateFailure(
                f"G8 platform coverage differs for {requirement}: "
                f"expected={sorted(expected_platforms)}, actual={sorted(actual_platforms)}"
            )
    return {
        "schema": "hyphae-native-g8-aggregate-v1",
        "gate": "G8",
        "status": "passed",
        "source_commit": expected_commit,
        "requirements": seen,
        "required_platforms": profile["required_platforms"],
        "claims": ["G8"],
        "closure_declared": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--receipts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        result = aggregate(
            Path(__file__).resolve().parents[1], arguments.receipts, arguments.expected_commit
        )
        arguments.output.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (ValueError, OSError) as error:
        print(f"native G8 receipts failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
