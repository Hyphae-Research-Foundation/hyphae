#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate the reviewed native search quality-corpus registry."""

from __future__ import annotations

import argparse
import hashlib
import json
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
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        result = validate_corpus(
            args.root,
            json.loads(args.corpus.read_text(encoding="utf-8")),
        )
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
