#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed local qualification for the selected durable ANN route."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-ann-durable-qualification-v1"
AUDIT_SCHEMA = "hyphae-native-ann-durable-qualification-audit-v1"
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
MINIMUM_SELECTED_QUERY_RECALL_PPM = 950_000
MAXIMUM_LOGICAL_PARTITIONS = 111
QUALIFICATION_VECTOR_COUNT = 512
QUALIFICATION_LOGICAL_PARTITIONS = 64
QUALIFICATION_PREFERRED_PARTITIONS = 32


class GateFailure(ValueError):
    """The receipt is malformed or contradicts its evidence."""


class QualificationIncomplete(GateFailure):
    """The receipt is valid diagnostics but cannot qualify the route."""

    def __init__(self, missing: list[str]) -> None:
        self.missing = missing
        super().__init__(
            "no closure: qualification evidence missing: " + ", ".join(missing)
        )


def _object(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise GateFailure(f"{label} fields mismatch")
    return value


def _integer(value: Any, label: str, minimum: int = 0) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise GateFailure(f"{label} is invalid")
    return value


def _digest(value: Any, pattern: re.Pattern[str], label: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise GateFailure(f"{label} is not canonical")
    return value


def _truth(value: Any, label: str) -> None:
    if value is not True:
        raise GateFailure(f"{label} was not demonstrated")


def _validate_source(payload: dict[str, Any], expected_commit: str) -> None:
    source = _object(payload["source"], {"commit", "tree", "clean"}, "source")
    if _digest(source["commit"], HEX40, "source commit") != expected_commit:
        raise GateFailure("source commit does not match the exact expected SHA")
    _digest(source["tree"], HEX40, "source tree")
    _truth(source["clean"], "clean source tree")


def _validate_dataset(payload: dict[str, Any]) -> tuple[int, int]:
    dataset = _object(
        payload["dataset"],
        {"generator", "digest", "vectors", "dimension", "metric"},
        "dataset",
    )
    if dataset["generator"] != "hyphae-ann-durable-qualification-corpus-v2" or dataset[
        "metric"
    ] not in {"squared-l2", "cosine", "negative-dot"}:
        raise GateFailure("corpus generator or metric identity mismatch")
    _digest(dataset["digest"], HEX64, "corpus digest")
    vectors = _integer(dataset["vectors"], "corpus vectors", 1)
    dimension = _integer(dataset["dimension"], "corpus dimension", 1)
    if vectors != QUALIFICATION_VECTOR_COUNT:
        raise GateFailure("qualification corpus must exercise 512 vectors")
    if dimension > 65_535:
        raise GateFailure("corpus dimension exceeds the native field")
    return vectors, dimension


def _validate_build(payload: dict[str, Any], vectors: int) -> dict[str, Any]:
    build = _object(
        payload["build"],
        {
            "builder",
            "input_identity",
            "aggregate_identity",
            "expected_base_identity",
            "expected_view_identity",
            "published_base_identity",
            "published_view_identity",
            "vector_count",
            "logical_partitions",
            "planned_workers",
            "planned_memory_bytes",
            "worker_batches",
            "routing_policy",
        },
        "build",
    )
    if build["builder"] != "partitioned-hnsw-v1":
        raise GateFailure("selected durable ANN builder identity mismatch")
    for field in (
        "input_identity",
        "aggregate_identity",
        "expected_base_identity",
        "expected_view_identity",
        "published_base_identity",
        "published_view_identity",
    ):
        _digest(build[field], HEX64, field.replace("_", " "))
    if build["routing_policy"] != "metric-bound-adaptive-v1":
        raise GateFailure("selected ANN routing policy identity mismatch")
    if _integer(build["vector_count"], "build vector count", 1) != vectors:
        raise GateFailure("build/corpus vector count mismatch")
    partitions = _integer(build["logical_partitions"], "logical partitions", 2)
    workers = _integer(build["planned_workers"], "planned workers", 1)
    batches = _integer(build["worker_batches"], "worker batches", 1)
    _integer(build["planned_memory_bytes"], "planned memory", 1)
    if (
        partitions != QUALIFICATION_LOGICAL_PARTITIONS
        or partitions > MAXIMUM_LOGICAL_PARTITIONS
        or partitions > vectors
        or workers > partitions
        or batches > workers
        or (workers > 1 and batches <= 1)
    ):
        raise GateFailure("logical worker/partition bounds mismatch")
    if build["expected_base_identity"] != build["expected_view_identity"]:
        raise GateFailure("empty pre-publication build/view identity mismatch")
    if not (
        build["published_base_identity"]
        == build["published_view_identity"]
        == build["aggregate_identity"]
    ):
        raise GateFailure("published build/view identity mismatch")
    return build


def _validate_quality(
    quality_value: Any, vectors: int, logical_partitions: int
) -> dict[str, Any] | None:
    if quality_value is None:
        return None
    quality = _object(
        quality_value,
        {
            "query_set_identity",
            "queries",
            "k",
            "ef_search",
            "selected_partitions",
            "certified_selected_queries",
            "full_fanout_fallback_queries",
            "maximum_searched_partitions",
            "selected_query_recall_ppm",
            "minimum_selected_query_recall_ppm",
            "oracle_result_identity",
            "default_result_identity",
            "full_fanout_result_identity",
            "selected_result_identity",
            "full_fanout_equals_default",
        },
        "quality",
    )
    for field in (
        "query_set_identity",
        "oracle_result_identity",
        "default_result_identity",
        "full_fanout_result_identity",
        "selected_result_identity",
    ):
        _digest(quality[field], HEX64, field.replace("_", " "))
    queries = _integer(quality["queries"], "query count", 1)
    k = _integer(quality["k"], "quality k", 1)
    ef_search = _integer(quality["ef_search"], "quality ef_search", 1)
    selected = _integer(quality["selected_partitions"], "selected partitions", 1)
    certified = _integer(
        quality["certified_selected_queries"], "certified selected queries"
    )
    fallbacks = _integer(
        quality["full_fanout_fallback_queries"], "full-fanout fallback queries"
    )
    maximum_searched = _integer(
        quality["maximum_searched_partitions"], "maximum searched partitions", 1
    )
    if queries > vectors or k > vectors or ef_search < k or ef_search > 256:
        raise GateFailure("quality query bounds mismatch")
    if selected != QUALIFICATION_PREFERRED_PARTITIONS or selected >= logical_partitions:
        raise GateFailure("selected route did not use the fixed preferred partition budget")
    if certified != queries or fallbacks != 0 or maximum_searched != selected:
        raise GateFailure(
            "every qualification query must be certified within its partition budget"
        )
    aggregate_recall = _integer(
        quality["selected_query_recall_ppm"], "selected aggregate recall"
    )
    minimum_recall = _integer(
        quality["minimum_selected_query_recall_ppm"], "selected minimum recall"
    )
    if aggregate_recall > 1_000_000 or minimum_recall > aggregate_recall:
        raise GateFailure("selected recall bounds mismatch")
    if minimum_recall < MINIMUM_SELECTED_QUERY_RECALL_PPM:
        raise GateFailure(
            "minimum selected-query recall must be >= 950000 ppm"
        )
    if (
        quality["full_fanout_equals_default"] is not True
        or quality["full_fanout_result_identity"]
        != quality["default_result_identity"]
    ):
        raise GateFailure("full-fanout equality was not demonstrated")
    return quality


def _validate_lifecycle(
    lifecycle_value: Any, build: dict[str, Any], quality: dict[str, Any] | None
) -> dict[str, Any]:
    lifecycle = _object(
        lifecycle_value,
        {"initial_reopen", "delta", "consolidation", "final_reopen"},
        "lifecycle",
    )
    published = build["published_base_identity"]
    initial = lifecycle["initial_reopen"]
    if initial is not None:
        initial = _object(
            initial,
            {"base_identity", "view_identity", "selected_result_identity"},
            "initial reopen",
        )
        for field in initial:
            _digest(initial[field], HEX64, f"initial reopen {field}")
        if (
            initial["base_identity"] != published
            or initial["view_identity"] != published
        ):
            raise GateFailure(
                "initial reopen does not reproduce the published build/view"
            )
        if (
            quality is not None
            and initial["selected_result_identity"]
            != quality["selected_result_identity"]
        ):
            raise GateFailure("initial reopen does not reproduce selected results")

    delta = lifecycle["delta"]
    if delta is not None:
        delta = _object(
            delta,
            {
                "before_base_identity",
                "before_view_identity",
                "after_base_identity",
                "after_view_identity",
                "upserted_vectors",
                "deleted_vectors",
                "upserts_visible",
                "deletes_hidden",
                "visible_result_identity",
            },
            "delta",
        )
        for field in (
            "before_base_identity",
            "before_view_identity",
            "after_base_identity",
            "after_view_identity",
            "visible_result_identity",
        ):
            _digest(delta[field], HEX64, f"delta {field}")
        _integer(delta["upserted_vectors"], "delta upserts", 1)
        _integer(delta["deleted_vectors"], "delta deletes", 1)
        _truth(delta["upserts_visible"], "delta upsert visibility")
        _truth(delta["deletes_hidden"], "delta delete visibility")
        if (
            delta["before_base_identity"] != published
            or delta["before_view_identity"] != published
            or delta["after_base_identity"] != published
            or delta["after_view_identity"] == published
        ):
            raise GateFailure("delta base/view identity chain mismatch")

    consolidation = lifecycle["consolidation"]
    if consolidation is not None:
        consolidation = _object(
            consolidation,
            {
                "before_base_identity",
                "before_view_identity",
                "after_base_identity",
                "after_view_identity",
                "remaining_delta_records",
                "view_preserved",
                "visible_result_identity",
                "partitioned_base_preserved",
                "routing_outcome_after",
                "total_partitions_after",
            },
            "consolidation",
        )
        for field in (
            "before_base_identity",
            "before_view_identity",
            "after_base_identity",
            "after_view_identity",
            "visible_result_identity",
        ):
            _digest(consolidation[field], HEX64, f"consolidation {field}")
        _integer(
            consolidation["remaining_delta_records"],
            "remaining consolidation deltas",
        )
        _truth(consolidation["view_preserved"], "consolidation view preservation")
        _truth(
            consolidation["partitioned_base_preserved"],
            "consolidation partition preservation",
        )
        total_partitions_after = _integer(
            consolidation["total_partitions_after"],
            "consolidation partition count",
            2,
        )
        if (
            consolidation["routing_outcome_after"] != "selected-certified"
            or total_partitions_after != build["logical_partitions"]
        ):
            raise GateFailure(
                "consolidation did not preserve the partitioned selected route"
            )
        if delta is not None and (
            consolidation["before_base_identity"] != delta["after_base_identity"]
            or consolidation["before_view_identity"] != delta["after_view_identity"]
            or consolidation["visible_result_identity"]
            != delta["visible_result_identity"]
        ):
            raise GateFailure("consolidation does not consume the exact delta view")
        if (
            consolidation["after_base_identity"]
            == consolidation["before_base_identity"]
            or consolidation["after_view_identity"]
            != consolidation["after_base_identity"]
            or consolidation["remaining_delta_records"] != 0
        ):
            raise GateFailure("consolidation did not publish a clean replacement view")

    final = lifecycle["final_reopen"]
    if final is not None:
        final = _object(
            final,
            {
                "base_identity",
                "view_identity",
                "delta_records",
                "view_preserved",
                "visible_result_identity",
                "routing_outcome",
                "total_partitions",
            },
            "final reopen",
        )
        for field in ("base_identity", "view_identity", "visible_result_identity"):
            _digest(final[field], HEX64, f"final reopen {field}")
        _integer(final["delta_records"], "final reopen deltas")
        _truth(final["view_preserved"], "final reopen view preservation")
        total_partitions = _integer(
            final["total_partitions"], "final reopen partitions", 2
        )
        if (
            final["routing_outcome"] != "selected-certified"
            or total_partitions != build["logical_partitions"]
        ):
            raise GateFailure("final reopen did not preserve the reopened partitioned route")
        if consolidation is not None and (
            final["base_identity"] != consolidation["after_base_identity"]
            or final["view_identity"] != consolidation["after_view_identity"]
            or final["visible_result_identity"]
            != consolidation["visible_result_identity"]
            or final["delta_records"] != 0
        ):
            raise GateFailure("final reopen does not reproduce the consolidated view")
    return lifecycle


def _missing_evidence(
    quality: dict[str, Any] | None, lifecycle: dict[str, Any]
) -> list[str]:
    missing: list[str] = []
    if quality is None:
        missing.extend(
            (
                "adaptive-routing-certification",
                "full-fanout-equality",
                "selected-recall-floor",
            )
        )
    for field, label in (
        ("initial_reopen", "durable-reopen"),
        ("delta", "delta-visibility"),
        ("consolidation", "consolidation-visibility"),
        ("final_reopen", "post-consolidation-reopen"),
    ):
        if lifecycle[field] is None:
            missing.append(label)
    return sorted(missing)


def validate(
    payload: dict[str, Any],
    expected_commit: str,
    *,
    mode: str,
    expected_corpus_identity: str | None = None,
) -> dict[str, Any]:
    _digest(expected_commit, HEX40, "expected commit")
    if mode not in {"diagnostic", "qualification"}:
        raise GateFailure("checker mode is invalid")
    _object(
        payload,
        {
            "schema",
            "status",
            "source",
            "dataset",
            "build",
            "quality",
            "lifecycle",
            "missing_gate_evidence",
            "claims",
            "closure_declared",
        },
        "qualification receipt",
    )
    if (
        payload["schema"] != SCHEMA
        or payload["status"] != "diagnostic"
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("qualification receipt identity or open state mismatch")
    _validate_source(payload, expected_commit)
    vectors, dimension = _validate_dataset(payload)
    if expected_corpus_identity is not None:
        _digest(expected_corpus_identity, HEX64, "expected corpus identity")
        if payload["dataset"]["digest"] != expected_corpus_identity:
            raise GateFailure("corpus identity does not match the expected corpus")
    elif mode == "qualification":
        raise GateFailure("qualification requires an expected corpus identity")
    build = _validate_build(payload, vectors)
    quality = _validate_quality(
        payload["quality"], vectors, build["logical_partitions"]
    )
    lifecycle = _validate_lifecycle(payload["lifecycle"], build, quality)
    missing = _missing_evidence(quality, lifecycle)
    disclosed = payload["missing_gate_evidence"]
    if (
        not isinstance(disclosed, list)
        or disclosed != missing
        or len(disclosed) != len(set(disclosed))
    ):
        raise GateFailure("qualification missing-evidence disclosure mismatch")
    if mode == "qualification" and missing:
        raise QualificationIncomplete(missing)
    qualified = mode == "qualification"
    return {
        "schema": AUDIT_SCHEMA,
        "status": "passed" if qualified else "diagnostic",
        "source_commit": expected_commit,
        "corpus_identity": payload["dataset"]["digest"],
        "metric": payload["dataset"]["metric"],
        "build_identity": build["aggregate_identity"],
        "view_identity": (
            lifecycle["final_reopen"]["view_identity"]
            if lifecycle["final_reopen"] is not None
            else build["published_view_identity"]
        ),
        "vectors": vectors,
        "dimension": dimension,
        "logical_partitions": build["logical_partitions"],
        "planned_workers": build["planned_workers"],
        "routing_policy": build["routing_policy"],
        "selected_partitions": (
            quality["selected_partitions"] if quality is not None else None
        ),
        "certified_selected_queries": (
            quality["certified_selected_queries"] if quality is not None else None
        ),
        "full_fanout_fallback_queries": (
            quality["full_fanout_fallback_queries"]
            if quality is not None
            else None
        ),
        "maximum_searched_partitions": (
            quality["maximum_searched_partitions"]
            if quality is not None
            else None
        ),
        "minimum_selected_query_recall_ppm": (
            quality["minimum_selected_query_recall_ppm"]
            if quality is not None
            else None
        ),
        "missing_gate_evidence": missing,
        "qualification_candidate": qualified,
        "closure_declared": False,
    }


def _diagnostic_audit(error: QualificationIncomplete) -> dict[str, Any]:
    return {
        "schema": AUDIT_SCHEMA,
        "status": "diagnostic",
        "diagnostic": str(error),
        "missing_gate_evidence": error.missing,
        "qualification_candidate": False,
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("receipt", type=Path)
    parser.add_argument("expected_commit")
    parser.add_argument(
        "--mode", choices=("diagnostic", "qualification"), default="diagnostic"
    )
    parser.add_argument("--expected-corpus-identity")
    arguments = parser.parse_args()
    try:
        payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        audit = validate(
            payload,
            arguments.expected_commit,
            mode=arguments.mode,
            expected_corpus_identity=arguments.expected_corpus_identity,
        )
    except QualificationIncomplete as error:
        print(json.dumps(_diagnostic_audit(error), sort_keys=True))
        return 1
    except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
        parser.error(str(error))
    print(json.dumps(audit, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
