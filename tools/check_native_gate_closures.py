#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Validate the retained, ordered Native Phase 1 gate closure prefix."""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
EXPECTED_GATES = [f"G{index}" for index in range(9)]
EXPECTED_SCHEMAS = {
    "G0": "hyphae-native-g0-closure-v1",
    "G1": "hyphae-native-g1-closure-v1",
    "G2": "hyphae-native-g2-readiness-v1",
    "G3": "hyphae-native-g3-readiness-v1",
    "G4": "hyphae-native-g4-closure-v1",
    "G5": "hyphae-native-g5-closure-v1",
    "G6": "hyphae-native-g6-closure-v1",
    "G7": "hyphae-native-g7-closure-v1",
}
CANONICAL_G7_PROFILE_SHA256 = (
    "421e96da451c726dce293f26c795b862dba148cd408a7a2634cb4e70b97367f6"
)
G7_C60_EVIDENCE = (
    "docs/gates/evidence/native-g7-provisional-do-c60-2026-08-13.json"
)


class GateFailure(ValueError):
    pass


def _object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _load(path: Path, label: str) -> dict[str, Any]:
    try:
        return _object(json.loads(path.read_text(encoding="utf-8")), label)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateFailure(f"cannot load {label}: {path}") from error


def _profile_requirements(root: Path, gate: str) -> list[str]:
    profile = _load(
        root / "config" / f"native-{gate.lower()}-readiness-profile.json",
        f"{gate} readiness profile",
    )
    if profile.get("gate") != gate:
        raise GateFailure(f"{gate} readiness profile identity mismatch")
    if gate == "G7":
        requirements = profile.get("required_cells")
        if (
            not isinstance(requirements, list)
            or not requirements
            or any(not isinstance(value, str) or not value for value in requirements)
            or len(requirements) != len(set(requirements))
        ):
            raise GateFailure("G7 readiness profile has invalid required cells")
        return requirements
    requirements = profile.get("requirements")
    if not isinstance(requirements, list) or not requirements:
        raise GateFailure(f"{gate} readiness profile has no requirements")
    identifiers = []
    for row in requirements:
        identifier = _object(row, f"{gate} profile requirement").get("id")
        if not isinstance(identifier, str) or not identifier:
            raise GateFailure(f"{gate} profile has an invalid requirement")
        identifiers.append(identifier)
    if len(identifiers) != len(set(identifiers)):
        raise GateFailure(f"{gate} readiness profile has duplicate requirements")
    return identifiers


def _validate_g7_identity(closure: dict[str, Any]) -> tuple[str, str]:
    expected_fields = {
        "schema", "gate", "status", "source_commit", "source_tree",
        "authority_date", "authority_scope", "evidence_class",
        "production_scale", "required", "passed", "requirements",
        "matrix_cells_required", "matrix_cells_passed", "contract",
        "contract_profile_sha256", "retained_evidence", "artifact",
        "artifact_sha256", "matrix_sha256", "checksums_sha256",
        "runner_override_sha256", "authority_execution",
        "bare_metal_diagnostic", "canonical_latency_certified",
        "dedicated_hardware_certified", "background_interference_certified",
        "non_claims", "claims", "closure_declared",
    }
    if set(closure) != expected_fields:
        raise GateFailure("G7 retained closure fields mismatch")
    source_commit = closure.get("source_commit")
    source_tree = closure.get("source_tree")
    if (
        closure.get("schema") != "hyphae-native-g7-closure-v1"
        or closure.get("gate") != "G7"
        or closure.get("status") != "passed"
        or source_commit != "ff188af589eff1f6f15ac4f2e782b43f0868fa21"
        or source_tree != "d9f6493e31b8d4c139937dd57092908421256de6"
        or closure.get("authority_date") != "2026-08-14"
        or closure.get("authority_scope") != "operational-scale-performance"
        or closure.get("evidence_class")
        != "virtual-machine-operational-scale-authority-v1"
        or closure.get("production_scale") is not True
        or closure.get("artifact")
        != "native-g7-provisional-do-c60-2026-08-13-bundle"
        or closure.get("non_claims") != [
            "canonical dedicated-hardware latency certification",
            "background-interference certification",
            "bare-metal qualification",
        ]
        or closure.get("claims") != ["G7"]
        or closure.get("closure_declared") is not True
    ):
        raise GateFailure("G7 C-60 closure identity mismatch")
    if (
        closure.get("canonical_latency_certified") is not False
        or closure.get("dedicated_hardware_certified") is not False
        or closure.get("background_interference_certified") is not False
    ):
        raise GateFailure("G7 C-60 closure overclaims its environment")
    return source_commit, source_tree


def _validate_g7_profile(root: Path, closure: dict[str, Any]) -> list[str]:
    profile_path = root / "config/native-g7-readiness-profile.json"
    profile_digest = hashlib.sha256(profile_path.read_bytes()).hexdigest()
    if (
        profile_digest != CANONICAL_G7_PROFILE_SHA256
        or closure.get("contract_profile_sha256") != profile_digest
    ):
        raise GateFailure("G7 canonical profile or threshold binding drifted")
    requirements = _profile_requirements(root, "G7")
    if (
        closure.get("requirements") != requirements
        or closure.get("required") != len(requirements)
        or closure.get("passed") != len(requirements)
        or closure.get("matrix_cells_required") != len(requirements) * 3
        or closure.get("matrix_cells_passed") != len(requirements) * 3
    ):
        raise GateFailure("G7 C-60 closure coverage mismatch")
    return requirements


def _validate_g7_source_evidence(
    root: Path,
    closure: dict[str, Any],
    source_commit: str,
    source_tree: str,
    requirements: list[str],
) -> dict[str, Any]:
    retained = _object(closure.get("retained_evidence"), "G7 retained evidence")
    if set(retained) != {"path", "sha256"} or retained.get("path") != G7_C60_EVIDENCE:
        raise GateFailure("G7 C-60 retained evidence reference mismatch")
    retained_path = root / G7_C60_EVIDENCE
    retained_digest = hashlib.sha256(retained_path.read_bytes()).hexdigest()
    if retained.get("sha256") != retained_digest:
        raise GateFailure("G7 C-60 retained evidence digest mismatch")
    evidence = _load(retained_path, "G7 C-60 retained evidence")
    if (
        evidence.get("schema") != "hyphae-native-g7-provisional-verdict-v1"
        or evidence.get("status") != "provisional-passed"
        or evidence.get("canonical_g7_status") != "open"
        or evidence.get("source_commit") != source_commit
        or evidence.get("source_tree") != source_tree
        or evidence.get("claims") != ["G7-provisional-control"]
        or evidence.get("closure_declared") is not False
    ):
        raise GateFailure("G7 C-60 source evidence identity mismatch")
    contract = evidence.get("contract")
    if not isinstance(contract, dict) or contract != closure.get("contract"):
        raise GateFailure("G7 C-60 closure contract differs from source evidence")
    if (
        contract.get("states") != ["warm"]
        or contract.get("background_modes") != ["control"]
        or contract.get("concurrency") != [1, 8, 32]
        or contract.get("observations_per_surface") != 1_000_000
        or contract.get("warmup_per_surface") != 100_000
        or contract.get("surfaces") != len(requirements)
    ):
        raise GateFailure("G7 C-60 measured contract is incomplete")
    cells = evidence.get("cells")
    if not isinstance(cells, list) or len(cells) != 3:
        raise GateFailure("G7 C-60 evidence must contain three concurrency cells")
    for expected_concurrency, cell in zip((1, 8, 32), cells, strict=True):
        if (
            not isinstance(cell, dict)
            or cell.get("concurrency") != expected_concurrency
            or cell.get("status") != "passed"
            or cell.get("surfaces") != len(requirements)
            or cell.get("observations_per_surface") != 1_000_000
            or cell.get("warmup_per_surface") != 100_000
            or cell.get("ann_recall_at_10") != 1.0
            or cell.get("recovery_missing") != 0
            or cell.get("recovery_mismatched") != 0
            or cell.get("strict_logical_commits") != 1_000_000
            or cell.get("strict_distinct_csns") != 1_000_000
            or cell.get("ann_targeted", 0) + cell.get("ann_generic_fallback", 0)
            != 1_000_000
            or set(cell.get("p50_nanos", {})) != set(requirements)
            or any(
                not isinstance(value, int) or isinstance(value, bool) or value <= 0
                for value in cell.get("p50_nanos", {}).values()
            )
        ):
            raise GateFailure("G7 C-60 cell evidence is incomplete")
    host = _object(evidence.get("host"), "G7 C-60 host evidence")
    authority_execution = _object(
        closure.get("authority_execution"), "G7 C-60 authority execution"
    )
    if (
        authority_execution
        != {
            "provider": "DigitalOcean",
            "droplet_id": 592176697,
            "size": "c-60-intel",
            "region": "nyc3",
            "vcpus": 60,
            "ram_gib": 120,
            "deleted": True,
        }
        or any(
            host.get(field) != authority_execution[field]
            for field in ("provider", "droplet_id", "size", "region", "vcpus", "ram_gib")
        )
        or host.get("dedicated") is not False
    ):
        raise GateFailure("G7 C-60 execution environment mismatch")
    latency = _object(evidence.get("latency"), "G7 C-60 latency evidence")
    if (
        latency.get("status") != "observed-not-normative"
        or latency.get("full_run_threshold_gate") != "not asserted"
    ):
        raise GateFailure("G7 C-60 latency non-claim drifted")
    return evidence


def _validate_g7_artifacts(
    closure: dict[str, Any], evidence: dict[str, Any]
) -> None:
    if (
        closure.get("matrix_sha256") != evidence.get("matrix_sha256")
        or closure.get("runner_override_sha256")
        != evidence.get("runner_override_sha256")
        or closure.get("artifact_sha256")
        != "760ae09fe409a7ba39222ba9ff55367dc5923620124525b02ef7c75b0f536a47"
        or closure.get("checksums_sha256")
        != "1f1a5c3e86eef1a36231f4aa06977d9a1076c473c26914e1ef2380449837197"
    ):
        raise GateFailure("G7 C-60 external artifact binding mismatch")
    diagnostic = _object(
        closure.get("bare_metal_diagnostic"), "G7 bare-metal diagnostic"
    )
    if (
        diagnostic
        != {
            "provider": "AWS",
            "instance_id": "i-0aec6617871f0ff8e",
            "runner": "hyphae-g7-i7i-2ac90c8e84eb",
            "workflow_run": 31789602285,
            "source_commit": "b2f8679ef28be6f10fabf773ac8809712cebf035",
            "conclusion": "failure",
            "failure_class": "infrastructure-toolchain-install",
            "failed_step": "Discover and calibrate dedicated hardware",
            "product_measurements_started": False,
        }
    ):
        raise GateFailure("G7 bare-metal diagnostic is not honest")


def validate_g7_c60_closure(root: Path, closure: dict[str, Any]) -> None:
    source_commit, source_tree = _validate_g7_identity(closure)
    requirements = _validate_g7_profile(root, closure)
    evidence = _validate_g7_source_evidence(
        root,
        closure,
        source_commit,
        source_tree,
        requirements,
    )
    _validate_g7_artifacts(closure, evidence)


def validate(root: Path) -> dict[str, Any]:
    status = _load(root / "config/native-gate-status.json", "native gate status")
    if (
        status.get("schema") != "hyphae-native-gate-status-v1"
        or status.get("program") != "native-local-phase-1"
        or status.get("authority") != "docs/gates/native-local-phase-1.md"
    ):
        raise GateFailure("unsupported native gate status authority")
    rows = status.get("gates")
    if not isinstance(rows, list) or [row.get("id") for row in rows if isinstance(row, dict)] != EXPECTED_GATES:
        raise GateFailure("native gates must be unique and ordered G0 through G8")

    indexes = {
        "docs": (root / "docs/README.md").read_text(encoding="utf-8"),
        "status": (root / "docs/gates/native-gate-status.md").read_text(encoding="utf-8"),
        "evidence": (root / "docs/gates/evidence/README.md").read_text(encoding="utf-8"),
    }
    closed: list[str] = []
    closed_rows: dict[str, dict[str, Any]] = {}
    encountered_open = False
    for raw_row in rows:
        row = _object(raw_row, "native gate row")
        gate = row["id"]
        state = row.get("status")
        if state == "open":
            encountered_open = True
            if set(row) != {"id", "status"}:
                raise GateFailure(f"open {gate} row contains closure fields")
            continue
        if state != "closed":
            raise GateFailure(f"unsupported status for {gate}: {state}")
        if encountered_open:
            raise GateFailure(f"closed {gate} appears after an open predecessor")
        if set(row) != {"id", "status", "source_commit", "evidence", "evidence_sha256"}:
            raise GateFailure(f"closed {gate} row fields mismatch")

        source_commit = row["source_commit"]
        evidence_reference = row["evidence"]
        evidence_digest = row["evidence_sha256"]
        if not isinstance(source_commit, str) or HEX40.fullmatch(source_commit) is None:
            raise GateFailure(f"{gate} source commit is not canonical")
        expected_reference = (
            f"docs/gates/evidence/closures/native-{gate.lower()}-{source_commit[:7]}.json"
        )
        if evidence_reference != expected_reference:
            raise GateFailure(f"{gate} evidence path is not source-bound")
        if not isinstance(evidence_digest, str) or HEX64.fullmatch(evidence_digest) is None:
            raise GateFailure(f"{gate} evidence digest is not canonical")

        evidence_path = root / evidence_reference
        try:
            actual_digest = hashlib.sha256(evidence_path.read_bytes()).hexdigest()
        except OSError as error:
            raise GateFailure(f"{gate} retained evidence is missing") from error
        if actual_digest != evidence_digest:
            raise GateFailure(f"{gate} retained evidence digest mismatch")
        evidence = _load(evidence_path, f"{gate} retained closure")
        if evidence.get("schema") != EXPECTED_SCHEMAS.get(gate):
            raise GateFailure(f"{gate} retained closure schema mismatch")
        if (
            evidence.get("gate") != gate
            or evidence.get("status") != "passed"
            or evidence.get("source_commit") != source_commit
        ):
            raise GateFailure(f"{gate} retained closure identity mismatch")
        required = evidence.get("required")
        passed = evidence.get("passed")
        if (
            not isinstance(required, int)
            or isinstance(required, bool)
            or required <= 0
            or passed != required
        ):
            raise GateFailure(f"{gate} retained closure count mismatch")
        if evidence.get("requirements") != _profile_requirements(root, gate):
            raise GateFailure(f"{gate} retained closure requirements drifted")
        if gate == "G7":
            validate_g7_c60_closure(root, evidence)
        else:
            workflow_run = evidence.get("workflow_run")
            if (
                not isinstance(workflow_run, int)
                or isinstance(workflow_run, bool)
                or workflow_run <= 0
            ):
                raise GateFailure(
                    f"{gate} retained closure workflow identity is invalid"
                )
        if not isinstance(evidence.get("artifact"), str) or not evidence["artifact"]:
            raise GateFailure(f"{gate} retained closure artifact is missing")
        expected_production_scale = gate == "G7"
        if evidence.get("production_scale") is not expected_production_scale:
            raise GateFailure(f"{gate} retained closure production scale mismatch")

        if gate == "G1":
            predecessor = _object(evidence.get("predecessor"), "G1 predecessor")
            previous = closed_rows.get("G0")
            if previous is None or predecessor != {
                "gate": "G0",
                "source_commit": previous["source_commit"],
                "evidence": previous["evidence"],
                "evidence_sha256": previous["evidence_sha256"],
            }:
                raise GateFailure("G1 retained closure is not bound to G0")

        docs_target = evidence_reference.removeprefix("docs/")
        status_target = evidence_reference.removeprefix("docs/gates/")
        evidence_target = evidence_reference.removeprefix("docs/gates/evidence/")
        if f"]({docs_target})" not in indexes["docs"]:
            raise GateFailure(f"{gate} closure is absent from docs/README.md")
        if f"]({status_target})" not in indexes["status"] or f"`{source_commit[:7]}`" not in indexes["status"]:
            raise GateFailure(f"{gate} closure is absent from the gate status index")
        if f"]({evidence_target})" not in indexes["evidence"]:
            raise GateFailure(f"{gate} closure is absent from the evidence index")
        closed.append(gate)
        closed_rows[gate] = row

    return {
        "schema": "hyphae-native-gate-closure-audit-v1",
        "status": "passed",
        "closed": closed,
        "open": EXPECTED_GATES[len(closed) :],
    }


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    try:
        result = validate(root)
    except (GateFailure, OSError, UnicodeError) as error:
        print(f"native gate closure validation failed: {error}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
