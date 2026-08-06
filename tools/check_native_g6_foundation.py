#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the accepted, deliberately open G6 contract foundation."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
REQUIREMENTS = [
    "shared-contracts-and-errors",
    "catalog-and-collection-model",
    "embedded-product-facade",
    "competitive-search-surface",
    "incremental-ann-lifecycle",
    "native-offline-proofs",
    "native-local-daemon",
    "native-http-v2",
    "single-binary-cli",
    "rust-python-typescript-sdks",
    "administration-and-explain",
    "telemetry-and-doctor",
    "backup-restore-product-surface",
    "cross-surface-conformance",
]
PLATFORMS = ["linux", "macos", "windows"]
SDKS = ["rust", "python", "typescript"]
TRANSPORTS = ["embedded", "native-local", "http-v2"]
PREDECESSORS = ["G0", "G1", "G2", "G3", "G4", "G5"]
CONTRACTS = [
    "docs/gates/native-local-phase-1.md",
    "docs/adr/0023-native-local-product-and-competitive-scope.md",
    "docs/roadmaps/native-g6-roadmap.md",
    "docs/native/local-product-v1.md",
    "docs/native/product-error-v1.md",
    "docs/native/catalog-api-v1.md",
    "docs/native/explain-v1.md",
    "docs/native/telemetry-v1.md",
    "docs/native/native-proof-v1.md",
    "docs/native/http-v2.md",
    "docs/native/local-protocol-v1.md",
    "docs/native/catalog-v1.md",
    "docs/native/search-semantics-v1.md",
    "docs/native/ann-semantics-v1.md",
    "docs/native/native-backup-v1.md",
]
WORKLOAD_ACCEPTANCE = {
    "shared-contracts-and-errors": {"stable-code-registry", "unknown-commit", "redaction", "local-http-sdk-parity"},
    "catalog-and-collection-model": {"bounded-enumeration", "catalogued-keyspaces", "fielded-collections", "named-vectors", "dependency-integrity"},
    "embedded-product-facade": {"curated-api", "direct-engine-calls", "limits-deadlines-cancellation", "durability-policy"},
    "competitive-search-surface": {"persistent-doc-values", "filter-aware-ann", "exact-filtered-oracle", "adaptive-exact-ann", "multi-target-vectors", "streaming-ingest-backpressure", "no-partial-mutation"},
    "incremental-ann-lifecycle": {"incremental-upsert", "incremental-delete", "durable-delta", "interrupted-consolidation", "atomic-generation-switch", "snapshot-safe-reclamation", "no-per-mutation-full-rebuild"},
    "native-offline-proofs": {"point-sql-search-proofs", "ann-approximation-binding", "hybrid-branch-binding", "origin-unavailable", "tamper-matrix"},
    "native-local-daemon": {"uds", "windows-named-pipe", "multi-client", "handshake-capabilities", "peer-identity", "prepared-handles", "provisional-read-completion", "flow-control", "cancel-deadline", "disconnect-transaction-outcome", "graceful-shutdown"},
    "native-http-v2": {"openapi-json-schema", "authentication", "bounded-streaming", "provisional-read-completion", "request-id", "v1-compatibility-policy"},
    "single-binary-cli": {"native-default-authority", "no-native-switch", "no-implicit-listener", "stable-exit-classes", "full-admitted-operation-corpus"},
    "rust-python-typescript-sdks": {"rust", "python", "typescript", "native-local", "http-v2", "typed-errors-proofs"},
    "administration-and-explain": {"typed-explain", "authorization", "progress-cancellation", "foreground-interference"},
    "telemetry-and-doctor": {"separate-clocks", "bounded-cardinality", "redaction", "whole-product-corruption", "lock-contention"},
    "backup-restore-product-surface": {"facade-cli-surface", "offline-verification", "doctor-after-restore", "complete-state-equivalence"},
    "cross-surface-conformance": {"linux-macos-windows", "embedded-local-cli-http", "rust-python-typescript", "identity-csn-result-error-explain-proof", "failure-paths"},
}


class GateFailure(ValueError):
    pass


def _object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _open(payload: dict[str, Any], schema: str) -> None:
    if payload.get("schema") != schema or payload.get("gate") != "G6":
        raise GateFailure(f"unsupported {schema}")
    if payload.get("claims") != [] or payload.get("closure_declared") is not False:
        raise GateFailure("G6 authorities must remain open and claim-free")


def _fields(payload: dict[str, Any], expected: set[str], label: str) -> None:
    if set(payload) != expected:
        raise GateFailure(f"{label} fields mismatch")


def validate(
    root: Path,
    profile: dict[str, Any],
    evidence: dict[str, Any],
    inventory: dict[str, Any],
    authority: dict[str, Any],
    workload: dict[str, Any],
    suite: dict[str, Any],
    predecessor: dict[str, Any],
    expected_commit: str,
    manifest_digests: dict[str, str],
) -> dict[str, Any]:
    documents = (
        (profile, "hyphae-native-g6-readiness-profile-v1"),
        (evidence, "hyphae-native-g6-readiness-evidence-v1"),
        (inventory, "hyphae-native-g6-inventory-v1"),
        (authority, "hyphae-native-g6-authority-manifest-v1"),
        (workload, "hyphae-native-g6-workload-manifest-v1"),
        (suite, "hyphae-native-g6-suite-manifest-v1"),
        (predecessor, "hyphae-native-g6-predecessor-manifest-v1"),
    )
    for payload, schema in documents:
        _open(payload, schema)
    _fields(
        profile,
        {"schema", "gate", "scope", "requirements", "required_platforms", "required_sdks", "required_transports", "claims", "closure_declared"},
        "G6 profile",
    )
    _fields(evidence, {"schema", "gate", "predecessor", "evidence", "claims", "closure_declared"}, "G6 evidence")
    _fields(inventory, {"schema", "gate", "requirements", "claims", "closure_declared"}, "G6 inventory")
    _fields(
        authority,
        {"schema", "gate", "scope", "evidence_class", "requirements", "required_predecessors", "required_platforms", "required_sdks", "required_transports", "contracts", "claims", "closure_declared"},
        "G6 authority",
    )
    _fields(workload, {"schema", "gate", "workloads", "claims", "closure_declared"}, "G6 workload")
    _fields(suite, {"schema", "gate", "requirements", "claims", "closure_declared"}, "G6 suite")
    _fields(predecessor, {"schema", "gate", "predecessors", "claims", "closure_declared"}, "G6 predecessor")
    if HEX40.fullmatch(expected_commit) is None:
        raise GateFailure("G6 expected commit is not canonical")
    if set(manifest_digests) != {"profile", "evidence", "inventory", "authority", "workload", "suite", "predecessor"} or any(
        HEX64.fullmatch(value) is None for value in manifest_digests.values()
    ):
        raise GateFailure("G6 manifest identities are incomplete")

    profile_rows = profile.get("requirements")
    if not isinstance(profile_rows, list) or [row.get("id") for row in profile_rows if isinstance(row, dict)] != REQUIREMENTS:
        raise GateFailure("profile must define the ordered fourteen-requirement G6 contract")
    if any(set(row) != {"id", "required_evidence"} or row["required_evidence"] != "hosted" for row in profile_rows):
        raise GateFailure("every G6 requirement needs hosted evidence")
    if (
        profile.get("scope") != "competitive-local-product"
        or profile.get("required_platforms") != PLATFORMS
        or profile.get("required_sdks") != SDKS
        or profile.get("required_transports") != TRANSPORTS
    ):
        raise GateFailure("G6 product scope mismatch")

    if evidence.get("predecessor") is not None or evidence.get("evidence") != {}:
        raise GateFailure("checked-in G6 evidence must remain empty")

    inventory_rows = inventory.get("requirements")
    if not isinstance(inventory_rows, list) or [row.get("id") for row in inventory_rows if isinstance(row, dict)] != REQUIREMENTS:
        raise GateFailure("G6 inventory coverage mismatch")
    for raw_row in inventory_rows:
        row = _object(raw_row, "G6 inventory row")
        if (
            set(row) != {"id", "status", "present", "gaps"}
            or not isinstance(row.get("id"), str)
            or
            row.get("status") not in {"open", "partial", "implemented-unhosted"}
            or not isinstance(row.get("present"), list)
            or not isinstance(row.get("gaps"), list)
            or not row["gaps"]
        ):
            raise GateFailure(f"G6 inventory must retain concrete gaps for {row.get('id')}")

    if (
        authority.get("scope") != "competitive-local-product-contract-foundation"
        or authority.get("evidence_class") != "authority-not-requirement-evidence"
        or authority.get("requirements") != REQUIREMENTS
        or authority.get("required_predecessors") != PREDECESSORS
        or authority.get("required_platforms") != PLATFORMS
        or authority.get("required_sdks") != SDKS
        or authority.get("required_transports") != TRANSPORTS
    ):
        raise GateFailure("G6 authority scope mismatch")
    contracts = authority.get("contracts")
    if not isinstance(contracts, list) or [row.get("reference") for row in contracts if isinstance(row, dict)] != CONTRACTS:
        raise GateFailure("G6 contract authority set is incomplete")
    seen_contracts: set[str] = set()
    for raw_row in contracts:
        row = _object(raw_row, "G6 contract authority")
        if set(row) != {"reference", "sha256"}:
            raise GateFailure("invalid G6 contract authority fields")
        reference = Path(row["reference"])
        if (
            reference.as_posix() in seen_contracts
            or reference.is_absolute()
            or ".." in reference.parts
            or HEX64.fullmatch(row["sha256"]) is None
        ):
            raise GateFailure("invalid G6 contract identity")
        artifact = root / reference
        if not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["sha256"]:
            raise GateFailure(f"missing or mismatched G6 contract: {reference}")
        seen_contracts.add(reference.as_posix())

    workload_rows = workload.get("workloads")
    suite_rows = suite.get("requirements")
    if not isinstance(workload_rows, list) or not isinstance(suite_rows, list):
        raise GateFailure("G6 workload and suite rows are required")
    workload_ids = [row.get("id") for row in workload_rows if isinstance(row, dict)]
    if (
        len(workload_ids) != len(REQUIREMENTS)
        or len(set(workload_ids)) != len(REQUIREMENTS)
        or [row.get("requirement") for row in workload_rows if isinstance(row, dict)] != REQUIREMENTS
    ):
        raise GateFailure("G6 workloads must map one-to-one to requirements")
    for raw_row in workload_rows:
        row = _object(raw_row, "G6 workload row")
        if set(row) != {"id", "requirement", "oracle", "acceptance"}:
            raise GateFailure("invalid G6 workload fields")
        requirement = row["requirement"]
        acceptance = row["acceptance"]
        if (
            not isinstance(row["oracle"], str)
            or not row["oracle"]
            or not isinstance(acceptance, list)
            or not acceptance
            or len(acceptance) != len(set(acceptance))
            or set(acceptance) != WORKLOAD_ACCEPTANCE[requirement]
        ):
            raise GateFailure(f"G6 workload acceptance mismatch for {requirement}")
    if [row.get("id") for row in suite_rows if isinstance(row, dict)] != REQUIREMENTS:
        raise GateFailure("G6 suite coverage mismatch")
    for requirement, raw_row in zip(REQUIREMENTS, suite_rows, strict=True):
        row = _object(raw_row, "G6 suite row")
        expected_workload = workload_rows[REQUIREMENTS.index(requirement)]["id"]
        if (
            set(row) != {"id", "workloads", "status", "suites"}
            or row["workloads"] != [expected_workload]
            or row["status"] != "planned"
            or row["suites"] != []
        ):
            raise GateFailure(f"G6 suite {requirement} must remain explicitly planned")

    predecessor_rows = predecessor.get("predecessors")
    if not isinstance(predecessor_rows, list) or [row.get("gate") for row in predecessor_rows if isinstance(row, dict)] != PREDECESSORS:
        raise GateFailure("G6 predecessor coverage mismatch")
    gate_status = json.loads((root / "config/native-gate-status.json").read_text(encoding="utf-8"))
    status_rows = {
        row["id"]: row
        for row in gate_status.get("gates", [])
        if isinstance(row, dict) and row.get("id") in PREDECESSORS
    }
    if list(status_rows) != PREDECESSORS:
        raise GateFailure("G6 predecessor status prefix mismatch")
    for raw_row in predecessor_rows:
        row = _object(raw_row, "G6 predecessor")
        if set(row) != {"gate", "source_commit", "reference", "sha256"}:
            raise GateFailure("invalid G6 predecessor fields")
        reference = Path(row["reference"])
        if (
            HEX40.fullmatch(row["source_commit"]) is None
            or HEX64.fullmatch(row["sha256"]) is None
            or reference.is_absolute()
            or ".." in reference.parts
        ):
            raise GateFailure("invalid G6 predecessor identity")
        status_row = status_rows[row["gate"]]
        if (
            status_row.get("status") != "closed"
            or status_row.get("source_commit") != row["source_commit"]
            or status_row.get("evidence") != row["reference"]
            or status_row.get("evidence_sha256") != row["sha256"]
        ):
            raise GateFailure(f"G6 predecessor differs from gate status for {row['gate']}")
        artifact = root / reference
        if not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != row["sha256"]:
            raise GateFailure(f"missing or mismatched G6 predecessor {row['gate']}")
        retained = json.loads(artifact.read_text(encoding="utf-8"))
        requirements = retained.get("requirements")
        if (
            retained.get("gate") != row["gate"]
            or retained.get("status") != "passed"
            or retained.get("source_commit") != row["source_commit"]
            or not isinstance(retained.get("schema"), str)
            or not retained["schema"].startswith(f"hyphae-native-{row['gate'].lower()}-")
            or retained.get("required") != retained.get("passed")
            or not isinstance(retained.get("required"), int)
            or isinstance(retained.get("required"), bool)
            or retained["required"] <= 0
            or not isinstance(requirements, list)
            or len(requirements) != retained["required"]
        ):
            raise GateFailure(f"unpassed G6 predecessor {row['gate']}")

    return {
        "schema": "hyphae-native-g6-foundation-audit-v1",
        "gate": "G6",
        "status": "foundation-complete",
        "source_commit": expected_commit,
        "requirements": len(REQUIREMENTS),
        "implemented_requirements": 0,
        "planned_requirements": len(REQUIREMENTS),
        "contracts": len(contracts),
        "predecessors": len(PREDECESSORS),
        "manifest_sha256": manifest_digests,
        "closure_status": "open",
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--expected-commit", required=True)
    for name in ("profile", "evidence", "inventory", "authority", "workload", "suite", "predecessor"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        names = ("profile", "evidence", "inventory", "authority", "workload", "suite", "predecessor")
        raw = {name: getattr(args, name).read_bytes() for name in names}
        payloads = [json.loads(raw[name]) for name in names]
        digests = {name: hashlib.sha256(raw[name]).hexdigest() for name in names}
        result = validate(args.root, *payloads, args.expected_commit, digests)
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G6 foundation audit failed: {error}")
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
