#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Measure a Weaviate OSS baseline under the pinned relevance protocol.

The harness drives a running Weaviate instance through its public REST and
GraphQL APIs with exactly the datasets, digests, metrics, and rounding
discipline of ``rag_eval.py``, so a head-to-head receipt compares like with
like: same corpus order, same qrels, same NDCG@k/Recall@k/MRR@k, and — when
an attested local model is supplied — the same embedding vectors on both
systems. The transport is the standard library only; nothing is installed.

The receipt additionally records operational cost probes unique to a
served, containerized engine: resident memory after ingest and after the
query phase, cold-start readiness seconds across a container restart, and a
query-stability pass that reruns every query and counts ranking changes.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from rag_eval import (  # noqa: E402
    DATASETS,
    HarnessError,
    acquire_dataset,
    embed_texts,
    host_declaration,
    load_jsonl,
    load_qrels,
    mrr_at_k,
    ndcg_at_k,
    recall_at_k,
)

RECEIPT_SCHEMA = "hyphae-weaviate-baseline-receipt-v1"
CLASS_NAME = "RagEval"
INGEST_BATCH_OBJECTS = 100
EMBED_BATCH_TEXTS = 256


def container_runtime() -> str:
    """The available container CLI: docker, or podman as its drop-in."""
    import shutil

    for name in ("docker", "podman"):
        if shutil.which(name):
            return name
    raise HarnessError("no container runtime found for the container probes")


def http_json(
    method: str, url: str, payload: dict | None = None, timeout: float = 600.0
) -> dict:
    request = urllib.request.Request(
        url,
        data=None if payload is None else json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        raise HarnessError(
            f"weaviate {method} {url} failed: {error.code} "
            f"{error.read().decode('utf-8', 'replace')[:300]}"
        ) from error
    except OSError as error:
        raise HarnessError(f"weaviate request failed: {error}") from error
    return json.loads(body) if body else {}


def wait_ready(endpoint: str, deadline_seconds: float) -> float:
    started = time.monotonic()
    deadline = started + deadline_seconds
    while True:
        try:
            request = urllib.request.Request(
                f"{endpoint}/v1/.well-known/ready", method="GET"
            )
            with urllib.request.urlopen(request, timeout=5) as response:
                if response.status == 200:
                    return time.monotonic() - started
        except OSError:
            pass
        if time.monotonic() > deadline:
            raise HarnessError("weaviate did not become ready")
        time.sleep(0.2)


def provision_class(endpoint: str) -> None:
    try:
        http_json("DELETE", f"{endpoint}/v1/schema/{CLASS_NAME}")
    except HarnessError:
        pass
    http_json(
        "POST",
        f"{endpoint}/v1/schema",
        {
            "class": CLASS_NAME,
            "vectorizer": "none",
            "properties": [
                {"name": "corpusId", "dataType": ["text"]},
                {"name": "text", "dataType": ["text"]},
            ],
        },
    )


def ingest(
    endpoint: str,
    documents: list[dict],
    vectors: list[list[float]] | None,
) -> None:
    for offset in range(0, len(documents), INGEST_BATCH_OBJECTS):
        window = documents[offset : offset + INGEST_BATCH_OBJECTS]
        objects = []
        for index, document in enumerate(window):
            entry: dict = {
                "class": CLASS_NAME,
                "properties": {
                    "corpusId": document["corpus_id"],
                    "text": document["text"],
                },
            }
            if vectors is not None:
                entry["vector"] = vectors[offset + index]
            objects.append(entry)
        results = http_json(
            "POST", f"{endpoint}/v1/batch/objects", {"objects": objects}
        )
        for result in results if isinstance(results, list) else []:
            errors = (result.get("result") or {}).get("errors")
            if errors:
                raise HarnessError(f"batch object failed: {errors}")


def query_ranking(
    endpoint: str,
    query_text: str,
    query_vector: list[float] | None,
    alpha: float,
    k: int,
) -> list[str]:
    escaped = json.dumps(query_text)
    if query_vector is not None:
        hybrid = (
            f"hybrid: {{query: {escaped}, vector: {json.dumps(query_vector)}, "
            f"alpha: {alpha}}}"
        )
    else:
        hybrid = f"bm25: {{query: {escaped}}}"
    body = http_json(
        "POST",
        f"{endpoint}/v1/graphql",
        {
            "query": "{ Get { %s(%s, limit: %d) { corpusId } } }"
            % (CLASS_NAME, hybrid, k)
        },
    )
    if body.get("errors"):
        raise HarnessError(f"graphql failed: {body['errors']}")
    entries = ((body.get("data") or {}).get("Get") or {}).get(CLASS_NAME) or []
    return [str(entry["corpusId"]) for entry in entries]


def container_rss_bytes(container: str) -> int | None:
    completed = subprocess.run(
        [
            container_runtime(),
            "stats",
            "--no-stream",
            "--format",
            "{{.MemUsage}}",
            container,
        ],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    if completed.returncode != 0:
        return None
    used = completed.stdout.strip().split("/")[0].strip()
    units = {"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30}
    for suffix, scale in units.items():
        if used.endswith(suffix):
            try:
                return int(float(used[: -len(suffix)]) * scale)
            except ValueError:
                return None
    return None


def cold_start_seconds(endpoint: str, container: str) -> float | None:
    completed = subprocess.run(
        [container_runtime(), "restart", container],
        capture_output=True,
        text=True,
        timeout=600,
        check=False,
    )
    if completed.returncode != 0:
        return None
    return round(wait_ready(endpoint, 600), 2)


def evaluate(
    endpoint: str,
    dataset: str,
    data_root: Path,
    k: int,
    download: bool,
    query_limit: int | None,
    embed_binary: Path | None,
    model_dir: Path | None,
    alpha: float,
    container: str | None,
) -> dict:
    extracted = acquire_dataset(dataset, data_root, download)
    corpus = load_jsonl(extracted / "corpus.jsonl")
    if len(corpus) != DATASETS[dataset]["documents"]:
        raise HarnessError(f"corpus cardinality differs for {dataset}: {len(corpus)}")
    queries = {
        str(row["_id"]): str(row["text"])
        for row in load_jsonl(extracted / "queries.jsonl")
    }
    qrels = load_qrels(extracted / DATASETS[dataset]["qrels"])
    evaluated_ids = sorted(qrels, key=str)
    if query_limit is not None:
        evaluated_ids = evaluated_ids[:query_limit]

    meta = http_json("GET", f"{endpoint}/v1/meta")
    ordered = sorted(corpus, key=lambda row: str(row["_id"]))
    documents = [
        {
            "corpus_id": str(row["_id"]),
            "text": f"{row.get('title', '')}\n{row.get('text', '')}".strip(),
        }
        for row in ordered
    ]
    corpus_vectors: list[list[float]] | None = None
    if embed_binary is not None and model_dir is not None:
        corpus_vectors = []
        texts = [document["text"] for document in documents]
        for offset in range(0, len(texts), EMBED_BATCH_TEXTS):
            vectors, _attestation = embed_texts(
                embed_binary, model_dir, texts[offset : offset + EMBED_BATCH_TEXTS]
            )
            corpus_vectors.extend(vectors)

    provision_class(endpoint)
    ingest_started = time.monotonic()
    ingest(endpoint, documents, corpus_vectors)
    ingest_seconds = time.monotonic() - ingest_started
    rss_after_ingest = container_rss_bytes(container) if container else None

    ndcg_total = recall_total = mrr_total = 0.0
    evaluated = 0
    rankings: dict[str, list[str]] = {}
    query_started = time.monotonic()
    for query_id in evaluated_ids:
        query_text = queries.get(query_id)
        if query_text is None:
            raise HarnessError(f"qrels query is missing: {query_id}")
        query_vector = None
        if embed_binary is not None and model_dir is not None:
            vectors, _attestation = embed_texts(embed_binary, model_dir, [query_text])
            query_vector = vectors[0]
        ranking = query_ranking(endpoint, query_text, query_vector, alpha, k)
        rankings[query_id] = ranking
        relevant = qrels[query_id]
        ndcg_total += ndcg_at_k(ranking, relevant, k)
        recall_total += recall_at_k(ranking, relevant, k)
        mrr_total += mrr_at_k(ranking, relevant, k)
        evaluated += 1
    query_seconds = time.monotonic() - query_started
    if evaluated == 0:
        raise HarnessError("no queries were evaluated")
    rss_after_queries = container_rss_bytes(container) if container else None

    # Stability pass: identical queries immediately rerun; a deterministic
    # engine returns identical rankings.
    changed_rankings = 0
    for query_id in evaluated_ids:
        query_text = queries[query_id]
        query_vector = None
        if embed_binary is not None and model_dir is not None:
            vectors, _attestation = embed_texts(embed_binary, model_dir, [query_text])
            query_vector = vectors[0]
        if query_ranking(endpoint, query_text, query_vector, alpha, k) != rankings[
            query_id
        ]:
            changed_rankings += 1

    restart_seconds = (
        cold_start_seconds(endpoint, container) if container else None
    )

    return {
        "schema": RECEIPT_SCHEMA,
        "dataset": {
            "name": dataset,
            "archive_sha256": DATASETS[dataset]["sha256"],
            "documents": len(corpus),
            "queries_evaluated": evaluated,
            "qrels": DATASETS[dataset]["qrels"],
        },
        "engine": {
            "server": "weaviate",
            "version": meta.get("version", ""),
            "modules": sorted((meta.get("modules") or {}).keys()),
        },
        "protocol": {
            "k": k,
            "class": CLASS_NAME,
            "ingest_batch_objects": INGEST_BATCH_OBJECTS,
            "branches": "bm25" if corpus_vectors is None else "hybrid",
            "alpha": alpha if corpus_vectors is not None else None,
            "vector_source": (
                None if corpus_vectors is None else "attested-local-embedder"
            ),
            "transport": "rest+graphql",
            "ingest_order": "sorted-corpus-id",
        },
        "host": host_declaration(),
        "cost": {
            "ingest_seconds": round(ingest_seconds, 2),
            "query_seconds": round(query_seconds, 2),
            "rss_bytes_after_ingest": rss_after_ingest,
            "rss_bytes_after_queries": rss_after_queries,
            "cold_start_ready_seconds": restart_seconds,
        },
        "stability": {
            "rerun_queries": evaluated,
            "changed_rankings": changed_rankings,
        },
        "metrics": {
            f"ndcg@{k}": round(ndcg_total / evaluated, 6),
            f"recall@{k}": round(recall_total / evaluated, 6),
            f"mrr@{k}": round(mrr_total / evaluated, 6),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default="http://127.0.0.1:8080")
    parser.add_argument("--dataset", choices=sorted(DATASETS), required=True)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--query-limit", type=int, default=None)
    parser.add_argument("--embed-binary", type=Path, default=None)
    parser.add_argument("--model-dir", type=Path, default=None)
    parser.add_argument("--alpha", type=float, default=0.5)
    parser.add_argument(
        "--container",
        default=None,
        help="docker container name for RSS and cold-start probes",
    )
    parser.add_argument("--output", type=Path, default=None)
    arguments = parser.parse_args()
    if arguments.k < 1 or arguments.k > 1024:
        print("error: k must be within 1..=1024", file=sys.stderr)
        return 1
    if not 0.0 <= arguments.alpha <= 1.0:
        print("error: alpha must be within 0..=1", file=sys.stderr)
        return 1
    try:
        receipt = evaluate(
            arguments.endpoint.rstrip("/"),
            arguments.dataset,
            arguments.data_root,
            arguments.k,
            arguments.download,
            arguments.query_limit,
            arguments.embed_binary,
            arguments.model_dir,
            arguments.alpha,
            arguments.container,
        )
    except (HarnessError, OSError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True)
    if arguments.output is not None:
        arguments.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
