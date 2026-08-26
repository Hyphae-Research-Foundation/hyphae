#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Evaluate frozen LoCoMo Slice A candidates with nested conversation folds.

The input traces are the NDJSON progress files produced by
``tools/long_term_memory_benchmarks.py --progress``. A frozen manifest binds
those traces to an exact ten-conversation query roster, a metric contract, a
baseline, candidate configurations, and a total simplicity order. This tool
does not execute retrieval and emits only a deterministic local diagnostic.

Manifest shape (paths are relative to the manifest):

{
  "schema": "hyphae-locomo-slice-a-candidate-manifest-v1",
  "slice": "A",
  "primary_metric": "evidence_recall@10",
  "metric_names": ["evidence_recall@10", "mrr@50", "ndcg@10",
                   "recall_all@10", "recall_any@10"],
  "conversations": [{"id": "sample-0", "queries": [
    {"id": "sample-0:0", "source_ordinal": 0, "segment": "1",
     "expected_targets": ["sample-0/D1:1"]}
  ]}],
  "baseline": {"id": "baseline", "configuration": {
                 "document_text_views": ["bare"],
                 "analyzer_english_stop": false,
                 "analyzer_english_stem": false,
                 "bm25_k1_micros": null, "bm25_b_micros": null,
                 "candidate_limit": 1000, "qrel_mode": "audited-v2"},
               "trace": "traces/baseline.jsonl", "trace_sha256": "...",
               "trace_protocol_sha256": "..."},
  "candidates": [{"id": "candidate-1", "simplicity_rank": 0,
                  "configuration": {"document_text_views": ["timestamp"],
                    "analyzer_english_stop": false,
                    "analyzer_english_stem": false,
                    "bm25_k1_micros": null, "bm25_b_micros": null,
                    "candidate_limit": 1000, "qrel_mode": "audited-v2"},
                  "trace": "traces/candidate-1.jsonl", "trace_sha256": "...",
                  "trace_protocol_sha256": "..."}]
}
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import sys
from collections import defaultdict
from fractions import Fraction
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Sequence


MANIFEST_SCHEMA = "hyphae-locomo-slice-a-candidate-manifest-v1"
RESULT_SCHEMA = "hyphae-locomo-slice-a-statistical-evaluation-v1"
TRACE_SCHEMA = "hyphae-long-term-memory-query-trace-v2"
TRACE_QUERY_SCHEMA = "hyphae-long-term-memory-query-result-v2"
TRACE_PROTOCOL_SCHEMA = "hyphae-long-term-memory-retrieval-protocol-v2"
TRACE_DIGEST_CANONICALIZATION = "sha256-canonical-json-utf8-v1"
LOCOMO_DATASET_SHA256 = "79fa87e90f04081343b8c8debecb80a9a6842b76a7aa537dc9fdf651ea698ff4"
CONFIGURATION_KEYS = {
    "document_text_views",
    "rrf_weights",
    "analyzer_english_stop",
    "analyzer_english_stem",
    "bm25_k1_micros",
    "bm25_b_micros",
    "candidate_limit",
    "qrel_mode",
}
CONVERSATION_COUNT = 10
BOOTSTRAP_SEED = 20_260_824
BOOTSTRAP_REPLICATES = 10_000
BOOTSTRAP_CONFIDENCE = 0.95
HEX64 = re.compile(r"[0-9a-f]{64}")
IDENTIFIER = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
METRIC_NAME = re.compile(
    r"(?:evidence_recall|recall_any|recall_all|ndcg|mrr)@[1-9][0-9]*"
)
TIMESTAMP_KEYS = {
    "timestamp",
    "timestamps",
    "created_at",
    "completed_at",
    "evaluated_at",
    "generated_at",
    "started_at",
    "finished_at",
}


class StatisticalEvaluationError(Exception):
    """A frozen statistical input or invariant is invalid."""


def _reject_json_constant(value: str) -> None:
    raise StatisticalEvaluationError(f"non-finite JSON number is invalid: {value}")


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise StatisticalEvaluationError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def parse_json(text: str, context: str) -> Any:
    try:
        return json.loads(
            text,
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (json.JSONDecodeError, UnicodeError) as error:
        raise StatisticalEvaluationError(f"{context} is invalid JSON") from error


def canonical_json_bytes(value: Any) -> bytes:
    try:
        encoded = json.dumps(
            value,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
    except (TypeError, ValueError) as error:
        raise StatisticalEvaluationError("value cannot be canonically encoded") from error
    return encoded.encode("ascii")


def canonical_digest(value: Any) -> str:
    return hashlib.sha256(canonical_json_bytes(value)).hexdigest()


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
    except OSError as error:
        raise StatisticalEvaluationError(f"cannot read input: {path}") from error
    return digest.hexdigest()


def require_keys(value: Any, expected: set[str], context: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        raise StatisticalEvaluationError(f"{context} has unexpected fields")
    return value


def _reject_timestamp_keys(value: Any, context: str) -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            normalized = key.lower()
            if normalized in TIMESTAMP_KEYS or normalized.endswith("_timestamp"):
                raise StatisticalEvaluationError(
                    f"{context} contains timestamp identity field {key!r}"
                )
            _reject_timestamp_keys(child, f"{context}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _reject_timestamp_keys(child, f"{context}[{index}]")
    elif isinstance(value, float) and not math.isfinite(value):
        raise StatisticalEvaluationError(f"{context} contains a non-finite number")
    elif not isinstance(value, (str, int, float, bool, type(None))):
        raise StatisticalEvaluationError(f"{context} is not a JSON value")


def _reject_absolute_paths(value: Any, context: str) -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            _reject_absolute_paths(child, f"{context}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _reject_absolute_paths(child, f"{context}[{index}]")
    elif isinstance(value, str) and (
        value.startswith(("/", "\\")) or re.match(r"^[A-Za-z]:[\\/]", value)
    ):
        raise StatisticalEvaluationError(f"{context} contains a host-specific absolute path")


def _validate_identifier(value: Any, context: str) -> str:
    if not isinstance(value, str) or IDENTIFIER.fullmatch(value) is None:
        raise StatisticalEvaluationError(f"{context} is not a canonical identifier")
    return value


def _validate_trace_path(value: Any, context: str) -> str:
    if not isinstance(value, str) or not value or "\\" in value:
        raise StatisticalEvaluationError(f"{context} must be a relative POSIX path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in path.parts):
        raise StatisticalEvaluationError(f"{context} must be a normalized relative path")
    return value


def _validate_run(
    value: Any, context: str, candidate: bool
) -> dict[str, Any]:
    expected = {
        "id",
        "configuration",
        "trace",
        "trace_sha256",
        "trace_protocol_sha256",
    }
    if candidate:
        expected.add("simplicity_rank")
    run = require_keys(value, expected, context)
    _validate_identifier(run["id"], f"{context}.id")
    configuration = run["configuration"]
    if not isinstance(configuration, dict):
        raise StatisticalEvaluationError(f"{context}.configuration must be an object")
    _reject_timestamp_keys(configuration, f"{context}.configuration")
    _reject_absolute_paths(configuration, f"{context}.configuration")
    if set(configuration) != CONFIGURATION_KEYS:
        raise StatisticalEvaluationError(
            f"{context}.configuration must have the exact retriever fields"
        )
    views = configuration["document_text_views"]
    weights = configuration["rrf_weights"]
    if (
        not isinstance(views, list)
        or not views
        or len(set(views)) != len(views)
        or any(
            view not in {"bare", "timestamp", "timestamp-previous", "centered"}
            for view in views
        )
        or not isinstance(weights, list)
        or len(weights) != len(views)
        or any(
            not isinstance(weight, int)
            or isinstance(weight, bool)
            or not 1 <= weight <= 1_000
            for weight in weights
        )
        or not isinstance(configuration["analyzer_english_stop"], bool)
        or not isinstance(configuration["analyzer_english_stem"], bool)
        or (
            configuration["bm25_k1_micros"] is None
            and configuration["bm25_b_micros"] is not None
        )
        or (
            configuration["bm25_k1_micros"] is not None
            and configuration["bm25_b_micros"] is None
        )
        or any(
            value is not None
            and (
                not isinstance(value, int)
                or isinstance(value, bool)
                or value < 0
            )
            for value in (
                configuration["bm25_k1_micros"],
                configuration["bm25_b_micros"],
            )
        )
        or not isinstance(configuration["candidate_limit"], int)
        or isinstance(configuration["candidate_limit"], bool)
        or not 1 <= configuration["candidate_limit"] <= 10_000
        or configuration["qrel_mode"] != "audited-v2"
    ):
        raise StatisticalEvaluationError(f"{context}.configuration is invalid")
    _validate_trace_path(run["trace"], f"{context}.trace")
    for digest_name in ("trace_sha256", "trace_protocol_sha256"):
        if (
            not isinstance(run[digest_name], str)
            or HEX64.fullmatch(run[digest_name]) is None
        ):
            raise StatisticalEvaluationError(f"{context}.{digest_name} is invalid")
    if candidate and (
        not isinstance(run["simplicity_rank"], int)
        or isinstance(run["simplicity_rank"], bool)
        or run["simplicity_rank"] < 0
    ):
        raise StatisticalEvaluationError(
            f"{context}.simplicity_rank must be a nonnegative integer"
        )
    return run


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise StatisticalEvaluationError(f"cannot read manifest: {path}") from error
    manifest = require_keys(
        parse_json(text, "candidate manifest"),
        {
            "schema",
            "slice",
            "primary_metric",
            "metric_names",
            "conversations",
            "baseline",
            "candidates",
        },
        "candidate manifest",
    )
    if manifest["schema"] != MANIFEST_SCHEMA or manifest["slice"] != "A":
        raise StatisticalEvaluationError("candidate manifest schema or slice is unsupported")

    metric_names = manifest["metric_names"]
    if (
        not isinstance(metric_names, list)
        or not metric_names
        or metric_names != sorted(metric_names)
        or len(set(metric_names)) != len(metric_names)
        or any(
            not isinstance(name, str) or METRIC_NAME.fullmatch(name) is None
            for name in metric_names
        )
    ):
        raise StatisticalEvaluationError(
            "metric_names must be a sorted unique LoCoMo metric contract"
        )
    primary = manifest["primary_metric"]
    if primary not in metric_names or not primary.startswith("evidence_recall@"):
        raise StatisticalEvaluationError(
            "primary_metric must be a declared fractional evidence-recall metric"
        )
    _validate_metric_contract(metric_names)

    conversations = manifest["conversations"]
    if not isinstance(conversations, list) or len(conversations) != CONVERSATION_COUNT:
        raise StatisticalEvaluationError("Slice A requires exactly ten conversations")
    conversation_ids: set[str] = set()
    query_ids: set[str] = set()
    source_ordinals: set[int] = set()
    for conversation_index, raw_conversation in enumerate(conversations):
        context = f"conversations[{conversation_index}]"
        conversation = require_keys(raw_conversation, {"id", "queries"}, context)
        conversation_id = _validate_identifier(conversation["id"], f"{context}.id")
        if conversation_id in conversation_ids:
            raise StatisticalEvaluationError("conversation identities are not unique")
        conversation_ids.add(conversation_id)
        queries = conversation["queries"]
        if not isinstance(queries, list) or not queries:
            raise StatisticalEvaluationError(f"{context}.queries must be nonempty")
        local_ordinals: set[int] = set()
        for query_index, raw_query in enumerate(queries):
            query_context = f"{context}.queries[{query_index}]"
            query = require_keys(
                raw_query,
                {"id", "source_ordinal", "segment", "expected_targets"},
                query_context,
            )
            query_id = query["id"]
            match = re.fullmatch(re.escape(conversation_id) + r":(0|[1-9][0-9]*)", str(query_id))
            if (
                not isinstance(query_id, str)
                or match is None
                or query_id in query_ids
                or int(match.group(1)) in local_ordinals
            ):
                raise StatisticalEvaluationError(
                    f"{query_context}.id does not belong exactly to its conversation"
                )
            source_ordinal = query["source_ordinal"]
            if (
                not isinstance(source_ordinal, int)
                or isinstance(source_ordinal, bool)
                or source_ordinal < 0
                or source_ordinal in source_ordinals
            ):
                raise StatisticalEvaluationError(
                    f"{query_context}.source_ordinal is invalid or duplicated"
                )
            if query["segment"] not in {"1", "2", "3", "4", "5"}:
                raise StatisticalEvaluationError(
                    f"{query_context}.segment is not a LoCoMo category"
                )
            targets = query["expected_targets"]
            if (
                not isinstance(targets, list)
                or not targets
                or any(
                    not isinstance(target, str)
                    or not target.startswith(f"{conversation_id}/")
                    for target in targets
                )
            ):
                raise StatisticalEvaluationError(
                    f"{query_context}.expected_targets are invalid or cross-conversation"
                )
            query_ids.add(query_id)
            source_ordinals.add(source_ordinal)
            local_ordinals.add(int(match.group(1)))

    baseline = _validate_run(manifest["baseline"], "baseline", candidate=False)
    candidates = manifest["candidates"]
    if not isinstance(candidates, list) or len(candidates) < 2:
        raise StatisticalEvaluationError("at least two frozen candidates are required")
    run_ids = {baseline["id"]}
    trace_paths = {baseline["trace"]}
    simplicity_ranks: set[int] = set()
    configurations = {canonical_digest(baseline["configuration"])}
    common_metric_configuration = {
        "candidate_limit": baseline["configuration"]["candidate_limit"],
        "qrel_mode": baseline["configuration"]["qrel_mode"],
    }
    for index, raw_candidate in enumerate(candidates):
        candidate = _validate_run(raw_candidate, f"candidates[{index}]", candidate=True)
        if candidate["id"] in run_ids:
            raise StatisticalEvaluationError("run identities are not unique")
        if candidate["trace"] in trace_paths:
            raise StatisticalEvaluationError("each run must bind a distinct trace")
        if candidate["simplicity_rank"] in simplicity_ranks:
            raise StatisticalEvaluationError("candidate simplicity ranks must be unique")
        if any(
            candidate["configuration"][key] != value
            for key, value in common_metric_configuration.items()
        ):
            raise StatisticalEvaluationError(
                "all runs must share candidate_limit and qrel_mode"
            )
        configuration_digest = canonical_digest(candidate["configuration"])
        if configuration_digest in configurations:
            raise StatisticalEvaluationError("candidate configurations must be unique")
        run_ids.add(candidate["id"])
        trace_paths.add(candidate["trace"])
        simplicity_ranks.add(candidate["simplicity_rank"])
        configurations.add(configuration_digest)
    if simplicity_ranks != set(range(len(candidates))):
        raise StatisticalEvaluationError(
            "candidate simplicity ranks must be the contiguous total order 0..N-1"
        )
    return manifest


def _validate_metric_contract(metric_names: list[str]) -> None:
    names = set(metric_names)
    if "mrr@50" not in names:
        raise StatisticalEvaluationError("LoCoMo metric contract must include mrr@50")
    cutoffs: defaultdict[str, set[int]] = defaultdict(set)
    for name in names - {"mrr@50"}:
        family, raw_cutoff = name.split("@", 1)
        cutoffs[family].add(int(raw_cutoff))
    expected_families = {"evidence_recall", "recall_any", "recall_all", "ndcg"}
    if set(cutoffs) != expected_families:
        raise StatisticalEvaluationError("LoCoMo metric families are incomplete")
    values = list(cutoffs.values())
    if any(value != values[0] for value in values[1:]):
        raise StatisticalEvaluationError("LoCoMo metric cutoffs differ by family")


def _number(value: Any, context: str) -> float:
    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(value)
        or not 0.0 <= value <= 1.0
    ):
        raise StatisticalEvaluationError(f"{context} must be a finite value within [0, 1]")
    return float(value)


def _validate_metric_values(metrics: Any, metric_names: list[str], context: str) -> dict[str, float]:
    if not isinstance(metrics, dict) or set(metrics) != set(metric_names):
        raise StatisticalEvaluationError(f"{context} does not match the exact metric contract")
    normalized = {name: _number(metrics[name], f"{context}.{name}") for name in metric_names}
    cutoffs = sorted(
        int(name.split("@", 1)[1])
        for name in metric_names
        if name.startswith("evidence_recall@")
    )
    previous = {"evidence_recall": 0.0, "recall_any": 0.0, "recall_all": 0.0}
    for cutoff in cutoffs:
        evidence = normalized[f"evidence_recall@{cutoff}"]
        recall_any = normalized[f"recall_any@{cutoff}"]
        recall_all = normalized[f"recall_all@{cutoff}"]
        ndcg = normalized[f"ndcg@{cutoff}"]
        if recall_any not in {0.0, 1.0} or recall_all not in {0.0, 1.0}:
            raise StatisticalEvaluationError(f"{context} boolean recall metrics are not exact")
        if not recall_all <= evidence <= recall_any:
            raise StatisticalEvaluationError(f"{context} recall metrics are inconsistent")
        if (ndcg > 0.0) != (recall_any == 1.0):
            raise StatisticalEvaluationError(f"{context} NDCG and recall_any disagree")
        for family, value in (
            ("evidence_recall", evidence),
            ("recall_any", recall_any),
            ("recall_all", recall_all),
        ):
            if value < previous[family]:
                raise StatisticalEvaluationError(
                    f"{context} {family} decreases at a larger cutoff"
                )
            previous[family] = value
    if 50 in cutoffs and (normalized["mrr@50"] > 0.0) != (
        normalized["recall_any@50"] == 1.0
    ):
        raise StatisticalEvaluationError(f"{context} mrr@50 and recall_any disagree")
    return normalized


def _query_roster(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    roster = []
    for conversation in manifest["conversations"]:
        for query in conversation["queries"]:
            roster.append({**query, "conversation_id": conversation["id"]})
    return sorted(roster, key=lambda query: query["source_ordinal"])


def _trace_metric_names(cutoffs: list[int]) -> list[str]:
    return sorted(
        [
            f"{family}@{cutoff}"
            for cutoff in cutoffs
            for family in ("evidence_recall", "recall_any", "recall_all", "ndcg")
        ]
        + ["mrr@50"]
    )


def _ranking_sha256(query_id: str, ranking: list[str]) -> str:
    return canonical_digest({"query_id": query_id, "logical_ranking": ranking})


def _binary_ndcg(ranking: list[str], targets: list[str], cutoff: int) -> float:
    target_set = set(targets)
    gains = [1 if item in target_set else 0 for item in ranking[:cutoff]]
    dcg = math.fsum(
        gain / math.log2(position + 2) for position, gain in enumerate(gains)
    )
    ideal = [1] * min(len(target_set), cutoff)
    idcg = math.fsum(
        gain / math.log2(position + 2) for position, gain in enumerate(ideal)
    )
    return dcg / idcg if idcg else 0.0


def _recompute_metrics(
    ranking: list[str], targets: list[str], cutoffs: list[int]
) -> dict[str, float]:
    target_set = set(targets)
    metrics = {}
    for cutoff in cutoffs:
        recalled = set(ranking[:cutoff])
        hits = sum(1 for target in targets if target in recalled)
        metrics[f"evidence_recall@{cutoff}"] = hits / len(targets)
        metrics[f"recall_any@{cutoff}"] = float(bool(target_set.intersection(recalled)))
        metrics[f"recall_all@{cutoff}"] = float(target_set.issubset(recalled))
        metrics[f"ndcg@{cutoff}"] = _binary_ndcg(ranking, targets, cutoff)
    metrics["mrr@50"] = next(
        (
            1.0 / rank
            for rank, item in enumerate(ranking[:50], start=1)
            if item in target_set
        ),
        0.0,
    )
    return metrics


def _nearest_rank_integer(values: list[int], percentile: float) -> int:
    ordered = sorted(values)
    rank = max(1, math.ceil(percentile / 100 * len(ordered)))
    return ordered[rank - 1]


def _latency_summary(values: list[int]) -> dict[str, Any]:
    total = sum(values)
    if total <= 0:
        raise StatisticalEvaluationError("latency samples must have a positive total")
    return {
        "samples": len(values),
        "p50_nanos": _nearest_rank_integer(values, 50),
        "p95_nanos": _nearest_rank_integer(values, 95),
        "p99_nanos": _nearest_rank_integer(values, 99),
        "p999_nanos": _nearest_rank_integer(values, 99.9),
        "maximum_nanos": max(values),
        "mean_nanos": round(total / len(values), 3),
        "total_nanos": total,
        "queries_per_second": round(len(values) * 1_000_000_000 / total, 3),
    }


def _validate_trace_metadata(
    value: Any,
    metric_names: list[str],
    expected_protocol_sha256: str,
    expected_source_ordinals: Sequence[int],
    expected_configuration: Mapping[str, Any],
    context: str,
) -> dict[str, Any]:
    metadata = require_keys(
        value,
        {
            "record_type",
            "schema",
            "query_record_schema",
            "digest_canonicalization",
            "protocol",
            "protocol_sha256",
        },
        context,
    )
    if (
        metadata["record_type"] != "metadata"
        or metadata["schema"] != TRACE_SCHEMA
        or metadata["query_record_schema"] != TRACE_QUERY_SCHEMA
        or metadata["digest_canonicalization"] != TRACE_DIGEST_CANONICALIZATION
        or metadata["protocol_sha256"] != expected_protocol_sha256
        or metadata["protocol_sha256"] != canonical_digest(metadata["protocol"])
    ):
        raise StatisticalEvaluationError(f"{context} is incompatible or has a stale digest")
    protocol = require_keys(
        metadata["protocol"],
        {
            "schema",
            "benchmark",
            "dataset_sha256",
            "dataset_stats",
            "source_ordinal",
            "query_population_total",
            "query_population_evaluated",
            "eligible_source_ordinals",
            "candidate_limit",
            "cutoffs",
            "metric_names",
            "warmup_per_query",
            "timed_repetitions",
            "logical_ranking_scope",
            "ranking_order",
            "ranking_identity",
            "ranking_digest",
            "metric_contributions",
            "qrel_mode",
            "qrel_semantics",
            "latency_clock",
            "latency_unit",
            "latency_scope",
            "omitted_dataset_fields",
            "benchmark_protocol",
            "execution_context",
        },
        f"{context}.protocol",
    )
    cutoffs = protocol["cutoffs"]
    candidate_limit = protocol["candidate_limit"]
    expected_query_count = len(expected_source_ordinals)
    if (
        protocol["schema"] != TRACE_PROTOCOL_SCHEMA
        or protocol["benchmark"] != "locomo"
        or protocol["dataset_sha256"] != LOCOMO_DATASET_SHA256
        or not isinstance(protocol["dataset_stats"], dict)
        or protocol["source_ordinal"]
        != "zero-based position in the pinned dataset query array"
        or not isinstance(protocol["query_population_total"], int)
        or isinstance(protocol["query_population_total"], bool)
        or protocol["query_population_total"] < expected_query_count
        or not isinstance(protocol["query_population_evaluated"], int)
        or isinstance(protocol["query_population_evaluated"], bool)
        or protocol["query_population_evaluated"] != expected_query_count
        or not isinstance(protocol["eligible_source_ordinals"], list)
        or protocol["eligible_source_ordinals"]
        != sorted(expected_source_ordinals)
        or not isinstance(candidate_limit, int)
        or isinstance(candidate_limit, bool)
        or candidate_limit < 1
        or not isinstance(cutoffs, list)
        or not cutoffs
        or cutoffs != sorted(set(cutoffs))
        or any(
            not isinstance(cutoff, int)
            or isinstance(cutoff, bool)
            or not 1 <= cutoff <= candidate_limit
            for cutoff in cutoffs
        )
        or _trace_metric_names(cutoffs) != metric_names
        or protocol["metric_names"] != metric_names
        or protocol["warmup_per_query"] != 1
        or not isinstance(protocol["timed_repetitions"], int)
        or isinstance(protocol["timed_repetitions"], bool)
        or protocol["timed_repetitions"] < 1
        or protocol["logical_ranking_scope"]
        != "all returned hits up to candidate_limit"
        or protocol["ranking_digest"] != TRACE_DIGEST_CANONICALIZATION
        or protocol["metric_contributions"]
        != "unrounded per-query values; aggregate is arithmetic mean"
        or protocol["qrel_mode"] != "audited-v2"
        or protocol["qrel_semantics"]
        != "stable first-occurrence deduplication before scoring"
        or protocol["latency_clock"] != "time.perf_counter_ns"
        or protocol["latency_unit"] != "nanoseconds"
        or protocol["omitted_dataset_fields"]
        != ["answer_text", "query_text", "document_text"]
        or not isinstance(protocol["ranking_order"], str)
        or not protocol["ranking_order"]
        or not isinstance(protocol["ranking_identity"], str)
        or not protocol["ranking_identity"]
        or not isinstance(protocol["latency_scope"], str)
        or not protocol["latency_scope"]
        or not isinstance(protocol["benchmark_protocol"], dict)
        or not isinstance(protocol["execution_context"], dict)
    ):
        raise StatisticalEvaluationError(f"{context}.protocol is not the Slice A contract")
    _reject_timestamp_keys(protocol, f"{context}.protocol")
    _reject_absolute_paths(protocol, f"{context}.protocol")
    benchmark_protocol = protocol["benchmark_protocol"]
    execution_context = protocol["execution_context"]
    configuration = {
        "document_text_views": benchmark_protocol.get("document_text_views"),
        "rrf_weights": benchmark_protocol.get("rrf_weights"),
        "analyzer_english_stop": execution_context.get("analyzer_english_stop"),
        "analyzer_english_stem": execution_context.get("analyzer_english_stem"),
        "bm25_k1_micros": execution_context.get("bm25_k1_micros"),
        "bm25_b_micros": execution_context.get("bm25_b_micros"),
        "candidate_limit": candidate_limit,
        "qrel_mode": protocol["qrel_mode"],
    }
    if configuration != expected_configuration:
        raise StatisticalEvaluationError(
            f"{context}.protocol does not match the frozen candidate configuration"
        )
    return metadata


def load_trace(
    path: Path,
    roster: list[dict[str, Any]],
    metric_names: list[str],
    run_id: str,
    expected_protocol_sha256: str,
    expected_configuration: Mapping[str, Any],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise StatisticalEvaluationError(f"cannot read trace for {run_id}: {path}") from error
    if not lines or any(not line.strip() for line in lines):
        raise StatisticalEvaluationError(f"trace for {run_id} is empty or contains blank records")
    if len(lines) != len(roster) + 1:
        raise StatisticalEvaluationError(
            f"trace for {run_id} has {len(lines) - 1} queries; expected {len(roster)}"
        )

    metadata = _validate_trace_metadata(
        parse_json(lines[0], f"trace {run_id} metadata"),
        metric_names,
        expected_protocol_sha256,
        [query["source_ordinal"] for query in roster],
        expected_configuration,
        f"trace {run_id} metadata",
    )
    protocol = metadata["protocol"]

    rows = []
    for index, (line, expected) in enumerate(zip(lines[1:], roster)):
        context = f"trace {run_id} line {index + 2}"
        record = require_keys(
            parse_json(line, context),
            {"record_type", "schema", "protocol_sha256", "source_ordinal", "result"},
            context,
        )
        if (
            record["record_type"] != "query"
            or record["schema"] != TRACE_QUERY_SCHEMA
            or record["protocol_sha256"] != metadata["protocol_sha256"]
        ):
            raise StatisticalEvaluationError(f"{context} has an incompatible envelope")
        if record["source_ordinal"] != expected["source_ordinal"]:
            raise StatisticalEvaluationError(f"{context} source ordinal differs from the roster")
        result = require_keys(
            record["result"],
            {
                "id",
                "sample_id",
                "conversation_id",
                "segment",
                "expected_targets",
                "scored_targets",
                "logical_ranking",
                "metric_contributions",
                "latency",
                "changed_rankings",
                "ranking_sha256",
            },
            f"{context}.result",
        )
        if (
            result["id"] != expected["id"]
            or result["sample_id"] != expected["conversation_id"]
            or result["conversation_id"] != expected["conversation_id"]
            or result["segment"] != expected["segment"]
            or result["expected_targets"] != expected["expected_targets"]
            or result["scored_targets"] != list(dict.fromkeys(expected["expected_targets"]))
        ):
            raise StatisticalEvaluationError(f"{context} query identity or segment differs")
        ranking = result["logical_ranking"]
        if (
            not isinstance(ranking, list)
            or len(ranking) > protocol["candidate_limit"]
            or len(ranking) != len(set(ranking))
            or any(not isinstance(item, str) or not item for item in ranking)
            or result["ranking_sha256"] != _ranking_sha256(result["id"], ranking)
        ):
            raise StatisticalEvaluationError(f"{context} logical ranking or digest is invalid")
        metrics = _validate_metric_values(
            result["metric_contributions"], metric_names, f"{context}.metric_contributions"
        )
        recomputed = _recompute_metrics(ranking, result["scored_targets"], protocol["cutoffs"])
        if metrics != recomputed:
            raise StatisticalEvaluationError(f"{context} metric contributions differ from ranking")
        latency = require_keys(
            result["latency"],
            {"clock", "unit", "warmup_excluded", "samples", "summary"},
            f"{context}.latency",
        )
        latencies = latency["samples"]
        if (
            not isinstance(latencies, list)
            or len(latencies) != protocol["timed_repetitions"]
            or any(
                not isinstance(value, int) or isinstance(value, bool) or value < 0
                for value in latencies
            )
            or latency["clock"] != protocol["latency_clock"]
            or latency["unit"] != protocol["latency_unit"]
            or latency["warmup_excluded"] is not True
            or latency["summary"] != _latency_summary(latencies)
        ):
            raise StatisticalEvaluationError(f"{context} latencies are invalid")
        if result["changed_rankings"] != 0:
            raise StatisticalEvaluationError(f"{context} records nondeterministic repeated rankings")
        ranking_digest = result["ranking_sha256"]
        if not isinstance(ranking_digest, str) or HEX64.fullmatch(ranking_digest) is None:
            raise StatisticalEvaluationError(f"{context} ranking digest is invalid")
        rows.append(
            {
                "source_ordinal": expected["source_ordinal"],
                "id": expected["id"],
                "conversation_id": expected["conversation_id"],
                "segment": expected["segment"],
                "metrics": metrics,
                "ranking_sha256": ranking_digest,
            }
        )
    return rows, metadata


def _mean(values: Sequence[float]) -> float:
    if not values:
        raise StatisticalEvaluationError("cannot average an empty sample")
    return math.fsum(values) / len(values)


def _standard_error(values: Sequence[float]) -> float:
    if len(values) < 2:
        raise StatisticalEvaluationError("standard error requires at least two folds")
    mean = _mean(values)
    variance = math.fsum((value - mean) ** 2 for value in values) / (len(values) - 1)
    return math.sqrt(variance / len(values))


def _rounded(value: float) -> float:
    return round(value, 12)


def select_one_standard_error(
    scores: Mapping[str, Sequence[float]], simplicity_ranks: Mapping[str, int]
) -> dict[str, Any]:
    if set(scores) != set(simplicity_ranks) or len(scores) < 2:
        raise StatisticalEvaluationError("selection scores and simplicity ranks differ")
    summaries = []
    fold_count: int | None = None
    for candidate_id in sorted(scores):
        values = list(scores[candidate_id])
        if fold_count is None:
            fold_count = len(values)
        if len(values) != fold_count or len(values) < 2 or any(
            not math.isfinite(value) for value in values
        ):
            raise StatisticalEvaluationError("inner-fold candidate scores are invalid")
        summaries.append(
            {
                "candidate_id": candidate_id,
                "simplicity_rank": simplicity_ranks[candidate_id],
                "mean": _mean(values),
                "standard_error": _standard_error(values),
            }
        )
    best = min(
        summaries,
        key=lambda item: (-item["mean"], item["simplicity_rank"], item["candidate_id"]),
    )
    threshold = best["mean"] - best["standard_error"]
    eligible = [item for item in summaries if item["mean"] >= threshold]
    selected = min(
        eligible, key=lambda item: (item["simplicity_rank"], item["candidate_id"])
    )
    return {
        "best_candidate_id": best["candidate_id"],
        "best_mean": _rounded(best["mean"]),
        "best_standard_error": _rounded(best["standard_error"]),
        "eligibility_threshold": _rounded(threshold),
        "eligible_candidate_ids": sorted(item["candidate_id"] for item in eligible),
        "selected_candidate_id": selected["candidate_id"],
        "candidates": [
            {
                **{key: value for key, value in item.items() if key not in {"mean", "standard_error"}},
                "mean": _rounded(item["mean"]),
                "standard_error": _rounded(item["standard_error"]),
            }
            for item in summaries
        ],
    }


def aggregate_metrics(rows: Sequence[dict[str, Any]], metric_names: Sequence[str]) -> dict[str, Any]:
    if not rows:
        raise StatisticalEvaluationError("OOF metric denominator is empty")
    grouped: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[row["conversation_id"]].append(row)
    by_conversation: dict[str, Any] = {}
    for conversation_id in sorted(grouped):
        conversation_rows = grouped[conversation_id]
        by_conversation[conversation_id] = {
            "query_count": len(conversation_rows),
            "metrics": {
                name: _rounded(_mean([row["metrics"][name] for row in conversation_rows]))
                for name in metric_names
            },
        }
    conversation_macro = {
        name: _rounded(
            _mean([entry["metrics"][name] for entry in by_conversation.values()])
        )
        for name in metric_names
    }
    question_micro = {
        name: _rounded(_mean([row["metrics"][name] for row in rows]))
        for name in metric_names
    }
    return {
        "conversation_count": len(grouped),
        "query_count": len(rows),
        "conversation_macro": conversation_macro,
        "question_micro": question_micro,
        "by_conversation": by_conversation,
    }


def exact_paired_sign_flip(
    candidate: Mapping[str, float], baseline: Mapping[str, float]
) -> dict[str, Any]:
    if set(candidate) != set(baseline) or len(candidate) != CONVERSATION_COUNT:
        raise StatisticalEvaluationError("paired sign-flip requires the same ten conversations")
    conversation_ids = sorted(candidate)
    values = [*candidate.values(), *baseline.values()]
    if any(
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(value)
        for value in values
    ):
        raise StatisticalEvaluationError("paired sign-flip values must be finite numbers")
    differences = [
        Fraction(str(candidate[conversation_id])) - Fraction(str(baseline[conversation_id]))
        for conversation_id in conversation_ids
    ]
    observed_sum = abs(sum(differences, Fraction()))
    assignments = 1 << len(differences)
    extreme = 0
    for mask in range(assignments):
        permuted_sum = sum(
            (difference if mask & (1 << index) else -difference)
            for index, difference in enumerate(differences)
        )
        if abs(permuted_sum) >= observed_sum:
            extreme += 1
    p_value = Fraction(extreme, assignments)
    return {
        "alternative": "two-sided",
        "statistic_mean_difference": _rounded(float(sum(differences) / len(differences))),
        "assignments": assignments,
        "extreme_assignments": extreme,
        "p_value": float(p_value),
        "p_value_fraction": str(p_value),
    }


def _nearest_rank(values: Sequence[float], probability: float) -> float:
    if not values or not 0.0 < probability <= 1.0:
        raise StatisticalEvaluationError("bootstrap percentile input is invalid")
    ordered = sorted(values)
    rank = max(1, math.ceil(probability * len(ordered)))
    return ordered[rank - 1]


def conversation_cluster_bootstrap(
    candidate: Mapping[str, Sequence[float]],
    baseline: Mapping[str, Sequence[float]],
    *,
    seed: int = BOOTSTRAP_SEED,
    replicates: int = BOOTSTRAP_REPLICATES,
    confidence: float = BOOTSTRAP_CONFIDENCE,
) -> dict[str, Any]:
    if (
        set(candidate) != set(baseline)
        or len(candidate) != CONVERSATION_COUNT
        or not isinstance(seed, int)
        or isinstance(seed, bool)
        or not isinstance(replicates, int)
        or isinstance(replicates, bool)
        or replicates < 2
        or not 0.0 < confidence < 1.0
    ):
        raise StatisticalEvaluationError("conversation bootstrap inputs are invalid")
    conversation_ids = sorted(candidate)
    cluster_differences: dict[str, list[float]] = {}
    for conversation_id in conversation_ids:
        candidate_values = list(candidate[conversation_id])
        baseline_values = list(baseline[conversation_id])
        if (
            not candidate_values
            or len(candidate_values) != len(baseline_values)
            or any(not math.isfinite(value) for value in candidate_values + baseline_values)
        ):
            raise StatisticalEvaluationError("paired bootstrap clusters are not aligned")
        cluster_differences[conversation_id] = [
            left - right for left, right in zip(candidate_values, baseline_values)
        ]

    macro_samples = []
    micro_samples = []
    state = seed & ((1 << 64) - 1)

    def sample_index(bound: int) -> int:
        nonlocal state
        # SplitMix64 is specified here instead of relying on implementation-specific
        # random-module sampling behavior. Rejection keeps bounded draws unbiased.
        limit = (1 << 64) - ((1 << 64) % bound)
        while True:
            state = (state + 0x9E3779B97F4A7C15) & ((1 << 64) - 1)
            value = state
            value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & ((1 << 64) - 1)
            value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & ((1 << 64) - 1)
            value ^= value >> 31
            if value < limit:
                return value % bound

    for _ in range(replicates):
        sampled = [conversation_ids[sample_index(len(conversation_ids))] for _ in conversation_ids]
        macro_samples.append(
            _mean([_mean(cluster_differences[conversation_id]) for conversation_id in sampled])
        )
        micro_values = [
            value
            for conversation_id in sampled
            for value in cluster_differences[conversation_id]
        ]
        micro_samples.append(_mean(micro_values))

    alpha = 1.0 - confidence

    def interval(samples: list[float], estimate: float) -> dict[str, Any]:
        return {
            "estimate": _rounded(estimate),
            "lower": _rounded(_nearest_rank(samples, alpha / 2.0)),
            "upper": _rounded(_nearest_rank(samples, 1.0 - alpha / 2.0)),
        }

    all_differences = [
        value for conversation_id in conversation_ids for value in cluster_differences[conversation_id]
    ]
    return {
        "seed": seed,
        "replicates": replicates,
        "confidence": confidence,
        "interval_method": "nearest-rank-percentile",
        "generator": "SplitMix64 with rejection sampling",
        "conversation_macro_difference": interval(
            macro_samples,
            _mean([_mean(cluster_differences[key]) for key in conversation_ids]),
        ),
        "question_micro_difference": interval(micro_samples, _mean(all_differences)),
    }


def holm_adjust(p_values: Mapping[str, float]) -> dict[str, float]:
    if not p_values or any(
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(value)
        or not 0.0 <= value <= 1.0
        for value in p_values.values()
    ):
        raise StatisticalEvaluationError("Holm p-values must be finite within [0, 1]")
    ordered = sorted(p_values.items(), key=lambda item: (item[1], item[0]))
    adjusted: dict[str, float] = {}
    running = 0.0
    count = len(ordered)
    for index, (name, value) in enumerate(ordered):
        running = max(running, min(1.0, (count - index) * float(value)))
        adjusted[name] = _rounded(running)
    return {name: adjusted[name] for name in sorted(adjusted)}


def _rows_by_conversation(rows: Sequence[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    grouped: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[row["conversation_id"]].append(row)
    return dict(grouped)


def evaluate_manifest(manifest_path: Path) -> dict[str, Any]:
    manifest_path = manifest_path.resolve()
    manifest = load_manifest(manifest_path)
    roster = _query_roster(manifest)
    metric_names = manifest["metric_names"]
    runs = [manifest["baseline"], *manifest["candidates"]]
    traces: dict[str, list[dict[str, Any]]] = {}
    trace_sources = []
    trace_identity: dict[str, Any] | None = None
    for run in sorted(runs, key=lambda item: item["id"]):
        trace_path = manifest_path.parent / run["trace"]
        observed_digest = file_digest(trace_path)
        if observed_digest != run["trace_sha256"]:
            raise StatisticalEvaluationError(
                f"trace digest differs from the frozen manifest for {run['id']}"
            )
        rows, metadata = load_trace(
            trace_path,
            roster,
            metric_names,
            run["id"],
            run["trace_protocol_sha256"],
            run["configuration"],
        )
        traces[run["id"]] = rows
        current_trace_identity = {
            key: metadata["protocol"][key]
            for key in (
                "dataset_sha256",
                "source_ordinal",
                "query_population_total",
                "query_population_evaluated",
                "eligible_source_ordinals",
                "candidate_limit",
                "cutoffs",
                "metric_names",
                "qrel_mode",
                "qrel_semantics",
            )
        }
        if trace_identity is None:
            trace_identity = current_trace_identity
        elif current_trace_identity != trace_identity:
            raise StatisticalEvaluationError(
                "candidate traces do not share one dataset and scoring protocol"
            )
        trace_sources.append(
            {
                "run_id": run["id"],
                "trace_file_sha256": observed_digest,
                "trace_protocol_sha256": run["trace_protocol_sha256"],
            }
        )

    primary_metric = manifest["primary_metric"]
    candidate_by_id = {candidate["id"]: candidate for candidate in manifest["candidates"]}
    candidate_groups = {
        candidate_id: _rows_by_conversation(traces[candidate_id])
        for candidate_id in sorted(candidate_by_id)
    }
    conversation_ids = sorted(conversation["id"] for conversation in manifest["conversations"])
    simplicity_ranks = {
        candidate_id: candidate_by_id[candidate_id]["simplicity_rank"]
        for candidate_id in candidate_by_id
    }
    outer_folds = []
    oof_predictions = []
    for held_out in conversation_ids:
        training = [conversation_id for conversation_id in conversation_ids if conversation_id != held_out]
        inner_scores = {
            candidate_id: [
                _mean(
                    [
                        row["metrics"][primary_metric]
                        for row in candidate_groups[candidate_id][conversation_id]
                    ]
                )
                for conversation_id in training
            ]
            for candidate_id in sorted(candidate_by_id)
        }
        selection = select_one_standard_error(inner_scores, simplicity_ranks)
        selection["inner_validation_scores"] = {
            candidate_id: {
                conversation_id: _rounded(score)
                for conversation_id, score in zip(training, inner_scores[candidate_id])
            }
            for candidate_id in sorted(inner_scores)
        }
        selected_id = selection["selected_candidate_id"]
        selected_candidate = candidate_by_id[selected_id]
        outer_folds.append(
            {
                "held_out_conversation_id": held_out,
                "inner_training_conversation_ids": training,
                "selected_candidate_id": selected_id,
                "selected_configuration": selected_candidate["configuration"],
                "inner_selection": selection,
            }
        )
        for row in candidate_groups[selected_id][held_out]:
            oof_predictions.append(
                {
                    "source_ordinal": row["source_ordinal"],
                    "id": row["id"],
                    "conversation_id": row["conversation_id"],
                    "segment": row["segment"],
                    "selected_candidate_id": selected_id,
                    "metrics": row["metrics"],
                    "ranking_sha256": row["ranking_sha256"],
                }
            )
    oof_predictions.sort(key=lambda row: row["source_ordinal"])
    baseline_rows = traces[manifest["baseline"]["id"]]
    selected_aggregate = aggregate_metrics(oof_predictions, metric_names)
    baseline_aggregate = aggregate_metrics(baseline_rows, metric_names)

    comparisons: dict[str, Any] = {}
    unadjusted: dict[str, float] = {}
    selected_groups = _rows_by_conversation(oof_predictions)
    baseline_groups = _rows_by_conversation(baseline_rows)
    for metric in metric_names:
        selected_conversation = {
            conversation_id: _mean(
                [row["metrics"][metric] for row in selected_groups[conversation_id]]
            )
            for conversation_id in conversation_ids
        }
        baseline_conversation = {
            conversation_id: _mean(
                [row["metrics"][metric] for row in baseline_groups[conversation_id]]
            )
            for conversation_id in conversation_ids
        }
        sign_flip = exact_paired_sign_flip(selected_conversation, baseline_conversation)
        bootstrap = conversation_cluster_bootstrap(
            {
                conversation_id: [
                    row["metrics"][metric] for row in selected_groups[conversation_id]
                ]
                for conversation_id in conversation_ids
            },
            {
                conversation_id: [
                    row["metrics"][metric] for row in baseline_groups[conversation_id]
                ]
                for conversation_id in conversation_ids
            },
        )
        comparisons[metric] = {
            "conversation_mean_differences": {
                conversation_id: _rounded(
                    selected_conversation[conversation_id]
                    - baseline_conversation[conversation_id]
                )
                for conversation_id in conversation_ids
            },
            "exact_paired_sign_flip": sign_flip,
            "conversation_cluster_bootstrap": bootstrap,
        }
        unadjusted[metric] = sign_flip["p_value"]
    adjusted = holm_adjust(unadjusted)
    for metric in metric_names:
        comparisons[metric]["holm_adjusted_p_value"] = adjusted[metric]

    results = {
        "outer_folds": outer_folds,
        "oof_predictions": oof_predictions,
        "metrics": {
            "selected_oof": selected_aggregate,
            "frozen_baseline": baseline_aggregate,
        },
        "comparisons_to_baseline": comparisons,
        "holm_family": {
            "method": "Holm step-down family-wise error control",
            "hypotheses": metric_names,
            "adjusted_p_values": adjusted,
        },
    }
    protocol = {
        "slice": "A",
        "conversation_count": CONVERSATION_COUNT,
        "query_count": len(roster),
        "metric_names": metric_names,
        "primary_metric": primary_metric,
        "metric_direction": "maximize",
        "query_roster_sha256": canonical_digest(roster),
        "outer_validation": "leave-one-conversation-out",
        "inner_selection": "leave-one-conversation-out over the nine outer-training conversations",
        "candidate_fitting": "none; every candidate is frozen before outer evaluation",
        "outer_fold_order": "conversation-id lexicographic",
        "one_standard_error_rule": {
            "threshold": "best inner conversation-macro mean minus its sample standard error",
            "choice": "lowest frozen simplicity rank meeting the threshold",
        },
        "aggregation": {
            "conversation_macro": "equal weight per conversation after within-conversation query mean",
            "question_micro": "equal weight per evaluated query",
        },
        "paired_test": {
            "method": "exact two-sided paired conversation sign-flip",
            "assignments": 1 << CONVERSATION_COUNT,
        },
        "cluster_bootstrap": {
            "sampling_unit": "conversation with replacement",
            "seed": BOOTSTRAP_SEED,
            "replicates": BOOTSTRAP_REPLICATES,
            "confidence": BOOTSTRAP_CONFIDENCE,
            "interval_method": "nearest-rank percentile",
            "generator": "SplitMix64 with rejection sampling",
        },
        "multiplicity": "Holm correction across all declared metrics",
        "baseline": {
            "id": manifest["baseline"]["id"],
            "configuration": manifest["baseline"]["configuration"],
        },
        "candidates": [
            {
                "id": candidate["id"],
                "simplicity_rank": candidate["simplicity_rank"],
                "configuration": candidate["configuration"],
            }
            for candidate in sorted(manifest["candidates"], key=lambda item: item["id"])
        ],
    }
    source = {
        "candidate_manifest_file_sha256": file_digest(manifest_path),
        "evaluator_file_sha256": file_digest(Path(__file__).resolve()),
        "traces": trace_sources,
    }
    identity = {
        "source_sha256": canonical_digest(source),
        "protocol_sha256": canonical_digest(protocol),
        "result_sha256": canonical_digest(results),
    }
    identity["evaluation_sha256"] = canonical_digest(
        {
            "schema": RESULT_SCHEMA,
            "slice": "A",
            **identity,
        }
    )
    return {
        "schema": RESULT_SCHEMA,
        "status": "passed",
        "evidence_class": "local-diagnostic",
        "publication": {
            "authorized": False,
            "reason": "local diagnostic only; publication requires explicit review and approval",
        },
        "source": source,
        "protocol": protocol,
        "results": results,
        "identity": identity,
        "claims": [],
        "closure_declared": False,
    }


def build_candidate_manifest(
    baseline_trace: Path,
    candidate_traces: list[Path],
    candidate_ids: list[str],
    output: Path,
) -> dict[str, Any]:
    """Builds one deterministic Slice A manifest from complete frozen traces."""
    if not candidate_traces or len(candidate_traces) != len(candidate_ids):
        raise StatisticalEvaluationError(
            "candidate trace paths and identities must be nonempty and aligned"
        )
    if len(set(candidate_ids)) != len(candidate_ids):
        raise StatisticalEvaluationError("candidate identities must be unique")

    def load_metadata(path: Path, context: str) -> dict[str, Any]:
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError) as error:
            raise StatisticalEvaluationError(f"cannot read {context}: {path}") from error
        if len(lines) < 2:
            raise StatisticalEvaluationError(f"{context} is incomplete")
        metadata = parse_json(lines[0], f"{context} metadata")
        if not isinstance(metadata, dict) or metadata.get("schema") != TRACE_SCHEMA:
            raise StatisticalEvaluationError(f"{context} metadata is unsupported")
        if metadata.get("protocol_sha256") != canonical_digest(metadata.get("protocol")):
            raise StatisticalEvaluationError(f"{context} metadata digest differs")
        return {"metadata": metadata, "lines": lines}

    loaded = [("baseline", baseline_trace, load_metadata(baseline_trace, "baseline trace"))]
    loaded.extend(
        (candidate_id, path, load_metadata(path, f"candidate trace {candidate_id}"))
        for candidate_id, path in zip(candidate_ids, candidate_traces)
    )
    baseline_protocol = loaded[0][2]["metadata"]["protocol"]
    metric_names = baseline_protocol.get("metric_names")
    if not isinstance(metric_names, list) or "evidence_recall@10" not in metric_names:
        raise StatisticalEvaluationError("trace metric contract lacks evidence_recall@10")

    configurations: dict[str, dict[str, Any]] = {}
    conversations: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
    roster_identity = None
    for run_id, path, trace in loaded:
        protocol = trace["metadata"]["protocol"]
        benchmark_protocol = protocol.get("benchmark_protocol", {})
        execution_context = protocol.get("execution_context", {})
        configuration = {
            "document_text_views": benchmark_protocol.get("document_text_views"),
            "rrf_weights": benchmark_protocol.get("rrf_weights"),
            "analyzer_english_stop": execution_context.get("analyzer_english_stop"),
            "analyzer_english_stem": execution_context.get("analyzer_english_stem"),
            "bm25_k1_micros": execution_context.get("bm25_k1_micros"),
            "bm25_b_micros": execution_context.get("bm25_b_micros"),
            "candidate_limit": protocol.get("candidate_limit"),
            "qrel_mode": protocol.get("qrel_mode"),
        }
        _validate_run(
            {
                "id": run_id,
                "configuration": configuration,
                "trace": path.name,
                "trace_sha256": file_digest(path),
                "trace_protocol_sha256": trace["metadata"]["protocol_sha256"],
                **({"simplicity_rank": 0} if run_id != "baseline" else {}),
            },
            f"trace {run_id}",
            candidate=run_id != "baseline",
        )
        configurations[run_id] = configuration

        roster = []
        for line_number, line in enumerate(trace["lines"][1:], start=2):
            record = parse_json(line, f"trace {run_id} line {line_number}")
            result = record.get("result") if isinstance(record, dict) else None
            if not isinstance(result, dict):
                raise StatisticalEvaluationError(f"trace {run_id} contains an invalid result")
            row = {
                "id": result.get("id"),
                "source_ordinal": record.get("source_ordinal"),
                "conversation_id": result.get("conversation_id"),
                "segment": result.get("segment"),
                "expected_targets": result.get("expected_targets"),
            }
            roster.append(row)
        current_identity = canonical_digest(roster)
        if roster_identity is None:
            roster_identity = current_identity
            for row in roster:
                conversations[row["conversation_id"]].append(
                    {
                        "id": row["id"],
                        "source_ordinal": row["source_ordinal"],
                        "segment": row["segment"],
                        "expected_targets": row["expected_targets"],
                    }
                )
        elif current_identity != roster_identity:
            raise StatisticalEvaluationError("candidate traces do not share one query roster")

    if len(conversations) != CONVERSATION_COUNT:
        raise StatisticalEvaluationError("Slice A requires exactly ten trace conversations")
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "slice": "A",
        "primary_metric": "evidence_recall@10",
        "metric_names": metric_names,
        "conversations": [
            {
                "id": conversation_id,
                "queries": sorted(rows, key=lambda row: row["source_ordinal"]),
            }
            for conversation_id, rows in sorted(conversations.items())
        ],
        "baseline": {
            "id": "baseline",
            "configuration": configurations["baseline"],
            "trace": baseline_trace.name,
            "trace_sha256": file_digest(baseline_trace),
            "trace_protocol_sha256": loaded[0][2]["metadata"]["protocol_sha256"],
        },
        "candidates": [
            {
                "id": candidate_id,
                "simplicity_rank": rank,
                "configuration": configurations[candidate_id],
                "trace": path.name,
                "trace_sha256": file_digest(path),
                "trace_protocol_sha256": trace["metadata"]["protocol_sha256"],
            }
            for rank, (candidate_id, path, trace) in enumerate(loaded[1:])
        ],
    }
    # Run the complete validator before publication to disk.
    output.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(manifest, indent=2, sort_keys=True, allow_nan=False) + "\n"
    output.write_text(encoded, encoding="utf-8")
    load_manifest(output)
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-baseline-trace", type=Path)
    parser.add_argument("--build-candidate-trace", type=Path, action="append")
    parser.add_argument("--build-candidate-id", action="append")
    arguments = parser.parse_args()
    building = arguments.build_baseline_trace is not None
    if building:
        if arguments.manifest is not None:
            print("error: --manifest conflicts with manifest construction", file=sys.stderr)
            return 1
        try:
            manifest = build_candidate_manifest(
                arguments.build_baseline_trace,
                arguments.build_candidate_trace or [],
                arguments.build_candidate_id or [],
                arguments.output,
            )
        except (StatisticalEvaluationError, OSError) as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        print(json.dumps(manifest, indent=2, sort_keys=True, allow_nan=False))
        return 0
    if arguments.manifest is None or not arguments.manifest.is_file():
        print("error: candidate manifest is missing", file=sys.stderr)
        return 1
    try:
        receipt = evaluate_manifest(arguments.manifest)
        encoded = json.dumps(receipt, indent=2, sort_keys=True, allow_nan=False) + "\n"
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(encoded, encoding="utf-8")
    except (StatisticalEvaluationError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
