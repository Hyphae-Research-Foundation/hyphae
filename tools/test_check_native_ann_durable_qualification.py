#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import io
import json
import re
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from tools.check_native_ann_durable_qualification import (
    GateFailure,
    QualificationIncomplete,
    main,
    validate,
)


COMMIT = "a" * 40
CORPUS = "c" * 64
SCHEMA_PATH = Path(
    "contracts/json-schema/native-ann-durable-qualification-v1.schema.json"
)


def _schema_target(schema: dict, root: dict) -> dict:
    reference = schema.get("$ref")
    if reference is None:
        return schema
    if not reference.startswith("#/$defs/"):
        raise AssertionError(f"unsupported schema reference: {reference}")
    return root["$defs"][reference.removeprefix("#/$defs/")]


def assert_schema_value(value: object, schema: dict, root: dict, path: str) -> None:
    schema = _schema_target(schema, root)
    if "oneOf" in schema:
        matches = 0
        for candidate in schema["oneOf"]:
            try:
                assert_schema_value(value, candidate, root, path)
            except AssertionError:
                continue
            matches += 1
        if matches != 1:
            raise AssertionError(f"{path} matched {matches} oneOf branches")
        return
    if "const" in schema and value != schema["const"]:
        raise AssertionError(f"{path} differs from schema const")
    if "enum" in schema and value not in schema["enum"]:
        raise AssertionError(f"{path} is outside schema enum")
    schema_type = schema.get("type")
    if schema_type == "null":
        if value is not None:
            raise AssertionError(f"{path} is not null")
        return
    if schema_type == "object":
        if not isinstance(value, dict):
            raise AssertionError(f"{path} is not an object")
        required = set(schema.get("required", []))
        if not required.issubset(value):
            raise AssertionError(f"{path} omits required fields")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False and set(value) - set(properties):
            raise AssertionError(f"{path} has additional fields")
        for name, member in value.items():
            if name in properties:
                assert_schema_value(member, properties[name], root, f"{path}.{name}")
        return
    if schema_type == "array":
        if not isinstance(value, list):
            raise AssertionError(f"{path} is not an array")
        if schema.get("uniqueItems") and len(value) != len(
            {json.dumps(item, sort_keys=True) for item in value}
        ):
            raise AssertionError(f"{path} contains duplicate items")
        for index, item in enumerate(value):
            assert_schema_value(item, schema["items"], root, f"{path}[{index}]")
        return
    if schema_type == "string":
        if not isinstance(value, str):
            raise AssertionError(f"{path} is not a string")
        pattern = schema.get("pattern")
        if pattern is not None and re.search(pattern, value) is None:
            raise AssertionError(f"{path} does not match schema pattern")
    elif schema_type == "integer":
        if not isinstance(value, int) or isinstance(value, bool):
            raise AssertionError(f"{path} is not an integer")
        if value < schema.get("minimum", value):
            raise AssertionError(f"{path} is below schema minimum")
        if value > schema.get("maximum", value):
            raise AssertionError(f"{path} exceeds schema maximum")


def qualify(payload: dict) -> dict:
    return validate(
        payload,
        COMMIT,
        mode="qualification",
        expected_corpus_identity=CORPUS,
    )


def receipt() -> dict:
    published = "1" * 64
    delta_view = "2" * 64
    consolidated = "3" * 64
    default_results = "4" * 64
    return {
        "schema": "hyphae-native-ann-durable-qualification-v1",
        "status": "diagnostic",
        "source": {"commit": COMMIT, "tree": "b" * 40, "clean": True},
        "dataset": {
            "generator": "hyphae-ann-durable-qualification-corpus-v2",
            "digest": CORPUS,
            "vectors": 512,
            "dimension": 384,
            "metric": "cosine",
        },
        "build": {
            "builder": "partitioned-hnsw-v1",
            "input_identity": "d" * 64,
            "aggregate_identity": published,
            "expected_base_identity": "e" * 64,
            "expected_view_identity": "e" * 64,
            "published_base_identity": published,
            "published_view_identity": published,
            "vector_count": 512,
            "logical_partitions": 64,
            "planned_workers": 4,
            "planned_memory_bytes": 1_048_576,
            "worker_batches": 4,
            "routing_policy": "metric-bound-adaptive-v1",
        },
        "quality": {
            "query_set_identity": "0" * 64,
            "queries": 128,
            "k": 10,
            "ef_search": 80,
            "selected_partitions": 32,
            "certified_selected_queries": 128,
            "full_fanout_fallback_queries": 0,
            "maximum_searched_partitions": 32,
            "selected_query_recall_ppm": 975_000,
            "minimum_selected_query_recall_ppm": 950_000,
            "oracle_result_identity": "5" * 64,
            "default_result_identity": default_results,
            "full_fanout_result_identity": default_results,
            "selected_result_identity": "6" * 64,
            "full_fanout_equals_default": True,
        },
        "lifecycle": {
            "initial_reopen": {
                "base_identity": published,
                "view_identity": published,
                "selected_result_identity": "6" * 64,
            },
            "delta": {
                "before_base_identity": published,
                "before_view_identity": published,
                "after_base_identity": published,
                "after_view_identity": delta_view,
                "upserted_vectors": 2,
                "deleted_vectors": 1,
                "upserts_visible": True,
                "deletes_hidden": True,
                "visible_result_identity": "7" * 64,
            },
            "consolidation": {
                "before_base_identity": published,
                "before_view_identity": delta_view,
                "after_base_identity": consolidated,
                "after_view_identity": consolidated,
                "remaining_delta_records": 0,
                "view_preserved": True,
                "visible_result_identity": "7" * 64,
                "partitioned_base_preserved": True,
                "routing_outcome_after": "selected-certified",
                "total_partitions_after": 64,
            },
            "final_reopen": {
                "base_identity": consolidated,
                "view_identity": consolidated,
                "delta_records": 0,
                "view_preserved": True,
                "visible_result_identity": "7" * 64,
                "routing_outcome": "selected-certified",
                "total_partitions": 64,
            },
        },
        "missing_gate_evidence": [],
        "claims": [],
        "closure_declared": False,
    }


class NativeAnnDurableQualificationTests(unittest.TestCase):
    def test_schema_structurally_accepts_the_canonical_fixture(self) -> None:
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        assert_schema_value(receipt(), schema, schema, "receipt")

    def test_qualification_accepts_complete_exact_bound_evidence(self) -> None:
        audit = qualify(receipt())
        self.assertEqual(audit["status"], "passed")
        self.assertTrue(audit["qualification_candidate"])
        self.assertFalse(audit["closure_declared"])
        self.assertEqual(audit["metric"], "cosine")
        self.assertEqual(audit["certified_selected_queries"], 128)
        self.assertEqual(audit["full_fanout_fallback_queries"], 0)
        self.assertEqual(audit["maximum_searched_partitions"], 32)

    def test_diagnostic_discloses_missing_evidence_without_closure(self) -> None:
        payload = receipt()
        payload["quality"] = None
        payload["lifecycle"]["delta"] = None
        payload["missing_gate_evidence"] = [
            "adaptive-routing-certification",
            "delta-visibility",
            "full-fanout-equality",
            "selected-recall-floor",
        ]
        audit = validate(payload, COMMIT, mode="diagnostic")
        self.assertEqual(audit["status"], "diagnostic")
        self.assertFalse(audit["qualification_candidate"])
        self.assertEqual(
            audit["missing_gate_evidence"], payload["missing_gate_evidence"]
        )
        with self.assertRaisesRegex(
            QualificationIncomplete, "no closure: qualification evidence missing"
        ):
            qualify(payload)

    def test_qualification_cli_emits_no_closure_diagnostic_and_fails(self) -> None:
        payload = receipt()
        payload["quality"] = None
        payload["missing_gate_evidence"] = [
            "adaptive-routing-certification",
            "full-fanout-equality",
            "selected-recall-floor",
        ]
        output = io.StringIO()
        arguments = [
            "check_native_ann_durable_qualification.py",
            "receipt.json",
            COMMIT,
            "--expected-corpus-identity",
            CORPUS,
            "--mode",
            "qualification",
        ]
        with patch("sys.argv", arguments):
            with patch("pathlib.Path.read_text", return_value=json.dumps(payload)):
                with redirect_stdout(output):
                    self.assertEqual(main(), 1)
        diagnostic = json.loads(output.getvalue())
        self.assertEqual(diagnostic["status"], "diagnostic")
        self.assertFalse(diagnostic["qualification_candidate"])
        self.assertFalse(diagnostic["closure_declared"])

    def test_rejects_recall_below_exact_floor(self) -> None:
        payload = receipt()
        payload["quality"]["minimum_selected_query_recall_ppm"] = 949_999
        with self.assertRaisesRegex(GateFailure, "950000"):
            qualify(payload)

    def test_accepts_only_the_three_certified_metrics(self) -> None:
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        for metric in ("squared-l2", "cosine", "negative-dot"):
            with self.subTest(metric=metric):
                payload = receipt()
                payload["dataset"]["metric"] = metric
                assert_schema_value(payload, schema, schema, "receipt")
                self.assertEqual(qualify(payload)["metric"], metric)
        payload = receipt()
        payload["dataset"]["metric"] = "dot"
        with self.assertRaisesRegex(AssertionError, "schema enum"):
            assert_schema_value(payload, schema, schema, "receipt")
        with self.assertRaisesRegex(GateFailure, "metric identity"):
            qualify(payload)

    def test_rejects_false_or_forged_full_fanout_equality(self) -> None:
        payload = receipt()
        payload["quality"]["full_fanout_equals_default"] = False
        with self.assertRaisesRegex(GateFailure, "full-fanout"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["quality"]["full_fanout_result_identity"] = "7" * 64
        with self.assertRaisesRegex(GateFailure, "full-fanout"):
            qualify(payload)

    def test_rejects_source_corpus_and_build_view_substitution(self) -> None:
        with self.assertRaisesRegex(GateFailure, "expected corpus identity"):
            validate(receipt(), COMMIT, mode="qualification")
        with self.assertRaisesRegex(GateFailure, "source commit"):
            validate(
                receipt(),
                "8" * 40,
                mode="qualification",
                expected_corpus_identity=CORPUS,
            )
        with self.assertRaisesRegex(GateFailure, "corpus identity"):
            validate(
                receipt(),
                COMMIT,
                mode="qualification",
                expected_corpus_identity="8" * 64,
            )
        payload = receipt()
        payload["build"]["vector_count"] = 511
        with self.assertRaisesRegex(GateFailure, "corpus vector count"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["build"]["published_view_identity"] = "9" * 64
        with self.assertRaisesRegex(GateFailure, "published build/view"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["build"]["routing_policy"] = "another-policy"
        with self.assertRaisesRegex(GateFailure, "routing policy identity"):
            qualify(payload)

    def test_keeps_logical_partitions_independent_from_workers(self) -> None:
        audit = qualify(receipt())
        self.assertEqual(audit["logical_partitions"], 64)
        self.assertEqual(audit["planned_workers"], 4)
        payload = receipt()
        payload["build"]["planned_workers"] = 65
        with self.assertRaisesRegex(GateFailure, "worker/partition bounds"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["build"]["logical_partitions"] = 16
        with self.assertRaisesRegex(GateFailure, "worker/partition bounds"):
            qualify(payload)

    def test_requires_every_qualification_query_to_prune_without_fallback(self) -> None:
        mutations = (
            ("certified_selected_queries", 127),
            ("full_fanout_fallback_queries", 1),
            ("maximum_searched_partitions", 15),
            ("maximum_searched_partitions", 64),
        )
        for field, value in mutations:
            with self.subTest(field=field, value=value):
                payload = receipt()
                payload["quality"][field] = value
                with self.assertRaisesRegex(
                    GateFailure, "certified within its partition budget"
                ):
                    qualify(payload)
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        payload = receipt()
        payload["quality"]["full_fanout_fallback_queries"] = 1
        with self.assertRaisesRegex(AssertionError, "oneOf branches"):
            assert_schema_value(payload, schema, schema, "receipt")

    def test_rejects_broken_reopen_delta_or_consolidation_chain(self) -> None:
        payload = receipt()
        payload["lifecycle"]["initial_reopen"]["view_identity"] = "8" * 64
        with self.assertRaisesRegex(GateFailure, "initial reopen"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["lifecycle"]["delta"]["after_base_identity"] = "8" * 64
        with self.assertRaisesRegex(GateFailure, "delta base"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["lifecycle"]["consolidation"]["view_preserved"] = False
        with self.assertRaisesRegex(GateFailure, "consolidation"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["lifecycle"]["consolidation"]["partitioned_base_preserved"] = False
        payload["lifecycle"]["consolidation"]["routing_outcome_after"] = (
            "single-generation-fallback"
        )
        payload["lifecycle"]["consolidation"]["total_partitions_after"] = 1
        with self.assertRaisesRegex(GateFailure, "partition"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["lifecycle"]["final_reopen"]["base_identity"] = "8" * 64
        with self.assertRaisesRegex(GateFailure, "final reopen"):
            qualify(payload)
        payload = copy.deepcopy(receipt())
        payload["lifecycle"]["final_reopen"]["routing_outcome"] = (
            "single-generation-fallback"
        )
        payload["lifecycle"]["final_reopen"]["total_partitions"] = 1
        with self.assertRaisesRegex(GateFailure, "final reopen partitions"):
            qualify(payload)

    def test_rejects_hidden_missing_evidence_and_closure_claim(self) -> None:
        payload = receipt()
        payload["lifecycle"]["final_reopen"] = None
        with self.assertRaisesRegex(GateFailure, "missing-evidence disclosure"):
            validate(payload, COMMIT, mode="diagnostic")
        payload = copy.deepcopy(receipt())
        payload["closure_declared"] = True
        with self.assertRaisesRegex(GateFailure, "open state"):
            qualify(payload)


if __name__ == "__main__":
    unittest.main()
