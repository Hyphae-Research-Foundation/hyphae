#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the fail-closed security crash matrix inventory."""

from __future__ import annotations

import copy
import json
import unittest

from tools.check_security_crash_matrix import (
    ACCESS_REGISTRY,
    CASES,
    OPERATION_SOURCE,
    ROOT,
    SecurityCrashMatrixError,
    validate,
    validate_receipt,
    validate_receipts,
)


def payload() -> dict:
    return json.loads(CASES.read_text(encoding="utf-8"))


def registry() -> dict:
    return json.loads(ACCESS_REGISTRY.read_text(encoding="utf-8"))


class SecurityCrashMatrixTests(unittest.TestCase):
    def test_checked_in_matrix_is_complete(self) -> None:
        result = validate(payload(), registry(), OPERATION_SOURCE, ROOT)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["product_operations"], 21)
        self.assertEqual(result["offline_operations"], 5)
        self.assertEqual(result["operation_cases"], 26)
        self.assertEqual(result["semantic_families"], 19)
        self.assertEqual(result["boundary_cases"], 182)
        self.assertEqual(result["hard_kill_cases"], 182)

    def test_new_access_registry_mutation_fails_closed(self) -> None:
        access = registry()
        future = copy.deepcopy(
            next(row for row in access["operations"] if row["id"] == "security.principal_create")
        )
        future["id"] = "security.future_mutation"
        future["source_variant"] = "SecurityFutureMutation"
        access["operations"].append(future)
        with self.assertRaisesRegex(SecurityCrashMatrixError, "absent ProductOperation"):
            validate(payload(), access, OPERATION_SOURCE, ROOT)

    def test_missing_case_or_boundary_fails_closed(self) -> None:
        corpus = payload()
        corpus["cases"].pop()
        with self.assertRaisesRegex(SecurityCrashMatrixError, "matrix drift"):
            validate(corpus, registry(), OPERATION_SOURCE, ROOT)

        corpus = payload()
        corpus["boundaries"].pop()
        with self.assertRaisesRegex(SecurityCrashMatrixError, "CommitBoundary"):
            validate(corpus, registry(), OPERATION_SOURCE, ROOT)

    def test_power_loss_claim_and_missing_evidence_anchor_fail_closed(self) -> None:
        corpus = payload()
        corpus["semantics"] = "power-loss"
        with self.assertRaisesRegex(SecurityCrashMatrixError, "power-loss"):
            validate(corpus, registry(), OPERATION_SOURCE, ROOT)

        corpus = payload()
        corpus["evidence"][0]["anchors"] = ["missing_anchor"]
        with self.assertRaisesRegex(SecurityCrashMatrixError, "anchor"):
            validate(corpus, registry(), OPERATION_SOURCE, ROOT)

    def test_process_receipt_is_boundary_exact_and_source_bound(self) -> None:
        rows = observations(0, 1)
        receipt = {
            "schema": "hyphae-security-process-crash-matrix-v2",
            "status": "passed",
            "source_commit": "a" * 40,
            "environment": "unit-test",
            "target": "x86_64-linux",
            "semantics": "process-crash-not-power-loss",
            "shard_index": 0,
            "shard_count": 1,
            "case_count": 26,
            "boundary_case_count": 182,
            "observations": rows,
        }
        self.assertEqual(validate_receipt(receipt, "a" * 40)["boundary_case_count"], 182)
        receipt["observations"][4]["recovered_state"] = "prior"
        with self.assertRaisesRegex(SecurityCrashMatrixError, "differs"):
            validate_receipt(receipt, "a" * 40)

    def test_representative_receipt_and_unwind_claim_fail_closed(self) -> None:
        receipt = make_receipt(24, 26)
        self.assertEqual(validate_receipt(receipt)["boundary_case_count"], 7)
        with self.assertRaisesRegex(SecurityCrashMatrixError, "shard inventory"):
            validate_receipts([receipt], payload())

        receipt["observations"][0]["child_unwound"] = True
        with self.assertRaisesRegex(SecurityCrashMatrixError, "differs"):
            validate_receipt(receipt)

    def test_exact_shard_aggregate_is_accepted(self) -> None:
        receipts = [make_receipt(index, 26) for index in range(26)]
        aggregate = validate_receipts(receipts, payload(), "a" * 40)
        self.assertEqual(aggregate["operation_cases"], 26)
        self.assertEqual(aggregate["boundary_cases"], 182)

        receipts[-1]["source_commit"] = "b" * 40
        with self.assertRaisesRegex(SecurityCrashMatrixError, "source commits"):
            validate_receipts(receipts, payload())


def observations(shard_index: int, shard_count: int) -> list[dict]:
    rows = []
    cases = [
        case
        for index, case in enumerate(payload()["cases"])
        if index % shard_count == shard_index
    ]
    for case in cases:
        for boundary in payload()["boundaries"]:
            state = "prior" if boundary in payload()["recovery_rule"]["prior"] else "complete"
            rows.append(
                {
                    "case_id": case["id"],
                    "semantic_family": case["semantic_family"],
                    "kind": case["kind"],
                    "product_operation": case["product_operation"],
                    "boundary": boundary,
                    "expected_state": state,
                    "recovered_state": state,
                    "boundary_hook_reached": True,
                    "child_unwound": False,
                    "termination": "signal-9",
                    "recovery_verified": True,
                }
            )
    return rows


def make_receipt(shard_index: int, shard_count: int) -> dict:
    rows = observations(shard_index, shard_count)
    return {
        "schema": "hyphae-security-process-crash-matrix-v2",
        "status": "passed",
        "source_commit": "a" * 40,
        "environment": "unit-test",
        "target": "x86_64-linux",
        "semantics": "process-crash-not-power-loss",
        "shard_index": shard_index,
        "shard_count": shard_count,
        "case_count": len(rows) // 7,
        "boundary_case_count": len(rows),
        "observations": rows,
    }


if __name__ == "__main__":
    unittest.main()
