# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

from tools.check_native_g6_conformance import (
    ConformanceFailure,
    PLATFORMS,
    REQUIRED_FAMILIES,
    REQUIRED_LANES,
    aggregate,
    canonical_cross_lane,
    corpus_digest,
    digest,
    flattened_case_ids,
    lane_case_ids,
    lane_families,
    schema_digest,
    validate_corpus,
    validate_receipt,
)


ROOT = Path(__file__).resolve().parents[1]


def transcript(lane: str) -> dict[str, object]:
    surfaces = {
        "embedded-rust": ("rust", "embedded"),
        "cli": ("cli", "cli"),
        "local-daemon": ("rust", "native-local"),
        "http": ("http", "http-v2"),
        "rust-sdk-local": ("rust", "native-local"),
        "rust-sdk-http": ("rust", "http-v2"),
        "python-sdk-local": ("python", "native-local"),
        "python-sdk-http": ("python", "http-v2"),
        "typescript-sdk-local": ("typescript", "native-local"),
        "typescript-sdk-http": ("typescript", "http-v2"),
    }
    adapter, transport = surfaces[lane]
    return {
        "schema": "hyphae-native-g6-transcript-v1",
        "lane": lane,
        "adapter": adapter,
        "transport": transport,
        "start": {
            "directory_lineage": "11" * 24,
            "catalog_version": 3,
            "visible_csn": 2,
            "root_digest": "22" * 32,
        },
        "cases": fixture_cases(lane),
        "coverage": lane_families(lane),
        "status": "passed",
    }


def fixture_cases(lane: str) -> list[dict[str, object]]:
    cases = []
    for case_id in lane_case_ids(lane):
        family, name = case_id.split("/", 1)
        if family == "capabilities": outcome = {"product_api_version": 1, "directory_format": 1}
        elif family == "catalog": outcome = {"snapshot": {"catalog_version": 3}, "object_ids": [8, 9]} if name == "catalog-list" else {"object_id": 10, "present": True}
        elif family == "sql": outcome = {"rows_affected": 1, "object_id": "20", "commit_csn": 3} if name in {"sql-ddl", "sql-dml"} else ({"columns": ["id"], "rows": [[1]], "snapshot": {"catalog_version": 3}} if name == "sql-prepared" else {"version": 1, "text": "plan"})
        elif family == "structures": outcome = {"family": name, "value": "value", "snapshot": {"catalog_version": 3}}
        elif family == "search": outcome = {"mode": name, "snapshot": {"catalog_version": 3}, "object_ids": [201], "approximate": name in {"ann", "hybrid"}}
        elif family == "transactions": outcome = {"status": "committed", "transaction_id": "20"} if name == "commit-status" else {"staged_operations": 1, "commit_csn": 4}
        elif family == "administration": outcome = {"snapshot": {"catalog_version": 3}} if name == "status" else ({"registry_version": 1, "metric_names": ["requests"]} if name == "telemetry" else {"status": "healthy", "snapshot_verified": True})
        elif family == "proofs": outcome = {"kind": "sql", "anchor_digest": "3" * 64, "proof_digest": "4" * 64, "result_digest": "5" * 64} if name == "generate" else {"status": "verified", "kind": "sql", "anchor_digest": "3" * 64, "proof_digest": "4" * 64, "semantic_reexecution_performed": True}
        elif family == "backup": outcome = {"visible_csn": 4, "checkpoint_digest": "6" * 64, "file_count": 7, "total_bytes": 1024} if name in {"create", "verify"} else ({"visible_csn": 4, "checkpoint_digest": "6" * 64, "doctor_status": "healthy", "snapshot_verified": True} if name == "restore" else {"status": "healthy", "snapshot_verified": True})
        elif family == "transport-failures" and name == "backpressure": outcome = {"stalled": True, "resumed": True, "completed": True}
        elif family == "transport-failures" and name == "disconnect-unknown-commit": outcome = {"status": "committed", "transaction_state": "committed", "transaction_id": "6202"}
        else: outcome = {"code": "invalid_request", "category": "invalid-request", "retry": "never", "transaction_state": "none", "request_id": "6000"}
        cases.append({"id": case_id, "outcome": outcome})
    return cases


def receipt(platform: str) -> dict[str, object]:
    lanes = [transcript(lane) for lane in REQUIRED_LANES]
    comparable = canonical_cross_lane(lanes)
    return {
        "schema": "hyphae-native-g6-conformance-receipt-v1",
        "source_commit": "a" * 40,
        "platform": platform,
        "status": "passed",
        "corpus_digest": corpus_digest(),
        "schema_digest": schema_digest(),
        "transcript_digest": digest(comparable),
        "lanes": lanes,
    }


class NativeG6ConformanceTests(unittest.TestCase):
    def test_checked_in_corpus_requires_all_lanes_and_families(self) -> None:
        value = json.loads((ROOT / "conformance/g6/fixtures/corpus.json").read_text())
        checked = validate_corpus(value)
        self.assertEqual(checked["lanes"], list(REQUIRED_LANES))
        self.assertEqual(tuple(checked["families"]), REQUIRED_FAMILIES)

    def test_receipt_requires_every_lane_and_exact_transcript(self) -> None:
        checked = validate_receipt(receipt("linux"))
        self.assertEqual(checked["status"], "passed")

        missing = receipt("linux")
        missing["lanes"].pop()  # type: ignore[union-attr]
        with self.assertRaisesRegex(ConformanceFailure, "every required lane"):
            validate_receipt(missing)

        mismatch = receipt("linux")
        mismatch["lanes"][4]["cases"][0]["outcome"]["product_api_version"] = 3  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "outcome mismatch|transcript mismatch"):
            validate_receipt(mismatch)

    def test_receipt_rejects_identity_and_result_tampering(self) -> None:
        identity = receipt("linux")
        identity["lanes"][2]["start"]["directory_lineage"] = "33" * 24  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "starting native identity mismatch"):
            validate_receipt(identity)

        proof = receipt("linux")
        proof["lanes"][7]["cases"][0]["outcome"]["product_api_version"] = 9  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "outcome mismatch|transcript mismatch"):
            validate_receipt(proof)

        digest_tamper = receipt("linux")
        digest_tamper["transcript_digest"] = "0" * 64
        with self.assertRaisesRegex(ConformanceFailure, "transcript digest"):
            validate_receipt(digest_tamper)

    def test_transcript_rejects_non_fixture_ids_and_literal_labels(self) -> None:
        value = transcript("embedded-rust")
        value["cases"][0]["id"] = "identity"  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "flattened fixture order"):
            validate_receipt({**receipt("linux"), "lanes": [value, *receipt("linux")["lanes"][1:]]})  # type: ignore[index]

        value = transcript("embedded-rust")
        value["cases"][0]["outcome"] = "capabilities"  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "semantic fields"):
            from tools.check_native_g6_conformance import validate_transcript
            validate_transcript(value)

    def test_explicit_applicability_requires_exact_declared_lane_execution(self) -> None:
        value = transcript("cli")
        value["cases"].append({"id": "failures/deadline", "outcome": {"code": "deadline_exceeded", "category": "deadline", "retry": "same-request", "transaction_state": "none", "request_id": "6111"}})  # type: ignore[union-attr]
        with self.assertRaisesRegex(ConformanceFailure, "flattened fixture order"):
            from tools.check_native_g6_conformance import validate_transcript
            validate_transcript(value)

        value = transcript("http")
        value["cases"] = [case for case in value["cases"] if case["id"] != "transport-failures/malformed-input"]  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "flattened fixture order"):
            from tools.check_native_g6_conformance import validate_transcript
            validate_transcript(value)

    def test_receipt_rejects_stable_error_parity_drift(self) -> None:
        value = receipt("linux")
        lane = REQUIRED_LANES[5]
        failure_index = lane_case_ids(lane).index("failures/syntax")
        value["lanes"][5]["cases"][failure_index]["outcome"]["retry"] = "same-request"  # type: ignore[index]
        with self.assertRaisesRegex(ConformanceFailure, "outcome mismatch|transcript mismatch|error parity"):
            validate_receipt(value)

    def test_aggregate_requires_final_linux_macos_windows_receipts(self) -> None:
        receipts = [receipt(platform) for platform in PLATFORMS]
        value = aggregate(receipts)
        self.assertEqual(value["platforms"], list(PLATFORMS))
        self.assertEqual(value["status"], "passed")

        with self.assertRaisesRegex(ConformanceFailure, "exactly three"):
            aggregate(receipts[:-1])
        reordered = [copy.deepcopy(receipts[1]), receipts[0], receipts[2]]
        with self.assertRaisesRegex(ConformanceFailure, "ordered"):
            aggregate(reordered)
        different_sha = copy.deepcopy(receipts)
        different_sha[2]["source_commit"] = "b" * 40
        with self.assertRaisesRegex(ConformanceFailure, "source_commit"):
            aggregate(different_sha)

    def test_aggregate_normalizes_platform_local_snapshot_identity(self) -> None:
        receipts = [receipt(platform) for platform in PLATFORMS]
        for index, value in enumerate(receipts):
            for lane in value["lanes"]:  # type: ignore[union-attr]
                for case in lane["cases"]:
                    snapshot = case["outcome"].get("snapshot")
                    if isinstance(snapshot, dict):
                        snapshot["directory_lineage"] = f"{index + 1:02x}" * 24
            value["transcript_digest"] = digest(canonical_cross_lane(value["lanes"]))  # type: ignore[index]
        self.assertEqual(aggregate(receipts)["status"], "passed")


if __name__ == "__main__":
    unittest.main()
