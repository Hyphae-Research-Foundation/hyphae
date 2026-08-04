#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the reviewed native search quality-corpus registry."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-quality-corpus-v1"
FIELDS = {
    "id",
    "engine",
    "producer",
    "test",
    "minimum_documents",
    "minimum_queries",
    "metrics",
}
ENGINES = {"lexical", "ann", "hybrid"}
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
ANN_FIELDS = {
    "schema", "source_commit", "environment", "dataset_digest", "vector_count",
    "dimension", "query_count", "k", "m", "ef_construction", "ef_search",
    "max_level", "directed_edges", "build_identity", "build_duration_millis",
    "recall_at_10", "recall_at_10_floor", "recall_floor_met",
    "minimum_query_recall_at_10", "p50_query_recall_at_10",
    "mean_visited_nodes", "mean_candidate_count", "exact_latency_micros",
    "hnsw_latency_micros",
}
LATENCY_FIELDS = {"p50", "p95", "p99", "maximum"}
LEXICAL_FIELDS = {
    "schema", "source_commit", "dataset_digest", "document_count", "query_count",
    "top_k", "exact_score_order_equivalence", "reopen_equivalence",
    "query_result_digests",
}


class GateFailure(RuntimeError):
    """The quality-corpus registry is malformed or unbound."""


def _mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _producer(root: Path, value: str) -> Path:
    resolved_root = root.resolve()
    path = (resolved_root / value).resolve()
    try:
        path.relative_to(resolved_root)
    except ValueError as error:
        raise GateFailure(f"producer escapes repository root: {value}") from error
    if not path.is_file():
        raise GateFailure(f"producer is missing: {value}")
    return path


def validate_lexical_receipt(receipt: dict[str, Any]) -> dict[str, Any]:
    """Validate one bounded lexical equivalence receipt."""

    if set(receipt) != LEXICAL_FIELDS:
        raise GateFailure("unknown lexical receipt field or missing required field")
    if receipt.get("schema") != "hyphae-native-lexical-quality-v1":
        raise GateFailure("unsupported lexical receipt schema")
    source_commit = receipt.get("source_commit")
    if not isinstance(source_commit, str) or COMMIT.fullmatch(source_commit) is None:
        raise GateFailure("lexical source commit is invalid")
    dataset_digest = receipt.get("dataset_digest")
    if not isinstance(dataset_digest, str) or SHA256.fullmatch(dataset_digest) is None:
        raise GateFailure("lexical dataset digest is invalid")
    for field in ("document_count", "query_count", "top_k"):
        value = receipt.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise GateFailure(f"lexical receipt {field} requires positive scale")
    if (
        receipt.get("exact_score_order_equivalence") is not True
        or receipt.get("reopen_equivalence") is not True
    ):
        raise GateFailure("lexical receipt equivalence is not complete")
    digests = receipt.get("query_result_digests")
    if not isinstance(digests, list) or len(digests) != receipt["query_count"]:
        raise GateFailure("lexical query digest count differs from query_count")
    if len(digests) != len(set(digests)) or any(
        not isinstance(digest, str) or SHA256.fullmatch(digest) is None for digest in digests
    ):
        raise GateFailure("lexical query result digests are invalid or duplicate")
    document_count = receipt["document_count"]
    query_count = receipt["query_count"]
    return {
        "status": "passed",
        "evidence_scope": "bounded-observation",
        "document_count": document_count,
        "query_count": query_count,
        "top_k": receipt["top_k"],
        "production_scale": document_count >= 1_000_000 and query_count >= 1_000,
    }


def _finite_positive(value: object, label: str, *, allow_zero: bool = False) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise GateFailure(f"{label} must be numeric")
    numeric = float(value)
    if not math.isfinite(numeric) or numeric < 0 or (numeric == 0 and not allow_zero):
        raise GateFailure(f"{label} must be finite latency or positive metric")
    return numeric


def validate_ann_receipt(receipt: dict[str, Any]) -> dict[str, Any]:
    """Validate one bounded ANN quality observation without promoting its scale."""

    if set(receipt) != ANN_FIELDS:
        raise GateFailure("unknown ANN receipt field or missing required field")
    if receipt.get("schema") != "hyphae-native-ann-quality-v1":
        raise GateFailure("unsupported ANN receipt schema")
    source_commit = receipt.get("source_commit")
    if not isinstance(source_commit, str) or COMMIT.fullmatch(source_commit) is None:
        raise GateFailure("ANN receipt source_commit digest is invalid")
    for field in ("dataset_digest", "build_identity"):
        value = receipt.get(field)
        if not isinstance(value, str) or SHA256.fullmatch(value) is None:
            raise GateFailure(f"ANN receipt {field} digest is invalid")
    for field in ("vector_count", "dimension", "query_count", "k", "m", "ef_construction", "ef_search"):
        value = receipt.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise GateFailure(f"ANN receipt {field} requires positive scale")
    recall = _finite_positive(receipt.get("recall_at_10"), "recall_at_10")
    floor = _finite_positive(receipt.get("recall_at_10_floor"), "recall_at_10_floor")
    if recall > 1 or floor > 1 or recall < floor or receipt.get("recall_floor_met") is not True:
        raise GateFailure("ANN recall floor or recall arithmetic is invalid")
    for field in ("build_duration_millis", "mean_visited_nodes", "mean_candidate_count"):
        _finite_positive(receipt.get(field), field, allow_zero=True)
    for name in ("exact_latency_micros", "hnsw_latency_micros"):
        latency = _mapping(receipt.get(name), name)
        if set(latency) != LATENCY_FIELDS:
            raise GateFailure(f"{name} fields are invalid")
        values = [_finite_positive(latency[field], f"{name}.{field}") for field in ("p50", "p95", "p99", "maximum")]
        if values != sorted(values):
            raise GateFailure(f"{name} percentile order is invalid")
    vector_count = receipt["vector_count"]
    query_count = receipt["query_count"]
    return {
        "status": "passed",
        "evidence_scope": "bounded-observation",
        "vector_count": vector_count,
        "query_count": query_count,
        "dimension": receipt["dimension"],
        "recall_at_10": recall,
        "production_scale": vector_count >= 1_000_000 and receipt["dimension"] >= 384,
    }


def validate_quality_receipt_set(
    lexical_receipt: dict[str, Any], ann_receipt: dict[str, Any]
) -> dict[str, Any]:
    """Aggregate exact lexical and ANN receipts without inflating evidence scope."""

    lexical = validate_lexical_receipt(lexical_receipt)
    ann = validate_ann_receipt(ann_receipt)
    production_scale = lexical["production_scale"] and ann["production_scale"]
    return {
        "schema": "hyphae-native-quality-aggregate-v1",
        "status": "passed",
        "evidence_scope": "production" if production_scale else "bounded-observation",
        "production_scale": production_scale,
        "engines": ["ann", "lexical"],
        "total_observations": lexical["query_count"] + ann["query_count"],
        "lexical": lexical,
        "ann": ann,
    }


def validate_corpus(root: Path, corpus: dict[str, Any]) -> dict[str, Any]:
    """Return a content-bound registry or fail closed on any ambiguity."""

    if corpus.get("schema") != SCHEMA or set(corpus) != {"schema", "corpora"}:
        raise GateFailure("unsupported or malformed quality corpus")
    entries = corpus.get("corpora")
    if not isinstance(entries, list) or not entries:
        raise GateFailure("corpora must be a nonempty array")
    seen: set[str] = set()
    rows: list[dict[str, Any]] = []
    total_documents = 0
    total_queries = 0
    for value in entries:
        entry = _mapping(value, "corpus")
        if set(entry) != FIELDS:
            raise GateFailure("unknown corpus field or missing required field")
        corpus_id = entry.get("id")
        if not isinstance(corpus_id, str) or not corpus_id:
            raise GateFailure("corpus ID must be a nonempty string")
        if corpus_id in seen:
            raise GateFailure(f"duplicate corpus {corpus_id}")
        seen.add(corpus_id)
        engine = entry.get("engine")
        if engine not in ENGINES:
            raise GateFailure(f"unknown engine: {engine}")
        metrics = entry.get("metrics")
        if (
            not isinstance(metrics, list)
            or not metrics
            or len(metrics) != len(set(metrics))
            or any(not isinstance(metric, str) or not metric for metric in metrics)
        ):
            raise GateFailure(f"corpus {corpus_id} requires unique metrics")
        documents = entry.get("minimum_documents")
        queries = entry.get("minimum_queries")
        if not isinstance(documents, int) or not isinstance(queries, int) or documents <= 0 or queries <= 0:
            raise GateFailure(f"corpus {corpus_id} requires positive scale")
        producer_value = entry.get("producer")
        symbol = entry.get("test")
        if not isinstance(producer_value, str) or not isinstance(symbol, str) or not symbol:
            raise GateFailure(f"corpus {corpus_id} producer and test must be strings")
        producer = _producer(root, producer_value)
        producer_bytes = producer.read_bytes()
        if symbol.encode("utf-8") not in producer_bytes:
            raise GateFailure(f"test symbol {symbol} missing from producer")
        total_documents += documents
        total_queries += queries
        rows.append(
            {
                "id": corpus_id,
                "engine": engine,
                "producer": producer_value,
                "producer_sha256": hashlib.sha256(producer_bytes).hexdigest(),
                "test": symbol,
                "minimum_documents": documents,
                "minimum_queries": queries,
                "metrics": metrics,
            }
        )
    return {
        "schema": "hyphae-native-quality-corpus-audit-v1",
        "status": "passed",
        "corpus_count": len(rows),
        "engines": sorted({row["engine"] for row in rows}),
        "minimum_documents": total_documents,
        "minimum_queries": total_queries,
        "corpora": rows,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--lexical-receipt", type=Path)
    parser.add_argument("--ann-receipt", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        aggregate_inputs = (args.lexical_receipt, args.ann_receipt)
        if any(value is not None for value in aggregate_inputs):
            if not all(value is not None for value in aggregate_inputs) or args.corpus is not None:
                raise GateFailure("quality aggregation requires exactly both receipt inputs")
            result = validate_quality_receipt_set(
                json.loads(args.lexical_receipt.read_text(encoding="utf-8")),
                json.loads(args.ann_receipt.read_text(encoding="utf-8")),
            )
        elif args.corpus is not None:
            result = validate_corpus(
                args.root,
                json.loads(args.corpus.read_text(encoding="utf-8")),
            )
        else:
            raise GateFailure("provide --corpus or both quality receipts")
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"native quality corpus failed: {error}")
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
