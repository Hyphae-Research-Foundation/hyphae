#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run one offline Agent Memory lexical retrieval quality and latency eval.

The harness provisions a disposable Native directory through the shipped
binary, uses only the Native local protocol, and writes a source-bound local
diagnostic receipt. It never reads the user's Agent Memory directory and never
authorizes publication.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from collections import defaultdict
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python" / "src"))

from hyphae_sdk.v2 import HyphaeClient  # noqa: E402

RECEIPT_SCHEMA = "hyphae-agent-memory-lexical-eval-receipt-v1"
FIXTURE_SCHEMA = "hyphae-agent-memory-lexical-eval-fixture-v1"
DOMAINS = ("personal", "work", "journal")
HARNESSES = ("claude-code-cli", "codex-cli", "opencode-cli", "pi-cli")
KINDS = ("decision", "command", "constraint", "fact", "note")
MAX_TEXT_BYTES = 4096
MAX_DOCUMENTS = 100_000
MAX_QUERIES = 10_000
TELEMETRY_TIMINGS = (
    "hyphae.product.timing.admission_us",
    "hyphae.product.timing.queueing_us",
    "hyphae.product.timing.planning_us",
    "hyphae.product.timing.engine_execution_us",
    "hyphae.product.timing.transport_us",
    "hyphae.product.timing.result_encoding_us",
    "hyphae.product.timing.request_decoding_us",
    "hyphae.product.timing.wal_append_us",
    "hyphae.product.timing.page_synchronization_us",
    "hyphae.product.timing.wal_synchronization_us",
    "hyphae.product.timing.durability_us",
)


class EvalError(Exception):
    """Fail-closed evaluation failure."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def require_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    if set(value) != expected:
        raise EvalError(f"{context} has unexpected fields")


def load_fixture(path: Path) -> dict[str, Any]:
    try:
        fixture = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise EvalError(f"fixture is invalid: {path}") from error
    if not isinstance(fixture, dict):
        raise EvalError("fixture must be one JSON object")
    require_keys(
        fixture,
        {"schema", "project", "collections", "protocol", "documents", "queries"},
        "fixture",
    )
    if fixture["schema"] != FIXTURE_SCHEMA:
        raise EvalError("fixture schema is unsupported")
    if not isinstance(fixture["project"], str) or not fixture["project"]:
        raise EvalError("fixture project is invalid")
    if fixture["project"] == "_global":
        raise EvalError("fixture project uses the reserved global identity")
    if fixture["collections"] != {"personal": 21, "work": 22, "journal": 23}:
        raise EvalError("fixture collections are not canonical")
    require_keys(fixture["protocol"], {"candidate_limit", "cutoffs"}, "protocol")
    candidate_limit = fixture["protocol"]["candidate_limit"]
    cutoffs = fixture["protocol"]["cutoffs"]
    if not isinstance(candidate_limit, int) or not 1 <= candidate_limit <= 10_000:
        raise EvalError("candidate limit is invalid")
    if cutoffs != [1, 5, 10]:
        raise EvalError("fixture cutoffs are not canonical")

    documents = fixture["documents"]
    queries = fixture["queries"]
    if (
        not isinstance(documents, list)
        or not 1 <= len(documents) <= MAX_DOCUMENTS
        or not isinstance(queries, list)
        or not 1 <= len(queries) <= MAX_QUERIES
    ):
        raise EvalError("fixture cardinality is invalid")
    document_ids: set[int] = set()
    represented_domains: set[str] = set()
    represented_harnesses: set[str] = set()
    models: set[str] = set()
    for ordinal, document in enumerate(documents):
        if not isinstance(document, dict):
            raise EvalError(f"document {ordinal} is invalid")
        require_keys(
            document,
            {
                "object_id", "project", "scope", "domain", "kind", "agent",
                "harness", "model", "text",
            },
            f"document {ordinal}",
        )
        object_id = document["object_id"]
        if not isinstance(object_id, int) or object_id <= 0 or object_id in document_ids:
            raise EvalError(f"document {ordinal} identity is invalid")
        document_ids.add(object_id)
        domain = document["domain"]
        harness = document["harness"]
        model = document["model"]
        text = document["text"]
        if domain not in DOMAINS or harness not in HARNESSES:
            raise EvalError(f"document {ordinal} provenance is invalid")
        if document["kind"] not in KINDS or not isinstance(document["agent"], str):
            raise EvalError(f"document {ordinal} kind or agent is invalid")
        if not isinstance(model, str) or not model:
            raise EvalError(f"document {ordinal} model must be explicit")
        if not isinstance(text, str) or not 1 <= len(text.encode("utf-8")) <= MAX_TEXT_BYTES:
            raise EvalError(f"document {ordinal} text is invalid")
        if domain == "journal" and not text.startswith(("I ", "Yo ", "Pienso ")):
            raise EvalError(f"journal document {ordinal} is not first-person")
        scope, project = document["scope"], document["project"]
        if (scope == "global" and project != "_global") or (
            scope == "project" and project not in {fixture["project"], "eval/foreign-project"}
        ):
            raise EvalError(f"document {ordinal} scope is invalid")
        if scope not in {"project", "global"}:
            raise EvalError(f"document {ordinal} scope is unsupported")
        represented_domains.add(domain)
        represented_harnesses.add(harness)
        models.add(model)
    if represented_domains != set(DOMAINS) or represented_harnesses != set(HARNESSES):
        raise EvalError("fixture does not cover every domain and harness")

    query_ids: set[str] = set()
    query_cells: set[tuple[str, str, str]] = set()
    for ordinal, query in enumerate(queries):
        if not isinstance(query, dict):
            raise EvalError(f"query {ordinal} is invalid")
        require_keys(query, {"id", "segment", "text", "qrels"}, f"query {ordinal}")
        query_id = query["id"]
        if not isinstance(query_id, str) or not query_id or query_id in query_ids:
            raise EvalError(f"query {ordinal} identity is invalid")
        query_ids.add(query_id)
        if not isinstance(query["text"], str) or not query["text"]:
            raise EvalError(f"query {ordinal} text is invalid")
        segment = query["segment"]
        if not isinstance(segment, dict):
            raise EvalError(f"query {ordinal} segment is invalid")
        require_keys(segment, {"domain", "harness", "model"}, f"query {ordinal} segment")
        cell = (segment["domain"], segment["harness"], segment["model"])
        if cell[0] not in DOMAINS or cell[1] not in HARNESSES or cell[2] not in models:
            raise EvalError(f"query {ordinal} segment is unsupported")
        query_cells.add(cell)
        qrels = query["qrels"]
        if not isinstance(qrels, list) or not qrels:
            raise EvalError(f"query {ordinal} qrels are empty")
        seen_qrels: set[int] = set()
        positive = False
        for qrel in qrels:
            if not isinstance(qrel, dict):
                raise EvalError(f"query {ordinal} qrel is invalid")
            require_keys(qrel, {"object_id", "relevance"}, f"query {ordinal} qrel")
            object_id, relevance = qrel["object_id"], qrel["relevance"]
            if (
                object_id not in document_ids
                or object_id in seen_qrels
                or not isinstance(relevance, int)
                or relevance < 0
            ):
                raise EvalError(f"query {ordinal} qrel is invalid")
            seen_qrels.add(object_id)
            positive = positive or relevance > 0
        if not positive:
            raise EvalError(f"query {ordinal} has no positive relevance")
    expected_cells = {
        (document["domain"], document["harness"], document["model"])
        for document in documents
        if document["project"] == fixture["project"]
    }
    if not expected_cells.issubset(query_cells):
        raise EvalError("fixture does not cover every domain/harness/model cell")
    return fixture


def ndcg_at_k(ranking: list[int], relevant: dict[int, int], k: int) -> float:
    gains = [relevant.get(document, 0) for document in ranking[:k]]
    dcg = sum(gain / math.log2(position + 2) for position, gain in enumerate(gains))
    ideal = sorted(relevant.values(), reverse=True)[:k]
    idcg = sum(gain / math.log2(position + 2) for position, gain in enumerate(ideal))
    return dcg / idcg if idcg > 0 else 0.0


def recall_at_k(ranking: list[int], relevant: dict[int, int], k: int) -> float:
    judged = {document for document, score in relevant.items() if score > 0}
    return len(judged.intersection(ranking[:k])) / len(judged) if judged else 0.0


def mrr_at_k(ranking: list[int], relevant: dict[int, int], k: int) -> float:
    for position, document in enumerate(ranking[:k], start=1):
        if relevant.get(document, 0) > 0:
            return 1.0 / position
    return 0.0


def nearest_rank(values: list[int], percentile: float) -> int:
    if not values or not 0 < percentile <= 100:
        raise EvalError("percentile input is invalid")
    ordered = sorted(values)
    rank = max(1, math.ceil(percentile / 100 * len(ordered)))
    return ordered[rank - 1]


def latency_summary(values: list[int]) -> dict[str, Any]:
    if not values:
        raise EvalError("latency sample is empty")
    total = sum(values)
    return {
        "samples": len(values),
        "p50_nanos": nearest_rank(values, 50),
        "p95_nanos": nearest_rank(values, 95),
        "p99_nanos": nearest_rank(values, 99),
        "p999_nanos": nearest_rank(values, 99.9),
        "maximum_nanos": max(values),
        "mean_nanos": round(total / len(values), 3),
        "total_nanos": total,
        "queries_per_second": round(len(values) * 1_000_000_000 / total, 3),
    }


def aggregate_metrics(results: list[dict[str, Any]], cutoffs: list[int]) -> dict[str, Any]:
    if not results:
        raise EvalError("metric segment is empty")
    aggregate: dict[str, Any] = {"query_count": len(results)}
    for cutoff in cutoffs:
        aggregate[f"ndcg@{cutoff}"] = round(
            sum(result["metrics"][f"ndcg@{cutoff}"] for result in results) / len(results), 6
        )
        aggregate[f"recall@{cutoff}"] = round(
            sum(result["metrics"][f"recall@{cutoff}"] for result in results) / len(results), 6
        )
        aggregate[f"hit_rate@{cutoff}"] = round(
            sum(result["metrics"][f"hit_rate@{cutoff}"] for result in results) / len(results), 6
        )
    aggregate["mrr@10"] = round(
        sum(result["metrics"]["mrr@10"] for result in results) / len(results), 6
    )
    aggregate["latency"] = latency_summary(
        [sample for result in results for sample in result["latencies_nanos"]]
    )
    return aggregate


def segmented_metrics(results: list[dict[str, Any]], cutoffs: list[int]) -> dict[str, Any]:
    dimensions = {
        "by_domain": lambda result: result["segment"]["domain"],
        "by_harness": lambda result: result["segment"]["harness"],
        "by_model": lambda result: result["segment"]["model"],
        "by_cell": lambda result: "/".join(
            (result["segment"]["domain"], result["segment"]["harness"], result["segment"]["model"])
        ),
    }
    output: dict[str, Any] = {"overall": aggregate_metrics(results, cutoffs)}
    for name, selector in dimensions.items():
        groups: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
        for result in results:
            groups[selector(result)].append(result)
        output[name] = {
            key: aggregate_metrics(value, cutoffs) for key, value in sorted(groups.items())
        }
    return output


def run_binary(binary: Path, arguments: list[str], timeout: int = 600) -> Any:
    completed = subprocess.run(
        [str(binary), *arguments], capture_output=True, text=True, timeout=timeout, check=False
    )
    if completed.returncode != 0:
        raise EvalError(
            f"binary failed: {' '.join(arguments[:5])}: "
            f"{(completed.stderr or completed.stdout).strip()[:400]}"
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return completed.stdout.strip()


def provision(binary: Path, data_dir: Path, collections: dict[str, int]) -> None:
    run_binary(binary, ["init", "--data-dir", str(data_dir)])
    for ordinal, domain in enumerate(DOMAINS):
        collection = collections[domain]
        arguments = [
            "catalog", "--data-dir", str(data_dir), "create-search-collection",
            "--database", "10", "--schema", "11", "--collection", str(collection),
            "--analyzer", "12", "--name", f"main.public.agent_memory_eval_{domain}",
            "--memory-schema",
        ]
        if ordinal > 0:
            arguments.append("--reuse-schema")
        run_binary(binary, arguments)
    for domain in DOMAINS:
        run_binary(
            binary,
            ["search", "--data-dir", str(data_dir), "provision", "--collection", str(collections[domain])],
        )


def start_daemon(binary: Path, data_dir: Path) -> tuple[subprocess.Popen[str], Path]:
    endpoint = Path(tempfile.gettempdir()) / f"hyphae-agent-memory-eval-{uuid.uuid4().hex}.sock"
    process = subprocess.Popen(
        [str(binary), "serve", "--data-dir", str(data_dir), "--endpoint", str(endpoint)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    deadline = time.monotonic() + 120
    while not endpoint.exists():
        if process.poll() is not None:
            raise EvalError(f"serve exited early: {process.stderr.read()[:400]}")
        if time.monotonic() > deadline:
            process.terminate()
            raise EvalError("serve did not bind its local endpoint")
        time.sleep(0.02)
    return process, endpoint


def stop_daemon(process: subprocess.Popen[str], endpoint: Path) -> None:
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=15)
    endpoint.unlink(missing_ok=True)


def doc_values(document: dict[str, Any]) -> dict[str, str]:
    return {
        "project": document["project"],
        "kind": document["kind"],
        "layer": document["domain"],
        "harness": document["harness"],
        "model": document["model"],
    }


def ingest(client: HyphaeClient, fixture: dict[str, Any]) -> None:
    by_domain: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
    for document in sorted(fixture["documents"], key=lambda item: item["object_id"]):
        by_domain[document["domain"]].append(
            {
                "object_id": document["object_id"],
                "text": document["text"],
                "doc_values": doc_values(document),
                "vectors": {},
            }
        )
    for domain in DOMAINS:
        documents = by_domain[domain]
        for offset in range(0, len(documents), 256):
            client.search_ingest(
                fixture["collections"][domain],
                {"idempotency_id": fixture["collections"][domain] * 1000 + offset + 1,
                 "documents": documents[offset : offset + 256]},
            )


def query_request(query: dict[str, Any], fixture: dict[str, Any], limit: int) -> dict[str, Any]:
    project = fixture["project"]
    return {
        "lexical": {
            "query": query["text"],
            "candidate_limit": fixture["protocol"]["candidate_limit"],
            "weight": 1,
        },
        "vectors": [],
        "filter": {
            "kind": "in",
            "field": "project",
            "values": [project, "_global"],
        },
        "limit": limit,
    }


def domain_query(client: HyphaeClient, fixture: dict[str, Any], query: dict[str, Any], limit: int) -> list[dict[str, Any]]:
    collection = fixture["collections"][query["segment"]["domain"]]
    response = client.search_collection(collection, query_request(query, fixture, limit))
    return response.value.get("hits", [])


def all_domain_query(client: HyphaeClient, fixture: dict[str, Any], query: dict[str, Any], limit: int) -> list[dict[str, Any]]:
    hits = []
    for domain in DOMAINS:
        response = client.search_collection(
            fixture["collections"][domain], query_request(query, fixture, limit)
        )
        hits.extend(response.value.get("hits", []))
    hits.sort(key=lambda hit: (-float(hit["score"]), int(hit["object_id"])))
    return hits[:limit]


def result_metrics(ranking: list[int], qrels: dict[int, int], cutoffs: list[int]) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for cutoff in cutoffs:
        metrics[f"ndcg@{cutoff}"] = ndcg_at_k(ranking, qrels, cutoff)
        metrics[f"recall@{cutoff}"] = recall_at_k(ranking, qrels, cutoff)
        metrics[f"hit_rate@{cutoff}"] = float(recall_at_k(ranking, qrels, cutoff) > 0)
    metrics["mrr@10"] = mrr_at_k(ranking, qrels, 10)
    return metrics


def evaluate_mode(
    client: HyphaeClient,
    fixture: dict[str, Any],
    repetitions: int,
    mode: str,
) -> tuple[list[dict[str, Any]], int, int, Any]:
    executor = domain_query if mode == "domain_filtered" else all_domain_query
    cutoffs = fixture["protocol"]["cutoffs"]
    limit = max(cutoffs)
    foreign_ids = {
        document["object_id"]
        for document in fixture["documents"]
        if document["project"] == "eval/foreign-project"
    }
    results = []
    foreign_hits = 0
    changed = 0
    ranking_digest = hashlib.sha256()
    for query in sorted(fixture["queries"], key=lambda item: item["id"]):
        baseline = [int(hit["object_id"]) for hit in executor(client, fixture, query, limit)]
        foreign_hits += len(set(baseline).intersection(foreign_ids))
        latencies = []
        for _ in range(repetitions):
            started = time.perf_counter_ns()
            ranking = [int(hit["object_id"]) for hit in executor(client, fixture, query, limit)]
            latencies.append(time.perf_counter_ns() - started)
            if ranking != baseline:
                changed += 1
        ranking_digest.update(query["id"].encode())
        ranking_digest.update(b"\0")
        for object_id in baseline:
            ranking_digest.update(object_id.to_bytes(16, "big"))
        qrels = {qrel["object_id"]: qrel["relevance"] for qrel in query["qrels"]}
        results.append(
            {
                "id": query["id"],
                "segment": query["segment"],
                "ranking": baseline,
                "metrics": result_metrics(baseline, qrels, cutoffs),
                "latencies_nanos": latencies,
            }
        )
    return results, foreign_hits, changed, ranking_digest


def telemetry_map(snapshot: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {metric["name"]: metric for metric in snapshot.get("metrics", [])}


def telemetry_delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    old, new = telemetry_map(before), telemetry_map(after)
    output = {}
    for name in TELEMETRY_TIMINGS:
        prior = old.get(name, {"count": 0, "sum_micros": 0, "buckets": [0] * 11})
        current = new.get(name, {"count": 0, "sum_micros": 0, "buckets": [0] * 11})
        output[name] = {
            "count": current["count"] - prior["count"],
            "sum_micros": current["sum_micros"] - prior["sum_micros"],
            "buckets": [right - left for left, right in zip(prior["buckets"], current["buckets"])],
        }
    return output


def git(root: Path, *arguments: str, environment: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        ["git", *arguments], cwd=root, env=environment, check=True,
        capture_output=True, text=True,
    )
    return completed.stdout.strip()


def worktree_tree(root: Path) -> str:
    with tempfile.TemporaryDirectory(prefix="hyphae-agent-memory-source-") as directory:
        environment = {**os.environ, "GIT_INDEX_FILE": str(Path(directory) / "index")}
        git(root, "read-tree", "HEAD", environment=environment)
        git(root, "add", "--all", environment=environment)
        return git(root, "write-tree", environment=environment)


def source_identity(binary: Path, require_clean: bool) -> dict[str, Any]:
    commit = git(REPO_ROOT, "rev-parse", "HEAD")
    status = git(REPO_ROOT, "status", "--porcelain=v1", "--untracked-files=all")
    clean = not status
    if require_clean and not clean:
        raise EvalError("source worktree is not clean")
    return {
        "commit": commit,
        "tree": git(REPO_ROOT, "rev-parse", "HEAD^{tree}") if clean else worktree_tree(REPO_ROOT),
        "source_mode": "clean" if clean else "integration",
        "clean": clean,
        "binary_sha256": sha256_file(binary),
        "harness_sha256": sha256_file(Path(__file__).resolve()),
    }


def host_declaration() -> dict[str, str]:
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


def evaluate(
    binary: Path,
    fixture_path: Path,
    repetitions: int,
    require_clean: bool,
) -> dict[str, Any]:
    fixture = load_fixture(fixture_path)
    source = source_identity(binary, require_clean)
    version = run_binary(binary, ["version", "--json"])
    scratch = Path(tempfile.mkdtemp(prefix="hyphae-agent-memory-eval-"))
    data_dir = scratch / "data"
    try:
        provision(binary, data_dir, fixture["collections"])
        process, endpoint = start_daemon(binary, data_dir)
        try:
            client = HyphaeClient.local(str(endpoint))
            ingest_started = time.perf_counter_ns()
            ingest(client, fixture)
            ingest_nanos = time.perf_counter_ns() - ingest_started
            before = client.telemetry().value
            domain_results, domain_foreign, domain_changed, domain_digest = evaluate_mode(
                client, fixture, repetitions, "domain_filtered"
            )
            all_results, all_foreign, all_changed, all_digest = evaluate_mode(
                client, fixture, repetitions, "all_domains"
            )
            after = client.telemetry().value
            client.close()
        finally:
            stop_daemon(process, endpoint)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    ranking_digest = hashlib.sha256(domain_digest.digest() + all_digest.digest()).hexdigest()
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "evidence_class": "local-diagnostic",
        "source": source,
        "publication": {
            "authorized": False,
            "reason": "local diagnostic only; publication requires explicit review and approval",
        },
        "fixture": {
            "schema": fixture["schema"],
            "sha256": sha256_file(fixture_path),
            "documents": len(fixture["documents"]),
            "queries": len(fixture["queries"]),
        },
        "engine": version,
        "host": host_declaration(),
        "protocol": {
            "offline": True,
            "network_used": False,
            "transport": "native-local",
            "branch": "lexical",
            "candidate_limit": fixture["protocol"]["candidate_limit"],
            "cutoffs": fixture["protocol"]["cutoffs"],
            "warmup_per_query": 1,
            "repetitions": repetitions,
            "timing_clock": "time.perf_counter_ns",
            "models_executed": False,
            "segment_labels_are_provenance_only": True,
        },
        "preparation": {
            "ingest_durability": "strict",
            "ingest_wall_nanos": ingest_nanos,
        },
        "correctness": {
            "foreign_project_hits": domain_foreign + all_foreign,
            "changed_rankings_on_repeat": domain_changed + all_changed,
            "ranking_sha256": ranking_digest,
        },
        "retrieval": {
            "domain_filtered": segmented_metrics(domain_results, fixture["protocol"]["cutoffs"]),
            "all_domains": segmented_metrics(all_results, fixture["protocol"]["cutoffs"]),
        },
        "clock_observations": {
            "client_end_to_end": "reported as independent latency summaries under each retrieval segment",
            "server_telemetry_delta": telemetry_delta(before, after),
            "retrieval_durability": {
                "status": "not-applicable",
                "reason": "lexical recall is read-only; strict ingest is reported under preparation",
            },
            "attribution_note": "histogram deltas are independent aggregates and are not subtracted from client latency",
        },
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument(
        "--fixture", type=Path,
        default=REPO_ROOT / "config" / "agent-memory-lexical-eval-v1.json",
    )
    parser.add_argument(
        "--output", type=Path,
        default=REPO_ROOT / "artifacts-local-agent-memory" / "agent-memory-lexical.receipt.json",
    )
    parser.add_argument("--repetitions", type=int, default=30)
    parser.add_argument("--require-clean-source", action="store_true")
    arguments = parser.parse_args()
    if not arguments.binary.is_file():
        print(f"error: binary is missing: {arguments.binary}", file=sys.stderr)
        return 1
    if not 1 <= arguments.repetitions <= 10_000:
        print("error: repetitions must be within 1..=10000", file=sys.stderr)
        return 1
    try:
        receipt = evaluate(
            arguments.binary.resolve(), arguments.fixture.resolve(),
            arguments.repetitions, arguments.require_clean_source,
        )
    except (EvalError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
