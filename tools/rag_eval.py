#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Measure Hyphae retrieval relevance under the pinned evidence protocol.

The harness ingests one pinned public BEIR-format dataset into a fresh
Native directory through the shipped binary, executes every test query
through the integrated search surface, and reports NDCG@k, Recall@k, and
MRR@k in a receipt that pins the dataset digests, the binary version, and
the host declaration. Every step is deterministic: dataset archives are
verified against frozen SHA-256 digests before use, documents are ingested
in sorted identifier order, and metrics are computed with a fixed rounding
discipline. Nothing is downloaded unless --download is passed explicitly.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python" / "src"))

from hyphae_sdk.v2 import HyphaeClient  # noqa: E402
from hyphae_sdk.v2.protocol import operation_required_minor  # noqa: E402

RECEIPT_SCHEMA = "hyphae-rag-relevance-receipt-v1"
BEIR_BASE_URL = "https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets"
INGEST_BATCH_DOCUMENTS = 256
CANDIDATE_LIMIT = 1000
MAX_DATASET_DOCUMENTS = 100_000

# Frozen dataset inventory: archives are immutable upstream artifacts and any
# digest change fails closed. Corpora above the collection-document cap join
# this table only when the cap is raised with evidence (R5).
DATASETS = {
    "fiqa": {
        "archive": "fiqa.zip",
        "sha256": "32c7df99ed21252fdfb2cf3f5673502a8d245ee0c44c4a133570d92ce2b3ad02",
        "documents": 57638,
        "qrels": "qrels/test.tsv",
    },
    "scifact": {
        "archive": "scifact.zip",
        "sha256": "536e14446a0ba56ed1398ab1055f39fe852686ecad24a6306c80c490fa8e0165",
        "documents": 5183,
        "qrels": "qrels/test.tsv",
    },
    "nfcorpus": {
        "archive": "nfcorpus.zip",
        "sha256": "efe5be03f8c5b86a5870102d0599d227c8c6e2484328e68c6522560385671b0b",
        "documents": 3633,
        "qrels": "qrels/test.tsv",
    },
}


class HarnessError(Exception):
    """Fail-closed harness failure."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def acquire_dataset(name: str, data_root: Path, download: bool) -> Path:
    entry = DATASETS[name]
    archive = data_root / entry["archive"]
    if not archive.is_file():
        if not download:
            raise HarnessError(
                f"dataset archive is missing: {archive} (pass --download to fetch it)"
            )
        data_root.mkdir(parents=True, exist_ok=True)
        url = f"{BEIR_BASE_URL}/{entry['archive']}"
        with urllib.request.urlopen(url, timeout=120) as response, archive.open(
            "wb"
        ) as handle:
            shutil.copyfileobj(response, handle)
    observed = sha256_file(archive)
    if observed != entry["sha256"]:
        raise HarnessError(
            f"dataset digest differs for {name}: observed {observed}, "
            f"frozen {entry['sha256']}"
        )
    extracted = data_root / name
    if not (extracted / "corpus.jsonl").is_file():
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.namelist():
                target = (data_root / member).resolve()
                if not str(target).startswith(str(data_root.resolve())):
                    raise HarnessError(f"archive member escapes the root: {member}")
            bundle.extractall(data_root)
    return extracted


def load_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def load_qrels(path: Path) -> dict[str, dict[str, int]]:
    qrels: dict[str, dict[str, int]] = {}
    with path.open(encoding="utf-8") as handle:
        header = handle.readline()
        if not header.lower().startswith("query-id"):
            raise HarnessError(f"unexpected qrels header in {path}: {header!r}")
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != 3:
                raise HarnessError(f"malformed qrels row: {line!r}")
            query_id, corpus_id, score = parts
            qrels.setdefault(query_id, {})[corpus_id] = int(score)
    return qrels


def run_binary(binary: Path, arguments: list[str], timeout: int = 600) -> dict:
    completed = subprocess.run(
        [str(binary), *arguments],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if completed.returncode != 0:
        raise HarnessError(
            f"binary failed: {' '.join(arguments[:4])}...: {completed.stderr.strip()[:400]}"
        )
    return json.loads(completed.stdout)


def provision(binary: Path, data_dir: Path, dimension: int) -> None:
    run_binary(binary, ["init", "--data-dir", str(data_dir)])
    run_binary(
        binary,
        [
            "catalog",
            "--data-dir",
            str(data_dir),
            "create-search-collection",
            "--database",
            "10",
            "--schema",
            "11",
            "--collection",
            "13",
            "--analyzer",
            "12",
            "--name",
            "main.public.rag_eval",
            "--dimension",
            str(dimension),
        ],
    )
    run_binary(
        binary,
        ["search", "--data-dir", str(data_dir), "provision", "--collection", "13"],
    )


def start_daemon(binary: Path, data_dir: Path) -> tuple[subprocess.Popen, Path]:
    endpoint = Path(tempfile.gettempdir()) / f"hyphae-rag-eval-{uuid.uuid4().hex}.sock"
    process = subprocess.Popen(
        [str(binary), "serve", "--data-dir", str(data_dir), "--endpoint", str(endpoint)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    # Opening a directory with a large live index legitimately takes longer
    # than the historical 30-second bound on modest hardware.
    deadline = time.monotonic() + 600
    while not endpoint.exists():
        if process.poll() is not None:
            raise HarnessError(f"serve exited early: {process.stderr.read()[:400]}")
        if time.monotonic() > deadline:
            process.terminate()
            raise HarnessError("serve did not bind its endpoint")
        time.sleep(0.05)
    return process, endpoint


def stop_daemon(process: subprocess.Popen, endpoint: Path) -> None:
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=15)
    endpoint.unlink(missing_ok=True)


def embed_texts(
    embed_binary: Path, model_dir: Path, texts: list[str]
) -> tuple[list[list[float]], str]:
    """Embeds texts through the attested local tool, returning vectors and
    the attestation envelope hex."""
    completed = subprocess.run(
        [str(embed_binary), "embed", "--model-dir", str(model_dir)],
        input=json.dumps(texts),
        capture_output=True,
        text=True,
        timeout=3600,
        check=False,
    )
    if completed.returncode != 0:
        raise HarnessError(f"embedding failed: {completed.stderr.strip()[:300]}")
    decoded = json.loads(completed.stdout)
    vectors = decoded.get("vectors")
    if not isinstance(vectors, list) or len(vectors) != len(texts):
        raise HarnessError("embedding output shape differs")
    return vectors, str(decoded.get("attestation_hex", ""))


def rerank_texts(
    embed_binary: Path, model_dir: Path, query: str, texts: list[str]
) -> tuple[list[float], str]:
    """Scores texts against the query through the attested local tool,
    returning scores and the attestation envelope hex."""
    completed = subprocess.run(
        [
            str(embed_binary),
            "rerank",
            "--model-dir",
            str(model_dir),
            "--query",
            query,
        ],
        input=json.dumps(texts),
        capture_output=True,
        text=True,
        timeout=3600,
        check=False,
    )
    if completed.returncode != 0:
        raise HarnessError(f"rerank failed: {completed.stderr.strip()[:300]}")
    decoded = json.loads(completed.stdout)
    scores = decoded.get("scores")
    if not isinstance(scores, list) or len(scores) != len(texts):
        raise HarnessError("rerank output shape differs")
    return scores, str(decoded.get("attestation_hex", ""))


def prepare_documents(corpus: list[dict]) -> tuple[dict[int, str], list[dict]]:
    identifier_map: dict[int, str] = {}
    documents = []
    for ordinal, row in enumerate(
        sorted(corpus, key=lambda row: str(row["_id"])), start=1
    ):
        identifier_map[ordinal] = str(row["_id"])
        text = f"{row.get('title', '')}\n{row.get('text', '')}".strip()
        documents.append({"object_id": ordinal, "text": text})
    return identifier_map, documents


def ingest_batches(
    client: HyphaeClient, documents: list[dict], offsets: list[int], batch_size: int
) -> None:
    for offset in offsets:
        batch = documents[offset : offset + batch_size]
        client.search_ingest(
            13,
            {
                "idempotency_id": offset // batch_size + 1,
                "documents": batch,
            },
        )


def execute_query(
    client: HyphaeClient,
    query: str,
    k: int,
    query_vector: list[float] | None,
    fusion: str | None,
    rerank: dict | None = None,
) -> list[int]:
    request: dict = {
        "lexical": {"query": query, "candidate_limit": CANDIDATE_LIMIT, "weight": 1},
        "vectors": [],
        "limit": k,
    }
    if query_vector is not None:
        request["vectors"] = [
            {
                "target": "exact",
                "query": query_vector,
                "candidate_limit": CANDIDATE_LIMIT,
                "weight": 1,
            }
        ]
    if fusion is not None:
        request["fusion"] = fusion
    if rerank is not None:
        request["rerank"] = rerank
    response = client.search_collection(13, request)
    return [int(hit["object_id"]) for hit in response.value.get("hits", [])]


def ndcg_at_k(ranking: list[str], relevant: dict[str, int], k: int) -> float:
    gains = [relevant.get(document, 0) for document in ranking[:k]]
    dcg = sum(
        gain / math.log2(position + 2) for position, gain in enumerate(gains)
    )
    ideal = sorted(relevant.values(), reverse=True)[:k]
    idcg = sum(
        gain / math.log2(position + 2) for position, gain in enumerate(ideal)
    )
    return dcg / idcg if idcg > 0 else 0.0


def recall_at_k(ranking: list[str], relevant: dict[str, int], k: int) -> float:
    judged = {document for document, score in relevant.items() if score > 0}
    if not judged:
        return 0.0
    return len(judged.intersection(ranking[:k])) / len(judged)


def mrr_at_k(ranking: list[str], relevant: dict[str, int], k: int) -> float:
    for position, document in enumerate(ranking[:k], start=1):
        if relevant.get(document, 0) > 0:
            return 1.0 / position
    return 0.0


def host_declaration() -> dict:
    cpu_model = ""
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.is_file():
        for line in cpuinfo.read_text(encoding="utf-8").splitlines():
            if line.lower().startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu_model": cpu_model,
        "python": platform.python_version(),
    }


def directory_bytes(data_dir: Path) -> int:
    return sum(f.stat().st_size for f in data_dir.rglob("*") if f.is_file())


def evaluate(
    binary: Path,
    dataset: str,
    data_root: Path,
    k: int,
    download: bool,
    query_limit: int | None,
    maintenance_interval_batches: int,
    embed_binary: Path | None,
    model_dir: Path | None,
    fusion: str | None,
    rerank_candidates: int,
    lexical_only: bool,
) -> dict:
    extracted = acquire_dataset(dataset, data_root, download)
    corpus = load_jsonl(extracted / "corpus.jsonl")
    if len(corpus) != DATASETS[dataset]["documents"]:
        raise HarnessError(
            f"corpus cardinality differs for {dataset}: {len(corpus)}"
        )
    if len(corpus) > MAX_DATASET_DOCUMENTS:
        raise HarnessError(
            f"{dataset} exceeds the collection document cap; gated on R5"
        )
    queries = {
        str(row["_id"]): str(row["text"]) for row in load_jsonl(extracted / "queries.jsonl")
    }
    qrels = load_qrels(extracted / DATASETS[dataset]["qrels"])
    evaluated_ids = sorted(qrels, key=str)
    if query_limit is not None:
        evaluated_ids = evaluated_ids[:query_limit]

    if rerank_candidates > 0 and (
        operation_required_minor(
            "search_collection",
            {"request": {"rerank": {"attestation": b"x", "scores": []}}},
        )
        < 4
    ):
        # An SDK without the rerank encoder would silently drop the stage
        # and reproduce the baseline ranking; fail closed instead.
        raise HarnessError("the installed SDK cannot encode the rerank stage")
    version = run_binary(binary, ["version", "--json"])
    # The working directory lives next to the dataset cache: /tmp is often a
    # memory-backed filesystem whose quota a durable ingest exhausts.
    with tempfile.TemporaryDirectory(
        prefix="hyphae-rag-eval-", dir=data_root
    ) as scratch:
        data_dir = Path(scratch) / "data"
        embed_dimension = 2
        corpus_vectors: list[list[float]] | None = None
        corpus_attestations: list[str] = []
        if embed_binary is not None and model_dir is not None and not lexical_only:
            ordered = sorted(corpus, key=lambda row: str(row["_id"]))
            texts = [
                f"{row.get('title', '')}\n{row.get('text', '')}".strip() for row in ordered
            ]
            corpus_vectors = []
            for offset in range(0, len(texts), INGEST_BATCH_DOCUMENTS):
                vectors, attestation = embed_texts(
                    embed_binary, model_dir, texts[offset : offset + INGEST_BATCH_DOCUMENTS]
                )
                corpus_vectors.extend(vectors)
                corpus_attestations.append(attestation)
            embed_dimension = len(corpus_vectors[0]) if corpus_vectors else 2
        provision(binary, data_dir, embed_dimension)
        process, endpoint = start_daemon(binary, data_dir)
        try:
            client = HyphaeClient.local(str(endpoint))
            import time as _time

            identifier_map, documents = prepare_documents(corpus)
            # Vector payloads shrink the batch so one request stays inside
            # the bounded local-protocol frame.
            batch_size = INGEST_BATCH_DOCUMENTS if corpus_vectors is None else 64
            if corpus_vectors is not None and maintenance_interval_batches == 0:
                # Vector deltas are capped; consolidate inside the cap.
                maintenance_interval_batches = 12
            if corpus_vectors is not None:
                for document, vector in zip(documents, corpus_vectors):
                    document["vectors"] = {"exact": vector}
            offsets = list(range(0, len(documents), batch_size))
            if maintenance_interval_batches > 0:
                windows = [
                    offsets[start : start + maintenance_interval_batches]
                    for start in range(0, len(offsets), maintenance_interval_batches)
                ]
            else:
                windows = [offsets]
            ingest_seconds = 0.0
            maintenance_seconds = 0.0
            ingested_bytes = 0
            for ordinal, window in enumerate(windows, start=1):
                ingest_started = _time.monotonic()
                ingest_batches(client, documents, window, batch_size)
                ingest_seconds += _time.monotonic() - ingest_started
                ingested_bytes = max(ingested_bytes, directory_bytes(data_dir))
                # Reclaim transient page and WAL generations: an unmaintained
                # directory grows unboundedly, every query pays to materialize
                # it, and large corpora exhaust the disk before the ingest
                # completes. The final cycle also isolates the query phase.
                client.close()
                stop_daemon(process, endpoint)
                maintenance_started = _time.monotonic()
                # Maintenance on a large transient directory legitimately
                # exceeds the default operation timeout.
                if corpus_vectors is not None:
                    # Drain accumulated vector deltas into a fresh generation
                    # before they reach the bounded delta capacity.
                    run_binary(
                        binary,
                        ["search", "--data-dir", str(data_dir), "consolidate", "--collection", "13"],
                        timeout=7200,
                    )
                run_binary(
                    binary, ["checkpoint", "--data-dir", str(data_dir)], timeout=7200
                )
                run_binary(binary, ["vacuum", "--data-dir", str(data_dir)], timeout=7200)
                maintenance_seconds += _time.monotonic() - maintenance_started
                if ordinal < len(windows):
                    process, endpoint = start_daemon(binary, data_dir)
                    client = HyphaeClient.local(str(endpoint))
            maintained_bytes = directory_bytes(data_dir)
            process, endpoint = start_daemon(binary, data_dir)
            client = HyphaeClient.local(str(endpoint))
            ndcg_total = recall_total = mrr_total = 0.0
            evaluated = 0
            rerank_attestations = 0
            query_started = _time.monotonic()
            for query_id in evaluated_ids:
                query_text = queries.get(query_id)
                if query_text is None:
                    raise HarnessError(f"qrels query is missing: {query_id}")
                query_vector = None
                if embed_binary is not None and model_dir is not None and not lexical_only:
                    vectors, attestation = embed_texts(
                        embed_binary, model_dir, [query_text]
                    )
                    query_vector = vectors[0]
                if rerank_candidates > 0:
                    # Two passes: retrieve candidates, score them through the
                    # attested local model, and let the engine apply the
                    # attested rerank stage inside the search pipeline.
                    first = execute_query(
                        client, query_text, rerank_candidates, query_vector, fusion
                    )
                    stage = None
                    if first:
                        texts = [documents[ordinal - 1]["text"] for ordinal in first]
                        scores, rerank_attestation = rerank_texts(
                            embed_binary, model_dir, query_text, texts
                        )
                        stage = {
                            "attestation": bytes.fromhex(rerank_attestation),
                            "scores": [
                                {"object_id": ordinal, "score": score}
                                for ordinal, score in zip(first, scores)
                            ],
                        }
                        rerank_attestations += 1
                    ordinals = execute_query(
                        client, query_text, k, query_vector, fusion, rerank=stage
                    )
                else:
                    ordinals = execute_query(client, query_text, k, query_vector, fusion)
                ranking = [identifier_map[ordinal] for ordinal in ordinals]
                relevant = qrels[query_id]
                ndcg_total += ndcg_at_k(ranking, relevant, k)
                recall_total += recall_at_k(ranking, relevant, k)
                mrr_total += mrr_at_k(ranking, relevant, k)
                evaluated += 1
            query_seconds = _time.monotonic() - query_started
            client.close()
        finally:
            stop_daemon(process, endpoint)
    if evaluated == 0:
        raise HarnessError("no queries were evaluated")
    return {
        "schema": RECEIPT_SCHEMA,
        "dataset": {
            "name": dataset,
            "archive_sha256": DATASETS[dataset]["sha256"],
            "documents": len(corpus),
            "queries_evaluated": evaluated,
            "qrels": DATASETS[dataset]["qrels"],
        },
        "engine": version,
        "protocol": {
            "k": k,
            "candidate_limit": CANDIDATE_LIMIT,
            "ingest_batch_documents": INGEST_BATCH_DOCUMENTS,
            "maintenance_interval_batches": maintenance_interval_batches,
            "branches": (
                "lexical"
                if embed_binary is None or lexical_only
                else "lexical+exact-vector"
            ),
            "fusion": fusion or "weighted-reciprocal-rank",
            "embedding_attestations": len(corpus_attestations),
            "rerank": (
                {
                    "candidates": rerank_candidates,
                    "attested_queries": rerank_attestations,
                }
                if rerank_candidates > 0
                else None
            ),
            "transport": "local-uds-daemon",
            "ingest_order": "sorted-corpus-id",
        },
        "host": host_declaration(),
        "cost": {
            "ingest_seconds": round(ingest_seconds, 2),
            "maintenance_seconds": round(maintenance_seconds, 2),
            "query_seconds": round(query_seconds, 2),
            "data_directory_bytes_after_ingest": ingested_bytes,
            "data_directory_bytes_after_maintenance": maintained_bytes,
        },
        "metrics": {
            f"ndcg@{k}": round(ndcg_total / evaluated, 6),
            f"recall@{k}": round(recall_total / evaluated, 6),
            f"mrr@{k}": round(mrr_total / evaluated, 6),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--dataset", choices=sorted(DATASETS), required=True)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--query-limit", type=int, default=None)
    parser.add_argument("--embed-binary", type=Path, default=None)
    parser.add_argument("--model-dir", type=Path, default=None)
    parser.add_argument("--fusion", choices=["weighted_score"], default=None)
    parser.add_argument(
        "--rerank-candidates",
        type=int,
        default=0,
        help="rerank this many first-pass candidates through the attested"
        " local model inside the search pipeline (0 disables; max 256)",
    )
    parser.add_argument(
        "--lexical-only",
        action="store_true",
        help="keep the vector branch off even when a model is supplied"
        " (the model still serves --rerank-candidates)",
    )
    parser.add_argument(
        "--maintenance-interval-batches",
        type=int,
        default=0,
        help="run a checkpoint+vacuum cycle after every N ingest batches"
        " (0 keeps the single post-ingest cycle)",
    )
    parser.add_argument("--output", type=Path, default=None)
    arguments = parser.parse_args()
    if not arguments.binary.is_file():
        print(f"error: binary is missing: {arguments.binary}", file=sys.stderr)
        return 1
    if arguments.k < 1 or arguments.k > 1024:
        print("error: k must be within 1..=1024", file=sys.stderr)
        return 1
    if arguments.maintenance_interval_batches < 0:
        print("error: maintenance interval must be nonnegative", file=sys.stderr)
        return 1
    if not 0 <= arguments.rerank_candidates <= 256:
        print("error: rerank candidates must be within 0..=256", file=sys.stderr)
        return 1
    if arguments.rerank_candidates > 0 and (
        arguments.embed_binary is None or arguments.model_dir is None
    ):
        print("error: rerank needs --embed-binary and --model-dir", file=sys.stderr)
        return 1
    try:
        receipt = evaluate(
            arguments.binary.resolve(),
            arguments.dataset,
            arguments.data_root,
            arguments.k,
            arguments.download,
            arguments.query_limit,
            arguments.maintenance_interval_batches,
            arguments.embed_binary,
            arguments.model_dir,
            arguments.fusion,
            arguments.rerank_candidates,
            arguments.lexical_only,
        )
    except (HarnessError, OSError, json.JSONDecodeError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True)
    if arguments.output is not None:
        arguments.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
