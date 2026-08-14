#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Evaluate the checked-in bounded G4 qrels with integer-only NDCG@10."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

CORPUS_SCHEMA = "hyphae-native-g4-quality-corpus-v1"
RECEIPT_SCHEMA = "hyphae-native-g4-quality-receipt-v1"
QUERY_FIELDS = {"id", "qrels", "lexical", "hybrid", "negative"}


class QualityFailure(RuntimeError):
    """The corpus, ranking, or quality threshold is invalid."""


def _object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise QualityFailure(f"{label} must be an object")
    return value


def ndcg_at_10(
    ranking: list[str], qrels: dict[str, int], discounts: list[int], scale: int
) -> int:
    """Return deterministic NDCG@10 in ``scale`` units using checked integers."""

    if len(discounts) != 10 or any(
        not isinstance(value, int) or isinstance(value, bool) or value <= 0
        for value in discounts
    ):
        raise QualityFailure("NDCG requires ten positive integer discounts")
    if not isinstance(scale, int) or isinstance(scale, bool) or scale <= 0:
        raise QualityFailure("NDCG scale must be a positive integer")
    if len(ranking) != len(set(ranking)):
        raise QualityFailure("ranking contains duplicate document IDs")
    if not qrels or any(
        not isinstance(document, str)
        or not document
        or not isinstance(grade, int)
        or isinstance(grade, bool)
        or grade < 0
        or grade > 30
        for document, grade in qrels.items()
    ):
        raise QualityFailure("qrels require document IDs and integer grades in 0..=30")

    def gain(grade: int) -> int:
        return (1 << grade) - 1

    dcg = sum(
        gain(qrels.get(document, 0)) * discounts[index]
        for index, document in enumerate(ranking[:10])
    )
    ideal_grades = sorted(qrels.values(), reverse=True)[:10]
    ideal = sum(gain(grade) * discounts[index] for index, grade in enumerate(ideal_grades))
    if ideal == 0:
        raise QualityFailure("qrels must contain a positive relevance grade")
    return dcg * scale // ideal


def evaluate_corpus(raw: bytes) -> dict[str, Any]:
    """Validate and evaluate one canonical bounded lexical/hybrid corpus."""

    try:
        corpus = _object(json.loads(raw), "corpus")
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise QualityFailure("corpus is not UTF-8 JSON") from error
    if set(corpus) != {"schema", "metric", "documents", "queries", "thresholds"}:
        raise QualityFailure("unknown corpus field or missing required field")
    if corpus["schema"] != CORPUS_SCHEMA:
        raise QualityFailure("unsupported G4 corpus schema")
    metric = _object(corpus["metric"], "metric")
    if set(metric) != {"name", "scale", "discount_scale", "discounts"}:
        raise QualityFailure("invalid metric schema")
    if metric["name"] != "ndcg@10" or metric["discount_scale"] != 1_000_000_000:
        raise QualityFailure("unsupported integer NDCG definition")
    if (
        not isinstance(metric["scale"], int)
        or isinstance(metric["scale"], bool)
        or metric["scale"] <= 0
        or not isinstance(metric["discounts"], list)
    ):
        raise QualityFailure("invalid integer NDCG scale or discounts")
    documents = corpus["documents"]
    if (
        not isinstance(documents, list)
        or not documents
        or len(documents) > 10_000
        or len(documents) != len(set(documents))
        or any(not isinstance(value, str) or not value for value in documents)
    ):
        raise QualityFailure("documents must be a bounded unique string array")
    document_set = set(documents)
    queries = corpus["queries"]
    if not isinstance(queries, list) or not queries or len(queries) > 1_000:
        raise QualityFailure("queries must be a bounded nonempty array")
    thresholds = _object(corpus["thresholds"], "thresholds")
    if set(thresholds) != {
        "lexical_mean_ndcg_ppm", "hybrid_mean_ndcg_ppm", "negative_max_ndcg_ppm"
    }:
        raise QualityFailure("invalid threshold schema")
    if any(
        not isinstance(value, int)
        or isinstance(value, bool)
        or value < 0
        or value > metric["scale"]
        for value in thresholds.values()
    ):
        raise QualityFailure("quality thresholds must be integer metric units")

    scores: dict[str, list[int]] = {"lexical": [], "hybrid": [], "negative": []}
    query_rows: list[dict[str, Any]] = []
    seen_queries: set[str] = set()
    for value in queries:
        query = _object(value, "query")
        if set(query) != QUERY_FIELDS:
            raise QualityFailure("unknown query field or missing required field")
        query_id = query["id"]
        if not isinstance(query_id, str) or not query_id or query_id in seen_queries:
            raise QualityFailure("query IDs must be nonempty and unique")
        seen_queries.add(query_id)
        qrels = _object(query["qrels"], f"qrels for {query_id}")
        for name in ("lexical", "hybrid", "negative"):
            ranking = query[name]
            if not isinstance(ranking, list) or any(
                not isinstance(item, str) or item not in document_set for item in ranking
            ):
                raise QualityFailure(f"{name} ranking references an unknown document")
            score = ndcg_at_10(ranking, qrels, metric["discounts"], metric["scale"])
            scores[name].append(score)
        if any(document not in document_set for document in qrels):
            raise QualityFailure("qrels reference an unknown document")
        query_rows.append({
            "id": query_id,
            "lexical_ndcg_ppm": scores["lexical"][-1],
            "hybrid_ndcg_ppm": scores["hybrid"][-1],
            "negative_ndcg_ppm": scores["negative"][-1],
        })

    means = {name: sum(values) // len(values) for name, values in scores.items()}
    passed = (
        means["lexical"] >= thresholds["lexical_mean_ndcg_ppm"]
        and means["hybrid"] >= thresholds["hybrid_mean_ndcg_ppm"]
        and max(scores["negative"]) <= thresholds["negative_max_ndcg_ppm"]
        and all(negative < positive for negative, positive in zip(scores["negative"], scores["lexical"]))
        and all(negative < positive for negative, positive in zip(scores["negative"], scores["hybrid"]))
    )
    if not passed:
        raise QualityFailure("G4 lexical/hybrid quality or negative control threshold failed")
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "scope": "bounded",
        "corpus_sha256": hashlib.sha256(raw).hexdigest(),
        "document_count": len(documents),
        "query_count": len(queries),
        "metric": {"name": "ndcg@10", "scale": metric["scale"], "arithmetic": "integer-floor"},
        "lexical": {"mean_ndcg_ppm": means["lexical"], "floor_ppm": thresholds["lexical_mean_ndcg_ppm"]},
        "hybrid": {"mean_ndcg_ppm": means["hybrid"], "floor_ppm": thresholds["hybrid_mean_ndcg_ppm"]},
        "negative_control": {"maximum_ndcg_ppm": max(scores["negative"]), "ceiling_ppm": thresholds["negative_max_ndcg_ppm"]},
        "queries": query_rows,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        receipt = evaluate_corpus(args.corpus.read_bytes())
    except (OSError, QualityFailure) as error:
        print(f"native G4 quality failed: {error}")
        return 2
    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
