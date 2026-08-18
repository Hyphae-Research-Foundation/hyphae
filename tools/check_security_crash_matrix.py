#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed inventory checker for security process-crash coverage."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

if __package__ in {None, ""}:
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tools.check_native_access_control import product_operation_variants


ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "conformance/v2/security-crash-cases.json"
ACCESS_REGISTRY = ROOT / "contracts/native-access-control-v1.json"
OPERATION_SOURCE = ROOT / "crates/hyphae-native-product/src/operation.rs"
RECEIPT_SCHEMA = ROOT / "conformance/v2/schema/security-crash-receipt.schema.json"
SCHEMA = "hyphae-security-crash-cases-v1"
BOUNDARIES = [
    "BlobStaged",
    "BlobPromoted",
    "PageAppended",
    "PageSynchronized",
    "WalAppended",
    "WalSynchronized",
    "RootPublished",
]
PRIOR = BOUNDARIES[:4]
COMPLETE = BOUNDARIES[4:]
OFFLINE_APIS = {
    "abort_owner_recovery_offline",
    "activate_legacy_bearer_migration_offline",
    "resume_owner_recovery_offline",
    "start_legacy_bearer_migration_offline",
    "start_owner_recovery_offline",
}
INVARIANTS = {
    "audit-commit-csn-coherent",
    "disconnect-equivalent-reopen",
    "epoch-result-marker-coherent",
    "exactly-one-audit",
    "exactly-one-mutation",
    "no-active-unconfirmed-key",
    "no-partial-state",
    "original-owner-until-owner-activation",
    "retry-conflicting-payload-rejected",
    "retry-same-payload-stable",
    "secrets-never-persisted",
}


class SecurityCrashMatrixError(ValueError):
    """The crash matrix is incomplete or inconsistent with current authority."""


def fail(message: str) -> None:
    raise SecurityCrashMatrixError(message)


def exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    if set(value) != expected:
        fail(f"{context} fields differ")


def access_security_mutations(registry: dict[str, Any]) -> dict[str, str]:
    rows = registry.get("operations")
    if not isinstance(rows, list):
        fail("access registry operations are missing")
    mutations: dict[str, str] = {}
    for row in rows:
        if not isinstance(row, dict):
            fail("access registry operation is not an object")
        operation = row.get("id")
        variant = row.get("source_variant")
        if (
            row.get("status") == "current"
            and isinstance(operation, str)
            and operation.startswith("security.")
            and operation
            not in {
                "security.assignment_list",
                "security.audit_read",
                "security.key_list",
                "security.principal_list",
                "security.role_list",
                "security.status",
            }
        ):
            if not isinstance(variant, str) or not variant:
                fail(f"current security mutation {operation} lacks ProductOperation")
            mutations[operation] = variant
    if not mutations:
        fail("access registry contains no current security mutations")
    return mutations


def validate(
    payload: dict[str, Any],
    registry: dict[str, Any],
    operation_source: Path,
    repository: Path,
) -> dict[str, Any]:
    exact_keys(
        payload,
        {
            "$comment",
            "schema",
            "semantics",
            "boundaries",
            "recovery_rule",
            "invariants",
            "cases",
            "evidence",
        },
        "security crash corpus",
    )
    if payload["schema"] != SCHEMA:
        fail("security crash corpus schema differs")
    if payload["semantics"] != "process-crash-not-power-loss":
        fail("security crash corpus must not claim power-loss semantics")
    if payload["boundaries"] != BOUNDARIES:
        fail("CommitBoundary inventory differs")
    if payload["recovery_rule"] != {"prior": PRIOR, "complete": COMPLETE}:
        fail("process-crash recovery rule differs")
    if set(payload["invariants"]) != INVARIANTS or payload["invariants"] != sorted(INVARIANTS):
        fail("security crash invariants differ")

    expected_mutations = access_security_mutations(registry)
    source_variants = product_operation_variants(operation_source)
    if not set(expected_mutations.values()).issubset(source_variants):
        fail("access registry names an absent ProductOperation")
    rows = payload["cases"]
    if not isinstance(rows, list) or not rows:
        fail("security crash cases must be a nonempty list")
    ids: list[str] = []
    covered: dict[str, str] = {}
    offline: set[str] = set()
    families: set[str] = set()
    for row in rows:
        if not isinstance(row, dict):
            fail("security crash case is not an object")
        exact_keys(
            row,
            {"id", "api", "kind", "product_operation", "semantic_family", "terminal_replay"},
            "security crash case",
        )
        case_id = row["id"]
        if not isinstance(case_id, str) or not case_id:
            fail("security crash case ID is invalid")
        ids.append(case_id)
        family = row["semantic_family"]
        if not isinstance(family, str) or not family:
            fail("security crash semantic family is invalid")
        families.add(family)
        if not isinstance(row["terminal_replay"], bool):
            fail("terminal replay flag is invalid")
        if row["kind"] == "product-operation":
            if row["api"] != "dispatch" or not isinstance(row["product_operation"], str):
                fail("ProductOperation crash case shape differs")
            covered[case_id] = row["product_operation"]
        elif row["kind"] == "offline":
            if row["product_operation"] is not None or row["api"] not in OFFLINE_APIS:
                fail("offline crash case does not use an exact offline API")
            offline.add(row["api"])
        else:
            fail("security crash case kind differs")
    if ids != sorted(set(ids)):
        fail("security crash cases must be sorted and unique")
    if covered != expected_mutations:
        missing = sorted(set(expected_mutations.items()) - set(covered.items()))
        unknown = sorted(set(covered.items()) - set(expected_mutations.items()))
        fail(f"security mutation matrix drift: missing={missing}, unknown={unknown}")
    if offline != OFFLINE_APIS:
        fail(f"offline security matrix drift: missing={sorted(OFFLINE_APIS - offline)}")

    evidence = payload["evidence"]
    if not isinstance(evidence, list) or len(evidence) != 2:
        fail("security crash evidence rows differ")
    evidence_ids: list[str] = []
    for row in evidence:
        if not isinstance(row, dict):
            fail("security crash evidence row is not an object")
        exact_keys(row, {"id", "source", "anchors", "command"}, "evidence row")
        evidence_ids.append(row["id"])
        source = repository / row["source"]
        try:
            text = source.read_text(encoding="utf-8")
        except OSError as error:
            fail(f"security crash evidence source is unavailable: {error}")
        if (
            not isinstance(row["anchors"], list)
            or not row["anchors"]
            or any(not isinstance(anchor, str) or anchor not in text for anchor in row["anchors"])
        ):
            fail("security crash evidence anchor is absent")
        if not isinstance(row["command"], str) or "security_crash_matrix" not in row["command"]:
            fail("security crash evidence command differs")
    if evidence_ids != ["injected-reopen-matrix", "hosted-process-hard-kill"]:
        fail("security crash evidence order differs")

    test_source = (
        repository / "crates/hyphae-native-product/examples/security_crash_support.rs"
    ).read_text(encoding="utf-8")
    for variant in expected_mutations.values():
        if variant not in test_source:
            fail(f"matrix test does not execute {variant}")
    for api in OFFLINE_APIS:
        if api not in test_source:
            fail(f"matrix test does not execute offline API {api}")
    if set(re.findall(r"CommitBoundary::([A-Za-z]+)", test_source)) < set(BOUNDARIES):
        fail("matrix test does not use every real CommitBoundary")
    hard_kill_source = (
        repository / "crates/hyphae-native-product/examples/security_crash_matrix.rs"
    ).read_text(encoding="utf-8")
    if "ProductOperation" not in hard_kill_source or ".dispatch(" not in hard_kill_source:
        fail("hard-kill matrix does not use the public ProductOperation dispatch path")
    for row in rows:
        if row["id"] not in hard_kill_source:
            fail(f"hard-kill matrix does not execute case {row['id']}")
        operation = row["product_operation"]
        if operation is not None and operation not in hard_kill_source:
            fail(f"hard-kill matrix does not execute {operation}")
    for api in OFFLINE_APIS:
        if api not in hard_kill_source:
            fail(f"hard-kill matrix does not execute offline API {api}")
    if "hook_next_security_commit_for_test" not in hard_kill_source:
        fail("hard-kill matrix does not stop in the CommitBoundary hook")
    for anchor in [
        "status.signal() != Some(9)",
        "child.try_wait()?",
        "child unwound before parent SIGKILL",
        "UnwindSentinel",
    ]:
        if anchor not in hard_kill_source:
            fail("hard-kill matrix no longer proves signal-9-before-unwind")
    return {
        "schema": SCHEMA,
        "status": "passed",
        "product_operations": len(covered),
        "offline_operations": len(offline),
        "operation_cases": len(rows),
        "semantic_families": len(families),
        "boundary_cases": len(rows) * len(BOUNDARIES),
        "hard_kill_cases": len(rows) * len(BOUNDARIES),
    }


def required_hard_kill_inventory(corpus: dict[str, Any]) -> list[tuple[str, str]]:
    rows = corpus.get("cases")
    if not isinstance(rows, list):
        fail("security crash cases are unavailable for receipt validation")
    return [(row["id"], boundary) for row in rows for boundary in BOUNDARIES]


def validate_receipt(
    receipt: dict[str, Any],
    expected_commit: str | None = None,
    corpus: dict[str, Any] | None = None,
) -> dict[str, Any]:
    corpus = corpus or json.loads(CASES.read_text(encoding="utf-8"))
    exact_keys(
        receipt,
        {
            "schema",
            "status",
            "source_commit",
            "environment",
            "target",
            "semantics",
            "shard_index",
            "shard_count",
            "case_count",
            "boundary_case_count",
            "observations",
        },
        "security crash receipt",
    )
    if (
        receipt["schema"] != "hyphae-security-process-crash-matrix-v2"
        or receipt["status"] != "passed"
        or receipt["semantics"] != "process-crash-not-power-loss"
    ):
        fail("security crash receipt authority differs")
    source_commit = receipt["source_commit"]
    if not isinstance(source_commit, str) or re.fullmatch(r"[0-9a-f]{40}", source_commit) is None:
        fail("security crash receipt source commit is invalid")
    if expected_commit is not None and source_commit != expected_commit:
        fail("security crash receipt source commit differs")
    if (
        not isinstance(receipt["environment"], str)
        or re.fullmatch(r"[A-Za-z0-9._:-]{1,128}", receipt["environment"]) is None
        or not isinstance(receipt["target"], str)
        or not receipt["target"]
    ):
        fail("security crash receipt labels are invalid")
    shard_index = receipt["shard_index"]
    shard_count = receipt["shard_count"]
    if (
        not isinstance(shard_index, int)
        or isinstance(shard_index, bool)
        or not isinstance(shard_count, int)
        or isinstance(shard_count, bool)
        or shard_count < 1
        or shard_count > len(corpus["cases"])
        or shard_index < 0
        or shard_index >= shard_count
    ):
        fail("security crash receipt shard selection is invalid")
    selected_cases = [
        row for index, row in enumerate(corpus["cases"]) if index % shard_count == shard_index
    ]
    required = [(case["id"], boundary) for case in selected_cases for boundary in BOUNDARIES]
    rows = receipt["observations"]
    if (
        not isinstance(rows, list)
        or receipt["case_count"] != len(selected_cases)
        or receipt["boundary_case_count"] != len(required)
        or len(rows) != len(required)
    ):
        fail("security crash receipt case counts differ")
    cases_by_id = {row["id"]: row for row in corpus["cases"]}
    observed: list[tuple[str, str]] = []
    for row in rows:
        if not isinstance(row, dict):
            fail("security crash receipt observation is not an object")
        exact_keys(
            row,
            {
                "case_id",
                "semantic_family",
                "kind",
                "product_operation",
                "boundary",
                "expected_state",
                "recovered_state",
                "boundary_hook_reached",
                "child_unwound",
                "termination",
                "recovery_verified",
            },
            "security crash receipt observation",
        )
        case_id = row["case_id"]
        case = cases_by_id.get(case_id)
        if case is None:
            fail(f"security crash receipt has unknown case {case_id}")
        boundary = row["boundary"]
        observed.append((case_id, boundary))
        expected_state = "prior" if boundary in PRIOR else "complete"
        if (
            boundary not in BOUNDARIES
            or row["semantic_family"] != case["semantic_family"]
            or row["kind"] != case["kind"]
            or row["product_operation"] != case["product_operation"]
            or row["expected_state"] != expected_state
            or row["recovered_state"] != expected_state
            or row["boundary_hook_reached"] is not True
            or row["child_unwound"] is not False
            or row["termination"] != "signal-9"
            or row["recovery_verified"] is not True
        ):
            fail(f"security crash receipt observation {case_id}/{boundary} differs")
    if observed != required:
        missing = sorted(set(required) - set(observed))
        unknown = sorted(set(observed) - set(required))
        fail(f"hard-kill receipt inventory differs: missing={missing}, unknown={unknown}")
    return {
        "schema": "hyphae-security-process-crash-matrix-v2",
        "status": "passed",
        "case_count": len(selected_cases),
        "boundary_case_count": len(rows),
        "shard_index": shard_index,
        "shard_count": shard_count,
        "source_commit": source_commit,
    }


def validate_receipts(
    receipts: list[dict[str, Any]],
    corpus: dict[str, Any],
    expected_commit: str | None = None,
) -> dict[str, Any]:
    if not receipts:
        fail("security crash receipt aggregate is empty")
    results = [validate_receipt(receipt, expected_commit, corpus) for receipt in receipts]
    shard_counts = {result["shard_count"] for result in results}
    if len(shard_counts) != 1:
        fail("security crash receipt aggregate mixes shard counts")
    shard_count = shard_counts.pop()
    indexes = [result["shard_index"] for result in results]
    if indexes != list(range(shard_count)):
        fail("security crash receipt aggregate shard inventory differs")
    commits = {result["source_commit"] for result in results}
    if len(commits) != 1:
        fail("security crash receipt aggregate mixes source commits")
    boundary_count = sum(result["boundary_case_count"] for result in results)
    required_count = len(required_hard_kill_inventory(corpus))
    if boundary_count != required_count:
        fail("security crash receipt aggregate boundary count differs")
    return {
        "schema": "hyphae-security-process-crash-matrix-v2",
        "status": "passed",
        "shard_count": shard_count,
        "operation_cases": len(corpus["cases"]),
        "boundary_cases": boundary_count,
        "source_commit": commits.pop(),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, default=CASES)
    parser.add_argument("--access-registry", type=Path, default=ACCESS_REGISTRY)
    parser.add_argument("--operation-source", type=Path, default=OPERATION_SOURCE)
    parser.add_argument("--receipt", type=Path, action="append")
    parser.add_argument("--expected-commit")
    args = parser.parse_args()
    try:
        corpus = json.loads(args.cases.read_text(encoding="utf-8"))
        result = validate(
            corpus,
            json.loads(args.access_registry.read_text(encoding="utf-8")),
            args.operation_source,
            ROOT,
        )
        schema = json.loads(RECEIPT_SCHEMA.read_text(encoding="utf-8"))
        if schema.get("$id") != "https://hyphae.dev/schema/security-process-crash-matrix-v2":
            fail("security crash receipt schema differs")
        observation = schema.get("$defs", {}).get("observation", {}).get("properties", {})
        if observation.get("termination", {}).get("const") != "signal-9" or observation.get(
            "child_unwound", {}
        ).get("const") is not False:
            fail("security crash receipt schema does not bind hard-kill semantics")
        if args.receipt is not None:
            result["receipt"] = validate_receipts(
                [json.loads(path.read_text(encoding="utf-8")) for path in args.receipt],
                corpus,
                args.expected_commit,
            )
    except (SecurityCrashMatrixError, OSError, json.JSONDecodeError) as error:
        print(f"security crash matrix validation failed: {error}")
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
