#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run pinned LoCoMo or LongMemEval retrieval through Native local search.

Datasets are caller-supplied, digest-verified external inputs and are never
copied into the repository. The harness executes no model and emits only a
source-bound local diagnostic receipt; it does not authorize publication.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
from datetime import datetime, timezone
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict
from itertools import groupby
from pathlib import Path
from typing import Any, Callable, TextIO

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT))
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python" / "src"))

from hyphae_sdk.v2 import HyphaeClient  # noqa: E402
from tools.agent_memory_retrieval_eval import (  # noqa: E402
    host_declaration,
    latency_summary,
    run_binary,
    sha256_file,
    source_identity,
    start_daemon,
    stop_daemon,
    telemetry_delta,
)

RECEIPT_SCHEMA = "hyphae-long-term-memory-retrieval-receipt-v1"
TRACE_SCHEMA = "hyphae-long-term-memory-query-trace-v2"
TRACE_QUERY_SCHEMA = "hyphae-long-term-memory-query-result-v2"
TRACE_PROTOCOL_SCHEMA = "hyphae-long-term-memory-retrieval-protocol-v2"
CANONICAL_DIGEST = "sha256-canonical-json-utf8-v1"
QREL_MODES = ("audited-v2", "raw-compat")
DEFAULT_QREL_MODE = "audited-v2"
COLLECTION = 13
LOCOMO_COLLECTION_BASE = 21
DOCUMENT_ID_BASE = 1_000_000
MAX_BATCH_DOCUMENTS = 256
MAX_BATCH_LOGICAL_BYTES = 2 * 1024 * 1024
CANDIDATE_LIMIT = 1_000

DATASETS = {
    "locomo": {
        "sha256": "79fa87e90f04081343b8c8debecb80a9a6842b76a7aa537dc9fdf651ea698ff4",
        "upstream_commit": "3eb6f2c585f5e1699204e3c3bdf7adc5c28cb376",
        "upstream": "https://github.com/snap-research/locomo",
        "license": "CC-BY-NC-4.0",
        "cutoffs": [5, 10, 25, 50, 100, 250, 500],
    },
    "longmemeval": {
        "sha256": "d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442",
        "upstream_commit": "98d7416c24c778c2fee6e6f3006e7a073259d48f",
        "upstream": "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned",
        "license": "MIT",
        "cutoffs": [1, 3, 5, 10, 30, 50],
    },
}


def cutoffs_for(benchmark: str, candidate_limit: int) -> list[int]:
    return [cutoff for cutoff in DATASETS[benchmark]["cutoffs"] if cutoff <= candidate_limit]


def locomo_collection_ids(prepared: dict[str, Any]) -> list[int]:
    return sorted(
        collection
        for sample_collections in prepared.get("collections", {}).values()
        for collection in sample_collections.values()
    )


class BenchmarkError(Exception):
    """Fail-closed benchmark adapter failure."""


def load_dataset(path: Path, benchmark: str) -> list[dict[str, Any]]:
    expected = DATASETS[benchmark]["sha256"]
    observed = sha256_file(path)
    if observed != expected:
        raise BenchmarkError(
            f"{benchmark} dataset digest differs: observed {observed}, expected {expected}"
        )
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise BenchmarkError(f"{benchmark} dataset is invalid") from error
    if not isinstance(value, list) or not value or not all(isinstance(row, dict) for row in value):
        raise BenchmarkError(f"{benchmark} dataset must be a nonempty JSON array")
    return value


def memory_document(object_id: int, project: str, text: str, harness: str) -> dict[str, Any]:
    if not text or len(text.encode("utf-8")) > 1_048_576:
        raise BenchmarkError("benchmark document text is empty or above the bounded search input")
    return {
        "object_id": object_id,
        "text": text,
        "doc_values": {
            "project": project,
            "kind": "fact",
            "layer": "work",
            "harness": harness,
            "model": "dataset",
        },
        "vectors": {},
    }


MONTHS = {
    "january": 1, "february": 2, "march": 3, "april": 4, "may": 5, "june": 6,
    "july": 7, "august": 8, "september": 9, "october": 10, "november": 11,
    "december": 12,
}


def parse_locomo_timestamp(timestamp: str) -> int:
    loose = re.match(
        r"(\d{1,2}):(\d{2})\s*(am|pm)?\s*on\s+(\d{1,2})\s+([A-Za-z]+),\s*(\d{4})",
        timestamp,
    )
    if loose is None:
        return 0
    hour = int(loose.group(1))
    minute = int(loose.group(2))
    meridiem = (loose.group(3) or "").lower()
    if meridiem == "pm" and hour != 12:
        hour += 12
    if meridiem == "am" and hour == 12:
        hour = 0
    month = MONTHS.get(loose.group(5).lower(), 1)
    parsed = datetime(int(loose.group(6)), month, int(loose.group(4)), hour, minute)
    return int(parsed.replace(tzinfo=timezone.utc).timestamp() * 1_000_000)


def normalize_locomo_evidence(raw: Any) -> list[str]:
    if not isinstance(raw, str):
        raise BenchmarkError("LoCoMo evidence must be text")
    normalized = raw.replace("D:", "D", 1)
    matches = re.findall(r"D(\d+):(\d+)", normalized)
    if not matches:
        return [f"unresolved:{raw}"]
    return [f"D{int(session)}:{int(turn)}" for session, turn in matches]


def locomo_turn_text(turn: dict[str, Any]) -> str:
    speaker, text = turn.get("speaker"), turn.get("text")
    if not all(isinstance(value, str) and value for value in (speaker, text)):
        raise BenchmarkError("LoCoMo turn fields are invalid")
    body = f'{speaker} said, "{text}"'
    caption = turn.get("blip_caption")
    if isinstance(caption, str) and caption:
        body += f" and shared {caption}"
    return body


def locomo_document_text(
    rendered: list[str], index: int, timestamp: str, view: str
) -> str:
    anchor = rendered[index]
    if view == "bare":
        return anchor
    if view == "timestamp":
        return f"Session date and time: {timestamp}. Evidence turn: {anchor}"
    previous = rendered[index - 1] if index > 0 else "No previous turn."
    if view == "timestamp-previous":
        return (
            f"Session date and time: {timestamp}. Previous turn: {previous} "
            f"Evidence turn: {anchor}"
        )
    if view == "centered":
        following = rendered[index + 1] if index + 1 < len(rendered) else "No following turn."
        return (
            f"Previous turn: {previous} Evidence turn: {anchor} "
            f"Following turn: {following}"
        )
    raise BenchmarkError(f"unsupported LoCoMo document view: {view}")


def prepare_locomo(
    rows: list[dict[str, Any]],
    views: tuple[str, ...] = ("bare",),
    rrf_weights: tuple[int, ...] | None = None,
) -> dict[str, Any]:
    if not views or len(set(views)) != len(views):
        raise BenchmarkError("LoCoMo views must be nonempty and unique")
    weights = rrf_weights or tuple(1 for _ in views)
    if len(weights) != len(views) or any(
        type(weight) is not int or not 1 <= weight <= 1_000 for weight in weights
    ):
        raise BenchmarkError("LoCoMo RRF weights must align with views and be within 1..=1000")
    documents = []
    queries = []
    document_lookup: dict[int, str] = {}
    actual_ids: set[tuple[str, str]] = set()
    next_id = DOCUMENT_ID_BASE
    category_counts: Counter[str] = Counter()
    unresolved_targets: Counter[str] = Counter()
    empty_evidence = 0
    turn_count = 0
    session_count = 0
    sample_ids: set[str] = set()

    collections = {}
    for sample_index, sample in enumerate(rows):
        sample_id = sample.get("sample_id")
        conversation = sample.get("conversation")
        qas = sample.get("qa")
        if (
            not isinstance(sample_id, str)
            or not sample_id
            or sample_id in sample_ids
            or not isinstance(conversation, dict)
            or not isinstance(qas, list)
        ):
            raise BenchmarkError("LoCoMo sample shape is invalid")
        sample_ids.add(sample_id)
        sample_collections = {
            view: LOCOMO_COLLECTION_BASE + sample_index * len(views) + view_index
            for view_index, view in enumerate(views)
        }
        collections[sample_id] = sample_collections
        session_numbers = sorted(
            int(key.removeprefix("session_"))
            for key in conversation
            if re.fullmatch(r"session_\d+", key)
        )
        if not session_numbers:
            raise BenchmarkError(f"LoCoMo sample {sample_id} has no sessions")
        session_count += len(session_numbers)
        for session in session_numbers:
            turns = conversation[f"session_{session}"]
            timestamp = conversation.get(f"session_{session}_date_time")
            if not isinstance(turns, list) or not isinstance(timestamp, str):
                raise BenchmarkError(f"LoCoMo sample {sample_id} session is invalid")
            rendered = [locomo_turn_text(turn) for turn in turns]
            session_micros = parse_locomo_timestamp(timestamp)
            for turn_index, turn in enumerate(turns):
                if not isinstance(turn, dict):
                    raise BenchmarkError("LoCoMo turn is invalid")
                dialog_id = turn.get("dia_id")
                if not isinstance(dialog_id, str) or not dialog_id:
                    raise BenchmarkError("LoCoMo turn fields are invalid")
                actor = turn.get("speaker")
                if not isinstance(actor, str) or not actor:
                    raise BenchmarkError("LoCoMo turn fields are invalid")
                for view, collection in sample_collections.items():
                    body = locomo_document_text(rendered, turn_index, timestamp, view)
                    document = memory_document(next_id, sample_id, body, f"locomo-{view}")
                    document["doc_values"].update(
                        {
                            "session": f"s{session}",
                            "actor": actor,
                            "date_anchor": timestamp,
                            "session_ts": session_micros,
                            "turn_ord": turn_index,
                        }
                    )
                    document["collection"] = collection
                    documents.append(document)
                document_lookup[next_id] = f"{sample_id}/{dialog_id}"
                actual_ids.add((sample_id, dialog_id))
                next_id += 1
                turn_count += 1
        for ordinal, qa in enumerate(qas):
            if not isinstance(qa, dict) or not isinstance(qa.get("question"), str):
                raise BenchmarkError("LoCoMo QA record is invalid")
            category = qa.get("category")
            evidence = qa.get("evidence")
            if category not in {1, 2, 3, 4, 5} or not isinstance(evidence, list):
                raise BenchmarkError("LoCoMo QA category or evidence is invalid")
            category_counts[str(category)] += 1
            targets = [
                f"{sample_id}/{target}"
                for raw in evidence
                for target in normalize_locomo_evidence(raw)
            ]
            if not targets:
                empty_evidence += 1
            for target in targets:
                raw_target = target.removeprefix(f"{sample_id}/")
                if (sample_id, raw_target) not in actual_ids:
                    unresolved_targets[raw_target] += 1
            session_stamps = sorted(
                {
                    parse_locomo_timestamp(
                        str(conversation.get(f"session_{number}_date_time") or "")
                    )
                    for number in session_numbers
                }
            )
            answer = qa.get("answer")
            queries.append(
                {
                    "id": f"{sample_id}:{ordinal}",
                    "sample_id": sample_id,
                    "conversation_id": sample_id,
                    "project": sample_id,
                    "collections": list(sample_collections.values()),
                    "rrf_weights": list(weights),
                    "text": qa["question"],
                    "segment": str(category),
                    "answer": answer if isinstance(answer, (str, int, float, list)) else None,
                    "targets": targets,
                    "slice_b": {
                        "session_window": (
                            [session_stamps[0], session_stamps[-1]]
                            if session_stamps
                            else None
                        ),
                        "session_quota": 2,
                    },
                }
            )
    if len(rows) != 10 or turn_count != 5_882 or len(queries) != 1_986:
        raise BenchmarkError("LoCoMo cardinality differs from the pinned release")
    return {
        "documents": documents,
        "document_lookup": document_lookup,
        "collections": collections,
        "queries": queries,
        "dataset_stats": {
            "samples": len(rows),
            "sessions": session_count,
            "turns": turn_count,
            "indexed_documents": len(documents),
            "questions": len(queries),
            "questions_by_category": dict(sorted(category_counts.items())),
            "empty_evidence_questions": empty_evidence,
            "unresolved_evidence_occurrences": sum(unresolved_targets.values()),
            "unresolved_evidence_values": dict(sorted(unresolved_targets.items())),
        },
        "protocol": {
            "document_granularity": "dialog-turn",
            "document_text_views": list(views),
            "fusion": "weighted-rrf-k60" if len(views) > 1 else "single-view",
            "rrf_weights": list(weights),
            "collection_scope": "one collection per conversation",
            "identity_scope": "sample_id/dialog_id",
            "query_denominator": "questions with at least one evidence value",
            "primary_metric": "mean fractional evidence recall",
        },
    }


def prepare_longmemeval(rows: list[dict[str, Any]]) -> dict[str, Any]:
    queries = []
    question_ids: set[str] = set()
    type_counts: Counter[str] = Counter()
    skipped_abstention = 0
    skipped_no_user_target = 0
    sessions = 0
    user_turns = 0
    duplicate_session_occurrences = 0

    for entry in rows:
        required = (
            "question_id", "question_type", "question", "answer_session_ids",
            "haystack_session_ids", "haystack_dates", "haystack_sessions",
        )
        if any(key not in entry for key in required):
            raise BenchmarkError("LongMemEval record is missing required fields")
        question_id = entry["question_id"]
        question_type = entry["question_type"]
        if (
            not isinstance(question_id, str)
            or not question_id
            or question_id in question_ids
            or not isinstance(question_type, str)
            or not isinstance(entry["question"], str)
        ):
            raise BenchmarkError("LongMemEval question identity is invalid")
        question_ids.add(question_id)
        type_counts[question_type] += 1
        session_ids = entry["haystack_session_ids"]
        dates = entry["haystack_dates"]
        history = entry["haystack_sessions"]
        if (
            not isinstance(session_ids, list)
            or not isinstance(dates, list)
            or not isinstance(history, list)
            or not (len(session_ids) == len(dates) == len(history))
        ):
            raise BenchmarkError("LongMemEval history arrays are inconsistent")
        duplicate_session_occurrences += len(session_ids) - len(set(session_ids))
        logical_ids = []
        session_documents = []
        document_lookup = {}
        has_positive_user_target = False
        for occurrence, (session_id, date, turns) in enumerate(
            zip(session_ids, dates, history), start=1
        ):
            if not isinstance(session_id, str) or not isinstance(date, str) or not isinstance(turns, list):
                raise BenchmarkError("LongMemEval session is invalid")
            user_contents = []
            user_positive = False
            for turn in turns:
                if not isinstance(turn, dict) or turn.get("role") not in {"user", "assistant"}:
                    raise BenchmarkError("LongMemEval turn role is invalid")
                content = turn.get("content")
                if not isinstance(content, str):
                    raise BenchmarkError("LongMemEval turn content is invalid")
                if turn["role"] == "user":
                    user_turns += 1
                    user_contents.append(content)
                    user_positive = user_positive or turn.get("has_answer") is True
            has_positive_user_target = has_positive_user_target or user_positive
            logical_id = session_id
            if "answer" in logical_id and not user_positive:
                logical_id = logical_id.replace("answer", "noans")
            text = " ".join(user_contents)
            if not text:
                text = "[no user content]"
            object_id = DOCUMENT_ID_BASE + occurrence
            session_documents.append(
                memory_document(
                    object_id, question_id, text, "longmemeval-session-user-only"
                )
            )
            document_lookup[object_id] = logical_id
            logical_ids.append(logical_id)
            sessions += 1
        targets = sorted(logical_id for logical_id in logical_ids if "answer" in logical_id)
        excluded = None
        if question_id.endswith("_abs"):
            skipped_abstention += 1
            excluded = "abstention"
        elif not has_positive_user_target or not targets:
            skipped_no_user_target += 1
            excluded = "no-positive-user-target"
        queries.append(
            {
                "id": question_id,
                "sample_id": question_id,
                "conversation_id": question_id,
                "text": entry["question"],
                "segment": question_type,
                "targets": targets,
                "excluded": excluded,
                "corpus_ids": logical_ids,
                "documents": session_documents,
                "document_lookup": document_lookup,
            }
        )
    if len(rows) != 500 or sessions != 23_867 or user_turns != 122_416:
        raise BenchmarkError("LongMemEval cardinality differs from the pinned cleaned S release")
    return {
        "documents": [],
        "document_lookup": {},
        "queries": queries,
        "dataset_stats": {
            "questions": len(rows),
            "questions_by_type": dict(sorted(type_counts.items())),
            "sessions": sessions,
            "user_turns": user_turns,
            "abstention_questions_excluded": skipped_abstention,
            "questions_without_positive_user_target_excluded": skipped_no_user_target,
            "evaluated_questions": len(rows) - skipped_abstention - skipped_no_user_target,
            "duplicate_session_occurrences": duplicate_session_occurrences,
        },
        "protocol": {
            "document_granularity": "session",
            "document_text": "official flat-bm25 user-only session concatenation",
            "storage_scope": "one disposable per-question corpus, matching the released baseline",
            "identity_scope": "question_id/session_id",
            "query_denominator": "official non-abstention questions with positive user evidence",
            "primary_metrics": "official recall_all and ndcg_any",
        },
    }


def provision(
    binary: Path,
    data_dir: Path,
    benchmark: str,
    locomo_collections: list[int] | None = None,
    english_stop: bool = False,
    english_stem: bool = False,
    bm25_k1_micros: int | None = None,
    bm25_b_micros: int | None = None,
) -> None:
    run_binary(binary, ["init", "--data-dir", str(data_dir)])
    collections = locomo_collections or [COLLECTION]
    for ordinal, collection in enumerate(collections):
        arguments = [
            "catalog", "--data-dir", str(data_dir), "create-search-collection",
            "--database", "10", "--schema", "11", "--collection", str(collection),
            "--analyzer", "12", "--name", f"main.public.{benchmark}_retrieval_{collection}",
            "--memory-schema",
        ]
        if ordinal > 0:
            arguments.append("--reuse-schema")
        if ordinal == 0 and english_stop:
            arguments.append("--analyzer-english-stop")
        if ordinal == 0 and english_stem:
            arguments.append("--analyzer-english-stem")
        if bm25_k1_micros is not None and bm25_b_micros is not None:
            arguments.extend(
                [
                    "--bm25-k1-micros", str(bm25_k1_micros),
                    "--bm25-b-micros", str(bm25_b_micros),
                ]
            )
        run_binary(binary, arguments)
    for collection in collections:
        run_binary(
            binary,
            ["search", "--data-dir", str(data_dir), "provision", "--collection", str(collection)],
        )


def reset_longmemeval_collection(
    client: HyphaeClient, active_ids: list[int], query_ordinal: int
) -> None:
    for object_id in active_ids:
        client.search_document_delete(
            COLLECTION,
            10_000_000_000 + query_ordinal * 1_000 + object_id - DOCUMENT_ID_BASE,
            object_id,
        )


def document_bytes(document: dict[str, Any]) -> int:
    return len(document["text"].encode("utf-8")) + sum(
        len(name.encode("utf-8")) + len(str(value).encode("utf-8"))
        for name, value in document["doc_values"].items()
    ) + 128


def ingest(
    client: HyphaeClient,
    documents: list[dict[str, Any]],
    collection: int = COLLECTION,
    idempotency_base: int = 0,
) -> int:
    batch = []
    batch_bytes = 0
    batch_id = 1
    commits = 0
    for document in documents:
        size = document_bytes(document)
        if size > MAX_BATCH_LOGICAL_BYTES:
            raise BenchmarkError("one benchmark document exceeds the local ingestion budget")
        if batch and (
            len(batch) == MAX_BATCH_DOCUMENTS
            or batch_bytes + size > MAX_BATCH_LOGICAL_BYTES
        ):
            client.search_ingest(
                collection,
                {"idempotency_id": idempotency_base + batch_id, "documents": batch},
            )
            commits += 1
            batch_id += 1
            batch, batch_bytes = [], 0
        batch.append({key: value for key, value in document.items() if key != "collection"})
        batch_bytes += size
    if batch:
        client.search_ingest(
            collection,
            {"idempotency_id": idempotency_base + batch_id, "documents": batch},
        )
        commits += 1
    return commits


def execute_collection_query(
    client: HyphaeClient,
    collection: int,
    query_text: str,
    limit: int,
    candidate_limit: int = CANDIDATE_LIMIT,
    options: dict[str, Any] | None = None,
) -> list[dict[str, Any]]:
    options = options or {}
    clauses: list[dict[str, Any]] = []
    if options.get("actor"):
        clauses.append(
            {
                "kind": "compare",
                "field": "actor",
                "operator": "equal",
                "value": options["actor"],
            }
        )
    if options.get("session_window") is not None:
        lower, upper = options["session_window"]
        clauses.append(
            {
                "kind": "compare",
                "field": "session_ts",
                "operator": "greater_or_equal",
                "value": lower,
            }
        )
        clauses.append(
            {
                "kind": "compare",
                "field": "session_ts",
                "operator": "less_or_equal",
                "value": upper,
            }
        )
    if options.get("sessions"):
        clauses.append(
            {
                "kind": "in",
                "field": "session",
                "values": list(options["sessions"]),
            }
        )
    request: dict[str, Any] = {
        "lexical": {
            "query": query_text,
            "candidate_limit": candidate_limit,
            "weight": 1,
        },
        "vectors": [],
        "filter": {"kind": "all", "filters": clauses} if clauses else {"kind": "match_all"},
        "limit": limit,
    }
    if options.get("session_quota"):
        request["parent_dedupe"] = {
            "field": "session",
            "first_k": options["session_quota"],
        }
    if options.get("highlight"):
        request["highlight"] = {
            "max_fragments": options["highlight"][0],
            "fragment_bytes": options["highlight"][1],
        }
    response = client.search_collection(collection, request)
    return response.value.get("hits", [])


def fuse_rrf_hits(
    branches: list[list[dict[str, Any]]],
    limit: int,
    weights: list[int] | tuple[int, ...] | None = None,
    session_values: list[dict[int, str]] | None = None,
    session_cover: int = 0,
) -> list[int]:
    branch_weights = list(weights or [1] * len(branches))
    if len(branch_weights) != len(branches) or any(
        type(weight) is not int or not 1 <= weight <= 1_000
        for weight in branch_weights
    ):
        raise BenchmarkError("RRF weights must align with branches and be within 1..=1000")
    if session_values is not None and len(session_values) != len(branches):
        raise BenchmarkError("session values must align with branches")
    if session_cover < 0 or session_cover > 8:
        raise BenchmarkError("session cover budget must be within 0..=8")
    # Fixed-point arithmetic makes ranking independent of floating-point
    # reduction order while retaining the literature-standard k=60 RRF form.
    fused: defaultdict[int, int] = defaultdict(int)
    for hits, weight in zip(branches, branch_weights):
        for rank, hit in enumerate(hits, start=1):
            fused[int(hit["object_id"])] += (1_000_000_000 * weight) // (60 + rank)
    ranking = [
        object_id
        for object_id, _ in sorted(fused.items(), key=lambda item: (-item[1], item[0]))
    ]
    if session_cover and session_values:
        distinct: list[str] = []
        for values in session_values:
            for session in values.values():
                if session not in distinct:
                    distinct.append(session)
        if len(distinct) > 1:
            covered: set[str] = set()
            head: list[int] = []
            tail: list[int] = []
            for object_id in ranking:
                session = next(
                    (
                        values[object_id]
                        for values in session_values
                        if object_id in values
                    ),
                    "",
                )
                if session and len(covered) < session_cover and session not in covered:
                    covered.add(session)
                    head.append(object_id)
                else:
                    tail.append(object_id)
            ranking = head + tail
    return ranking[:limit]


def execute_query_with_hits(
    client: HyphaeClient,
    query: dict[str, Any],
    limit: int,
    candidate_limit: int = CANDIDATE_LIMIT,
) -> list[tuple[int, str]]:
    highlight = (4, 512)
    collections = query.get("collections")
    if not collections:
        hits = execute_collection_query(
            client,
            query.get("collection", COLLECTION),
            query["text"],
            limit,
            candidate_limit,
            {"highlight": highlight},
        )
        return [
            (int(hit["object_id"]), " ".join(hit.get("fragments") or []))
            for hit in hits
        ]
    branches = [
        execute_collection_query(
            client,
            collection,
            query["text"],
            candidate_limit,
            candidate_limit,
            {"highlight": highlight},
        )
        for collection in collections
    ]
    weights = list(query.get("rrf_weights") or [1] * len(collections))
    texts: dict[int, str] = {}
    for hits in branches:
        for hit in hits:
            texts[int(hit["object_id"])] = " ".join(hit.get("fragments") or [])
    fused = fuse_rrf_hits(branches, limit, weights)
    return [(object_id, texts.get(object_id, "")) for object_id in fused]


def session_lookup_from_hits(hits: list[dict[str, Any]]) -> dict[int, str]:
    lookup: dict[int, str] = {}
    for hit in hits:
        doc_values = hit.get("doc_values") or {}
        session = doc_values.get("session")
        if isinstance(session, str) and session:
            lookup[int(hit["object_id"])] = session
    return lookup


def execute_query(
    client: HyphaeClient,
    query: dict[str, Any],
    limit: int,
    candidate_limit: int = CANDIDATE_LIMIT,
    slice_b: bool = False,
    slice_b_quota_only: bool = False,
    session_cover: int = 0,
) -> list[int]:
    collections = query.get("collections")
    if not collections:
        return [
            int(hit["object_id"])
            for hit in execute_collection_query(
                client, query.get("collection", COLLECTION), query["text"], limit, candidate_limit
            )
        ]
    branches = [
        execute_collection_query(client, collection, query["text"], candidate_limit, candidate_limit)
        for collection in collections
    ]
    session_values: list[dict[int, str]] | None = (
        [session_lookup_from_hits(hits) for hits in branches] if session_cover else None
    )
    if slice_b and query.get("slice_b"):
        context = query["slice_b"]
        if (
            not slice_b_quota_only
            and context.get("session_window") is not None
        ):
            branches.append(
                execute_collection_query(
                    client,
                    collections[0],
                    query["text"],
                    candidate_limit,
                    candidate_limit,
                    {"session_window": context["session_window"]},
                )
            )
        if context.get("session_quota"):
            branches.append(
                execute_collection_query(
                    client,
                    collections[0],
                    query["text"],
                    candidate_limit,
                    candidate_limit,
                    {"session_quota": context["session_quota"]},
                )
            )
    weights = list(query.get("rrf_weights") or [1] * len(collections))
    while len(weights) < len(branches):
        weights.append(1)
    return fuse_rrf_hits(
        branches, limit, weights, session_values, session_cover,
    )


def binary_ndcg(ranking: list[str], targets: list[str], k: int) -> float:
    target_set = set(targets)
    gains = [1 if item in target_set else 0 for item in ranking[:k]]
    dcg = sum(gain / math.log2(position + 2) for position, gain in enumerate(gains))
    ideal = [1] * min(len(target_set), k)
    idcg = sum(gain / math.log2(position + 2) for position, gain in enumerate(ideal))
    return dcg / idcg if idcg else 0.0


def official_longmemeval_ndcg(
    ranked_ids: list[str],
    targets: list[str],
    corpus_ids: list[str],
    k: int,
    qrel_mode: str = "raw-compat",
) -> float:
    target_set = set(targets)
    if qrel_mode == "audited-v2":
        seen_ranked: set[str] = set()
        sorted_relevance = []
        for item in ranked_ids[:k]:
            sorted_relevance.append(
                1 if item in target_set and item not in seen_ranked else 0
            )
            seen_ranked.add(item)
        ideal = [1] * min(len(target_set), k)
    elif qrel_mode == "raw-compat":
        sorted_relevance = [1 if item in target_set else 0 for item in ranked_ids[:k]]
        ideal = sorted(
            (1 if item in target_set else 0 for item in corpus_ids), reverse=True
        )[:k]
    else:
        raise BenchmarkError(f"unsupported qrel mode: {qrel_mode}")

    def dcg(values: list[int]) -> float:
        if not values:
            return 0.0
        return float(values[0]) + sum(
            value / math.log2(position) for position, value in enumerate(values[1:], start=2)
        )

    ideal_dcg = dcg(ideal)
    return dcg(sorted_relevance) / ideal_dcg if ideal_dcg else 0.0


def scored_targets(targets: list[str], qrel_mode: str) -> list[str]:
    if qrel_mode not in QREL_MODES:
        raise BenchmarkError(f"unsupported qrel mode: {qrel_mode}")
    if qrel_mode == "raw-compat":
        return list(targets)
    return list(dict.fromkeys(targets))


def locomo_query_metrics(
    ranking: list[str],
    query: dict[str, Any],
    cutoffs: list[int],
    qrel_mode: str = DEFAULT_QREL_MODE,
) -> dict[str, float]:
    targets = scored_targets(query["targets"], qrel_mode)
    target_set = set(targets)
    metrics = {}
    for cutoff in cutoffs:
        recalled = set(ranking[:cutoff])
        hits = sum(1 for target in targets if target in recalled)
        metrics[f"evidence_recall@{cutoff}"] = hits / len(targets)
        metrics[f"recall_any@{cutoff}"] = float(bool(target_set.intersection(recalled)))
        metrics[f"recall_all@{cutoff}"] = float(target_set.issubset(recalled))
        metrics[f"ndcg@{cutoff}"] = binary_ndcg(ranking, targets, cutoff)
    metrics["mrr@50"] = next(
        (1.0 / rank for rank, item in enumerate(ranking[:50], start=1) if item in target_set),
        0.0,
    )
    return metrics


def longmemeval_query_metrics(
    ranking: list[str],
    query: dict[str, Any],
    cutoffs: list[int],
    qrel_mode: str = DEFAULT_QREL_MODE,
) -> dict[str, float]:
    targets = scored_targets(query["targets"], qrel_mode)
    target_set = set(targets)
    metrics = {}
    for cutoff in cutoffs:
        recalled = set(ranking[:cutoff])
        metrics[f"recall_any@{cutoff}"] = float(bool(target_set.intersection(recalled)))
        metrics[f"recall_all@{cutoff}"] = float(target_set.issubset(recalled))
        metrics[f"ndcg_any@{cutoff}"] = official_longmemeval_ndcg(
            ranking, targets, query["corpus_ids"], cutoff, qrel_mode
        )
    return metrics


def aggregate(results: list[dict[str, Any]]) -> dict[str, Any]:
    if not results:
        raise BenchmarkError("benchmark metric denominator is empty")
    names = set(results[0]["metric_contributions"])
    if any(set(result["metric_contributions"]) != names for result in results):
        raise BenchmarkError("benchmark results do not share one metric schema")
    output = {
        "query_count": len(results),
        "latency": latency_summary(
            [sample for result in results for sample in result["latency"]["samples"]]
        ),
    }
    if any(
        result["ranking_sha256"]
        != query_ranking_sha256(result["id"], result["logical_ranking"])
        for result in results
    ):
        raise BenchmarkError("benchmark result ranking digest differs")
    for name in sorted(names):
        output[name] = round(
            sum(result["metric_contributions"][name] for result in results) / len(results),
            6,
        )
    return output


def aggregate_results(results: list[dict[str, Any]]) -> dict[str, Any]:
    groups: defaultdict[str, list[dict[str, Any]]] = defaultdict(list)
    for result in results:
        groups[result["segment"]].append(result)
    return {
        "overall": aggregate(results),
        "by_segment": {key: aggregate(value) for key, value in sorted(groups.items())},
    }


def json_safe(value: Any) -> bool:
    if value is None or type(value) in (bool, int, str):
        return True
    if type(value) is float:
        return math.isfinite(value)
    if isinstance(value, list):
        return all(json_safe(item) for item in value)
    if isinstance(value, dict):
        return all(isinstance(key, str) and json_safe(item) for key, item in value.items())
    return False


def metric_names_for(benchmark: str, cutoffs: list[int]) -> list[str]:
    names = {
        name
        for cutoff in cutoffs
        for name in (
            (
                f"evidence_recall@{cutoff}", f"recall_any@{cutoff}",
                f"recall_all@{cutoff}", f"ndcg@{cutoff}",
            )
            if benchmark == "locomo"
            else (
                f"recall_any@{cutoff}", f"recall_all@{cutoff}",
                f"ndcg_any@{cutoff}",
            )
        )
    }
    if benchmark == "locomo":
        names.add("mrr@50")
    return sorted(names)


def canonical_sha256(value: Any) -> str:
    encoded = json.dumps(value, separators=(",", ":"), sort_keys=True).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def query_ranking_sha256(query_id: str, ranking: list[str]) -> str:
    return canonical_sha256({"query_id": query_id, "logical_ranking": ranking})


def combined_ranking_sha256(results: list[dict[str, Any]]) -> str:
    return canonical_sha256(
        [
            {
                "query_id": result["id"],
                "ranking_sha256": result["ranking_sha256"],
            }
            for result in results
        ]
    )


def reject_json_constant(value: str) -> Any:
    raise ValueError(f"invalid JSON constant {value}")


def reject_duplicate_json_fields(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON field: {key}")
        value[key] = item
    return value


def trace_metadata(
    prepared: dict[str, Any],
    benchmark: str,
    repetitions: int,
    candidate_limit: int,
    qrel_mode: str,
    context: dict[str, Any] | None = None,
) -> dict[str, Any]:
    if qrel_mode not in QREL_MODES:
        raise BenchmarkError(f"unsupported qrel mode: {qrel_mode}")
    execution_context = context or {"mode": "unspecified-local-execution"}
    if not isinstance(execution_context, dict) or not json_safe(execution_context):
        raise BenchmarkError("trace execution context must be a finite JSON object")
    queries = prepared["queries"]
    eligible_source_ordinals = [
        ordinal
        for ordinal, query in enumerate(queries)
        if query.get("excluded") is None
        and not (benchmark == "locomo" and not query["targets"])
    ]
    protocol = {
        "schema": TRACE_PROTOCOL_SCHEMA,
        "benchmark": benchmark,
        "dataset_sha256": DATASETS[benchmark]["sha256"],
        "dataset_stats": prepared.get("dataset_stats", {}),
        "source_ordinal": "zero-based position in the pinned dataset query array",
        "query_population_total": len(queries),
        "query_population_evaluated": len(eligible_source_ordinals),
        "eligible_source_ordinals": eligible_source_ordinals,
        "candidate_limit": candidate_limit,
        "cutoffs": cutoffs_for(benchmark, candidate_limit),
        "metric_names": metric_names_for(
            benchmark, cutoffs_for(benchmark, candidate_limit)
        ),
        "warmup_per_query": 1,
        "timed_repetitions": repetitions,
        "logical_ranking_scope": "all returned hits up to candidate_limit",
        "ranking_order": "engine order; weighted RRF score then object ID for multi-view LoCoMo",
        "ranking_identity": prepared["protocol"].get("identity_scope", "session-id"),
        "ranking_digest": CANONICAL_DIGEST,
        "metric_contributions": "unrounded per-query values; aggregate is arithmetic mean",
        "qrel_mode": qrel_mode,
        "qrel_semantics": (
            "stable first-occurrence deduplication before scoring"
            if qrel_mode == "audited-v2"
            else "raw target occurrences retained for compatibility"
        ),
        "latency_clock": "time.perf_counter_ns",
        "latency_unit": "nanoseconds",
        "latency_scope": "client end-to-end timed retrieval; warmup excluded",
        "omitted_dataset_fields": ["answer_text", "query_text", "document_text"],
        "benchmark_protocol": prepared["protocol"],
        "execution_context": execution_context,
    }
    return {
        "record_type": "metadata",
        "schema": TRACE_SCHEMA,
        "query_record_schema": TRACE_QUERY_SCHEMA,
        "digest_canonicalization": CANONICAL_DIGEST,
        "protocol": protocol,
        "protocol_sha256": canonical_sha256(protocol),
    }


def validate_trace_metadata(metadata: Any, path: Path) -> None:
    if not isinstance(metadata, dict) or set(metadata) != {
        "record_type", "schema", "query_record_schema", "digest_canonicalization",
        "protocol", "protocol_sha256",
    }:
        raise BenchmarkError(f"progress trace {path} has incompatible metadata")
    protocol = metadata["protocol"]
    if not isinstance(protocol, dict) or set(protocol) != {
        "schema", "benchmark", "dataset_sha256", "dataset_stats", "source_ordinal",
        "query_population_total", "query_population_evaluated",
        "eligible_source_ordinals", "candidate_limit", "cutoffs", "metric_names",
        "warmup_per_query", "timed_repetitions", "logical_ranking_scope",
        "ranking_order", "ranking_identity", "ranking_digest",
        "metric_contributions", "qrel_mode", "qrel_semantics", "latency_clock",
        "latency_unit", "latency_scope", "omitted_dataset_fields", "benchmark_protocol",
        "execution_context",
    }:
        raise BenchmarkError(f"progress trace {path} has incompatible protocol metadata")
    benchmark = protocol["benchmark"]
    if (
        metadata["record_type"] != "metadata"
        or metadata["schema"] != TRACE_SCHEMA
        or metadata["query_record_schema"] != TRACE_QUERY_SCHEMA
        or metadata["digest_canonicalization"] != CANONICAL_DIGEST
        or protocol["schema"] != TRACE_PROTOCOL_SCHEMA
        or benchmark not in DATASETS
        or protocol["dataset_sha256"] != DATASETS[benchmark]["sha256"]
        or not isinstance(protocol["dataset_stats"], dict)
        or protocol["source_ordinal"]
        != "zero-based position in the pinned dataset query array"
        or type(protocol["query_population_total"]) is not int
        or type(protocol["query_population_evaluated"]) is not int
        or not 0 < protocol["query_population_evaluated"]
        <= protocol["query_population_total"]
        or not isinstance(protocol["eligible_source_ordinals"], list)
        or any(
            type(ordinal) is not int
            or not 0 <= ordinal < protocol["query_population_total"]
            for ordinal in protocol["eligible_source_ordinals"]
        )
        or protocol["eligible_source_ordinals"]
        != sorted(set(protocol["eligible_source_ordinals"]))
        or len(protocol["eligible_source_ordinals"])
        != protocol["query_population_evaluated"]
        or type(protocol["candidate_limit"]) is not int
        or protocol["candidate_limit"] < 1
        or protocol["cutoffs"] != cutoffs_for(benchmark, protocol["candidate_limit"])
        or protocol["metric_names"] != metric_names_for(benchmark, protocol["cutoffs"])
        or protocol["warmup_per_query"] != 1
        or type(protocol["timed_repetitions"]) is not int
        or protocol["timed_repetitions"] < 1
        or protocol["qrel_mode"] not in QREL_MODES
        or protocol["ranking_digest"] != CANONICAL_DIGEST
        or protocol["latency_clock"] != "time.perf_counter_ns"
        or protocol["latency_unit"] != "nanoseconds"
        or protocol["omitted_dataset_fields"]
        != ["answer_text", "query_text", "document_text"]
        or not isinstance(protocol["benchmark_protocol"], dict)
        or not isinstance(protocol["execution_context"], dict)
        or not json_safe(protocol)
        or metadata["protocol_sha256"] != canonical_sha256(protocol)
    ):
        raise BenchmarkError(f"progress trace {path} has incompatible metadata")


def validate_query_trace_record(
    record: Any,
    metadata: dict[str, Any],
    prepared: dict[str, Any] | None = None,
) -> None:
    if not isinstance(record, dict) or set(record) != {
        "record_type", "schema", "protocol_sha256", "source_ordinal", "result"
    }:
        raise BenchmarkError("progress trace contains an invalid query record envelope")
    if (
        record["record_type"] != "query"
        or record["schema"] != TRACE_QUERY_SCHEMA
        or record["protocol_sha256"] != metadata["protocol_sha256"]
        or type(record["source_ordinal"]) is not int
        or record["source_ordinal"] not in metadata["protocol"]["eligible_source_ordinals"]
    ):
        raise BenchmarkError("progress trace query record is incompatible")
    result = record["result"]
    if not isinstance(result, dict) or set(result) != {
        "id", "sample_id", "conversation_id", "segment", "expected_targets",
        "scored_targets", "logical_ranking", "metric_contributions", "latency",
        "changed_rankings", "ranking_sha256",
    }:
        raise BenchmarkError("progress trace result has an invalid schema")
    if any("answer" in key.lower() or "text" in key.lower() for key in result):
        raise BenchmarkError("progress trace result contains a forbidden text field")
    string_fields = ("id", "sample_id", "conversation_id", "segment", "ranking_sha256")
    if any(not isinstance(result[field], str) for field in string_fields):
        raise BenchmarkError("progress trace result identity is invalid")
    sequence_fields = ("expected_targets", "scored_targets", "logical_ranking")
    if any(
        not isinstance(result[field], list)
        or not all(isinstance(value, str) for value in result[field])
        for field in sequence_fields
    ):
        raise BenchmarkError("progress trace targets or ranking are invalid")
    protocol = metadata["protocol"]
    if (
        not result["id"]
        or not result["sample_id"]
        or not result["conversation_id"]
        or result["scored_targets"]
        != scored_targets(result["expected_targets"], protocol["qrel_mode"])
    ):
        raise BenchmarkError("progress trace target scoring mode differs")
    if len(result["logical_ranking"]) > protocol["candidate_limit"]:
        raise BenchmarkError("progress trace ranking exceeds the candidate limit")
    if result["ranking_sha256"] != query_ranking_sha256(
        result["id"], result["logical_ranking"]
    ):
        raise BenchmarkError("progress trace ranking digest differs")
    metrics = result["metric_contributions"]
    if (
        not isinstance(metrics, dict)
        or not metrics
        or any(
            not isinstance(name, str)
            or type(value) not in (int, float)
            or not math.isfinite(value)
            or not 0.0 <= value <= 1.0
            for name, value in metrics.items()
        )
    ):
        raise BenchmarkError("progress trace metric contributions are invalid")
    if sorted(metrics) != metric_names_for(protocol["benchmark"], protocol["cutoffs"]):
        raise BenchmarkError("progress trace metric contribution names differ")
    latency = result["latency"]
    if not isinstance(latency, dict) or set(latency) != {
        "clock", "unit", "warmup_excluded", "samples", "summary"
    }:
        raise BenchmarkError("progress trace latency is invalid")
    samples = latency["samples"]
    if (
        latency["clock"] != protocol["latency_clock"]
        or latency["unit"] != protocol["latency_unit"]
        or latency["warmup_excluded"] is not True
        or not isinstance(samples, list)
        or len(samples) != protocol["timed_repetitions"]
        or any(type(value) is not int or value <= 0 for value in samples)
        or latency["summary"] != latency_summary(samples)
    ):
        raise BenchmarkError("progress trace latency samples are incompatible")
    changed = result["changed_rankings"]
    if type(changed) is not int or not 0 <= changed <= len(samples):
        raise BenchmarkError("progress trace changed-ranking count is invalid")
    if prepared is not None:
        ordinal = record["source_ordinal"]
        queries = prepared["queries"]
        if ordinal >= len(queries):
            raise BenchmarkError("progress trace source ordinal is out of range")
        query = queries[ordinal]
        if (
            protocol["query_population_total"] != len(queries)
            or protocol["dataset_stats"] != prepared.get("dataset_stats", {})
            or protocol["benchmark_protocol"] != prepared["protocol"]
        ):
            raise BenchmarkError("progress trace dataset protocol metadata differs")
        eligible_ordinals = [
            index
            for index, candidate in enumerate(queries)
            if candidate.get("excluded") is None
            and not (
                protocol["benchmark"] == "locomo" and not candidate["targets"]
            )
        ]
        if protocol["eligible_source_ordinals"] != eligible_ordinals:
            raise BenchmarkError("progress trace eligible source ordinals differ")
        expected_context = trace_metadata(
            prepared,
            protocol["benchmark"],
            protocol["timed_repetitions"],
            protocol["candidate_limit"],
            protocol["qrel_mode"],
            protocol["execution_context"],
        )
        if expected_context != metadata:
            raise BenchmarkError("progress trace metadata differs from the dataset")
        if query.get("excluded") is not None or (
            not query["targets"] and protocol["benchmark"] == "locomo"
        ):
            raise BenchmarkError("progress trace contains an excluded source ordinal")
        expected = {
            "id": query["id"],
            "sample_id": query["sample_id"],
            "conversation_id": query["conversation_id"],
            "segment": query["segment"],
            "expected_targets": query["targets"],
            "scored_targets": scored_targets(query["targets"], protocol["qrel_mode"]),
        }
        if any(result[name] != value for name, value in expected.items()):
            raise BenchmarkError("progress trace query identity or targets differ from the dataset")
        ranking = result["logical_ranking"]
        known_ids = (
            set(query["corpus_ids"])
            if protocol["benchmark"] == "longmemeval"
            else set(prepared["document_lookup"].values())
        )
        if any(item not in known_ids for item in ranking):
            raise BenchmarkError("progress trace ranking references an unknown logical ID")
        metric_function = (
            locomo_query_metrics
            if protocol["benchmark"] == "locomo"
            else longmemeval_query_metrics
        )
        recomputed_metrics = metric_function(
            ranking, query, protocol["cutoffs"], protocol["qrel_mode"]
        )
        if result["metric_contributions"] != recomputed_metrics:
            raise BenchmarkError("progress trace metric contributions differ")


def read_trace(
    handle: TextIO,
    path: Path,
    expected_metadata: dict[str, Any] | None = None,
    prepared: dict[str, Any] | None = None,
) -> tuple[dict[str, Any] | None, list[dict[str, Any]]]:
    handle.seek(0)
    try:
        lines = handle.readlines()
    except UnicodeError as error:
        raise BenchmarkError(f"progress trace {path} is not valid UTF-8") from error
    if not lines:
        return None, []
    records = []
    for line_number, line in enumerate(lines, start=1):
        if not line.endswith("\n"):
            raise BenchmarkError(f"progress trace {path} has a partial final record")
        try:
            records.append(
                json.loads(
                    line,
                    object_pairs_hook=reject_duplicate_json_fields,
                    parse_constant=reject_json_constant,
                )
            )
        except (json.JSONDecodeError, ValueError) as error:
            raise BenchmarkError(
                f"progress trace {path} has invalid JSON on line {line_number}"
            ) from error
    metadata = records[0]
    validate_trace_metadata(metadata, path)
    if expected_metadata is not None and metadata != expected_metadata:
        raise BenchmarkError(f"progress trace {path} protocol metadata differs")
    seen_ordinals: set[int] = set()
    seen_query_ids: set[str] = set()
    query_records = records[1:]
    for record in query_records:
        validate_query_trace_record(record, metadata, prepared)
        ordinal = record["source_ordinal"]
        if ordinal in seen_ordinals:
            raise BenchmarkError(f"progress trace {path} contains duplicate source ordinals")
        seen_ordinals.add(ordinal)
        query_id = record["result"]["id"]
        if query_id in seen_query_ids:
            raise BenchmarkError(f"progress trace {path} contains duplicate query IDs")
        seen_query_ids.add(query_id)
    return metadata, query_records


def open_trace(
    path: Path,
    metadata: dict[str, Any],
    prepared: dict[str, Any],
    selected_ordinals: set[int],
) -> tuple[TextIO, set[int]]:
    handle = path.open("a+", encoding="utf-8")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        existing_metadata, records = read_trace(handle, path, metadata, prepared)
        existing_ordinals = {record["source_ordinal"] for record in records}
        duplicates = existing_ordinals.intersection(selected_ordinals)
        if duplicates:
            rendered = ", ".join(str(value) for value in sorted(duplicates)[:10])
            raise BenchmarkError(
                f"progress trace {path} already contains source ordinal(s): {rendered}"
            )
        if existing_metadata is None:
            handle.write(json.dumps(metadata, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        return handle, existing_ordinals
    except Exception:
        handle.close()
        raise


def append_trace_record(handle: TextIO, record: dict[str, Any]) -> None:
    handle.write(json.dumps(record, sort_keys=True) + "\n")
    handle.flush()
    os.fsync(handle.fileno())


def locomo_corpora(rows: list[dict[str, Any]]) -> dict[str, list[str]]:
    """Returns the plain per-turn corpora used by the comparable systems.

    Full-context and chunked-RAG answer through the identical extractive
    reader over these exact texts, so every system shares one reader axis.
    """
    corpora: dict[str, list[str]] = {}
    for sample in rows:
        sample_id = sample.get("sample_id")
        conversation = sample.get("conversation")
        if not isinstance(sample_id, str) or not isinstance(conversation, dict):
            raise BenchmarkError("LoCoMo sample shape is invalid")
        texts: list[str] = []
        for key in sorted(
            key for key in conversation if re.fullmatch(r"session_\d+", key)
        ):
            turns = conversation[key]
            if not isinstance(turns, list):
                raise BenchmarkError(f"LoCoMo sample {sample_id} session is invalid")
            texts.extend(locomo_turn_text(turn) for turn in turns if isinstance(turn, dict))
        corpora[sample_id] = texts
    return corpora


def bm25_rank_documents(
    query_text: str,
    documents: list[str],
    limit: int,
    k1: float = 0.8,
    b: float = 0.4,
) -> list[int]:
    """Deterministic flat BM25 over one conversation corpus."""
    query_terms = [
        token
        for token in re.findall(r"[a-z0-9']+", query_text.lower())
    ]
    if not query_terms:
        return list(range(min(limit, len(documents))))
    tokenized = [
        re.findall(r"[a-z0-9']+", document.lower()) for document in documents
    ]
    counts = [Counter(tokens) for tokens in tokenized]
    lengths = [len(tokens) for tokens in tokenized]
    average = sum(lengths) / len(lengths) if lengths else 1.0
    docs_with_term: dict[str, int] = {}
    for token in set(query_terms):
        docs_with_term[token] = sum(1 for count in counts if count.get(token))
    total = len(documents)
    scores: list[tuple[float, int]] = []
    for ordinal, count in enumerate(counts):
        score = 0.0
        for token in query_terms:
            frequency = count.get(token, 0)
            if not frequency:
                continue
            containing = docs_with_term.get(token, 0) or 1
            idf = math.log(1 + (total - containing + 0.5) / (containing + 0.5))
            norm = frequency * (k1 + 1) / (
                frequency + k1 * (1 - b + b * lengths[ordinal] / average)
            )
            score += idf * norm
        scores.append((score, ordinal))
    ranked = [
        ordinal
        for score, ordinal in sorted(scores, key=lambda item: (-item[0], item[1]))
        if scores and score > 0
    ][:limit]
    return ranked


def chunked_rag_rank(
    query_text: str,
    corpus: list[str],
    chunk_tokens: int,
    top_k: int,
    limit: int,
) -> list[int]:
    """Deterministic lexical chunked-RAG: fixed chunks, BM25 over chunks."""
    chunks: list[tuple[int, str]] = []
    for ordinal, document in enumerate(corpus):
        words = document.split()
        for start in range(0, len(words), chunk_tokens):
            chunks.append((ordinal, " ".join(words[start:start + chunk_tokens])))
    ranked_chunks = bm25_rank_documents(
        query_text, [text for _, text in chunks], top_k
    )
    seen: list[int] = []
    for index in ranked_chunks:
        ordinal = chunks[index][0]
        if ordinal not in seen:
            seen.append(ordinal)
        if len(seen) >= limit:
            break
    return seen


def run_comparable_qa(
    rows: list[dict[str, Any]],
    system: str,
    query_limit: int | None = None,
    judge_key: str | None = None,
) -> dict[str, Any]:
    """Runs full-context or chunked-RAG QA through the shared reader."""
    from tools.reader_extractive import judge_correct, qa_metrics, read_list, read_single

    if system not in {"full-context", "chunked-rag"}:
        raise BenchmarkError(f"unsupported comparable system: {system}")
    corpora = locomo_corpora(rows)
    results: list[dict[str, Any]] = []
    skipped = Counter()
    selected = []
    for sample in rows:
        for ordinal, qa in enumerate(sample.get("qa") or []):
            if not isinstance(qa, dict) or qa.get("answer") in (None, ""):
                skipped["no-reference"] += 1
                continue
            selected.append((sample.get("sample_id"), ordinal, qa))
    if query_limit is not None:
        selected = selected[:query_limit]
    for sample_id, ordinal, qa in selected:
        corpus = corpora[sample_id]
        if system == "full-context":
            prediction = read_list(" || ".join(corpus))
        else:
            ranked = chunked_rag_rank(
                str(qa["question"]), corpus, 256, 2, 5,
            )
            prediction = read_list(" ".join(corpus[i] for i in ranked))
        references = qa["answer"]
        reference_texts = references if isinstance(references, list) else [references]
        metrics = qa_metrics(
            prediction, [str(reference) for reference in reference_texts]
        )
        verdict: dict[str, Any] = {"judge_executed": False}
        if judge_key:
            correct, record = judge_correct(
                str(qa["question"]),
                str(reference_texts[0]),
                prediction,
                judge_key,
            )
            verdict = {
                "judge_executed": True,
                "judge_model": record.model,
                "judge_correct": correct,
                "judge_request_digest": record.request_digest,
                "judge_response_digest": record.response_digest,
            }
        results.append(
            {
                "id": f"{sample_id}:{ordinal}",
                "segment": str(qa.get("category")),
                "f1": metrics["f1"],
                "bleu1": metrics["bleu1"],
                **verdict,
            }
        )
    judged = [r for r in results if r.get("judge_executed")]
    return {
        "system": system,
        "evaluated_questions": len(results),
        "skipped": dict(sorted(skipped.items())),
        "f1": round(sum(r["f1"] for r in results) / len(results), 6) if results else 0.0,
        "bleu1": round(sum(r["bleu1"] for r in results) / len(results), 6)
        if results
        else 0.0,
        "judge_accuracy": round(
            sum(r["judge_correct"] for r in judged) / len(judged), 6
        )
        if judged
        else None,
        "by_segment": {
            segment: {
                "questions": len(rows_in_segment),
                "f1": round(
                    sum(r["f1"] for r in rows_in_segment) / len(rows_in_segment), 6
                ),
            }
            for segment, rows_in_segment in sorted(
                (segment, list(group))
                for segment, group in groupby(
                    sorted(results, key=lambda r: r["segment"]),
                    key=lambda r: r["segment"],
                )
            )
        },
    }


def run_qa_span_probe(
    client: HyphaeClient,
    prepared: dict[str, Any],
    query_limit: int | None,
    start_after: int = 0,
    candidate_limit: int = CANDIDATE_LIMIT,
) -> dict[str, Any]:
    """Offline span-reader probe over the hybrid retrieval (no judge)."""
    from tools.reader_extractive import qa_metrics, read_span

    results: list[dict[str, Any]] = []
    selected = list(enumerate(prepared["queries"]))[start_after:]
    if query_limit is not None:
        selected = selected[:query_limit]
    for _, query in selected:
        references = query.get("answer")
        if references in (None, ""):
            continue
        reference_texts = references if isinstance(references, list) else [references]
        hits = execute_query_with_hits(client, query, candidate_limit, candidate_limit)
        prediction = read_span(" || ".join(text for _, text in hits[:5] if text))
        metrics = qa_metrics(
            prediction, [str(reference) for reference in reference_texts]
        )
        results.append({"segment": query["segment"], **metrics})
    return {
        "evaluated_questions": len(results),
        "f1": round(sum(r["f1"] for r in results) / len(results), 6) if results else 0.0,
        "bleu1": round(sum(r["bleu1"] for r in results) / len(results), 6)
        if results
        else 0.0,
        "by_segment": {
            segment: {
                "questions": len(rows_in),
                "f1": round(sum(r["f1"] for r in rows_in) / len(rows_in), 6),
            }
            for segment, rows_in in sorted(
                (segment, list(group))
                for segment, group in groupby(
                    sorted(results, key=lambda r: r["segment"]),
                    key=lambda r: r["segment"],
                )
            )
        },
    }


def run_qa(
    client: HyphaeClient,
    prepared: dict[str, Any],
    benchmark: str,
    query_limit: int | None,
    start_after: int = 0,
    candidate_limit: int = CANDIDATE_LIMIT,
    judge_key: str | None = None,
    judge_model: str = "openai-gpt-oss-120b",
) -> dict[str, Any]:
    """Runs the declarative end-to-end QA phase.

    Every system answers with the identical extractive reader over the same
    retrieval, so end-to-end numbers share one axis with published systems
    while staying deterministic and LLM-free; the optional declared judge
    only adds an audit-class label and never authorizes a claim.
    """
    from tools.reader_extractive import qa_metrics, judge_correct, read_single, read_list

    results: list[dict[str, Any]] = []
    skipped = Counter()
    selected_window = list(enumerate(prepared["queries"]))[start_after:]
    if query_limit is not None:
        selected_window = selected_window[:query_limit]
    for _, query in selected_window:
        references = query.get("answer")
        if benchmark == "locomo":
            if references is None or references == "":
                skipped["no-reference"] += 1
                continue
            reference_texts = (
                references if isinstance(references, list) else [references]
            )
        else:
            if query.get("excluded") is not None or not query.get("answer"):
                skipped[str(query.get("excluded") or "no-reference")] += 1
                continue
            reference_texts = [str(query["answer"])]
        if benchmark == "longmemeval":
            ingest(client, query["documents"], COLLECTION, 30_000_000_000)
        ranking_hits = execute_query_with_hits(
            client, query, candidate_limit, candidate_limit
        )
        top_texts = [text for _, text in ranking_hits[:5] if text]
        if not top_texts:
            prediction = ""
        elif isinstance(query.get("answer"), list):
            prediction = read_list(" ".join(top_texts))
        else:
            prediction = read_single(top_texts[0])
        metrics = qa_metrics(
            prediction, [str(reference) for reference in reference_texts]
        )
        verdict: dict[str, Any] = {"judge_executed": False}
        if judge_key:
            correct, record = judge_correct(
                query["text"],
                str(reference_texts[0]),
                prediction,
                judge_key,
                judge_model,
            )
            verdict = {
                "judge_executed": True,
                "judge_model": record.model,
                "judge_correct": correct,
                "judge_request_digest": record.request_digest,
                "judge_response_digest": record.response_digest,
            }
        results.append(
            {
                "id": query["id"],
                "segment": query["segment"],
                "f1": metrics["f1"],
                "bleu1": metrics["bleu1"],
                **verdict,
            }
        )
    return {
        "evaluated_questions": len(results),
        "skipped": dict(sorted(skipped.items())),
        "f1": round(sum(r["f1"] for r in results) / len(results), 6) if results else 0.0,
        "bleu1": round(sum(r["bleu1"] for r in results) / len(results), 6)
        if results
        else 0.0,
        "judge_accuracy": round(
            sum(r["judge_correct"] for r in results if r.get("judge_executed"))
            / len([r for r in results if r.get("judge_executed")]),
            6,
        )
        if any(r.get("judge_executed") for r in results)
        else None,
        "by_segment": {
            segment: {
                "questions": len(rows_in_segment),
                "f1": round(
                    sum(r["f1"] for r in rows_in_segment) / len(rows_in_segment), 6
                ),
            }
            for segment, rows_in_segment in sorted(
                (segment, list(group))
                for segment, group in groupby(
                    sorted(results, key=lambda r: r["segment"]),
                    key=lambda r: r["segment"],
                )
            )
        },
    }


def run_queries(
    client: HyphaeClient,
    prepared: dict[str, Any],
    benchmark: str,
    repetitions: int,
    query_limit: int | None,
    progress_path: Path | None = None,
    start_after: int = 0,
    candidate_limit: int = CANDIDATE_LIMIT,
    execution_context: dict[str, Any] | None = None,
    qrel_mode: str = DEFAULT_QREL_MODE,
    slice_b: bool = False,
    slice_b_quota_only: bool = False,
    session_cover: int = 0,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    cutoffs = cutoffs_for(benchmark, candidate_limit)
    if not cutoffs:
        raise BenchmarkError("candidate limit is below every benchmark cutoff")
    metric_function: Callable[
        [list[str], dict[str, Any], list[int], str], dict[str, float]
    ] = (
        locomo_query_metrics if benchmark == "locomo" else longmemeval_query_metrics
    )
    results = []
    changed_rankings = 0
    evaluated = 0
    skipped = Counter()
    longmemeval_active_ids: list[int] = []
    longmemeval_ingest_commits = 0
    selected_window = list(enumerate(prepared["queries"]))[start_after:]
    if query_limit is not None:
        selected_window = selected_window[:query_limit]
    indexed_queries = [
        (ordinal, query)
        for ordinal, query in selected_window
        if query.get("excluded") is None
        and not (benchmark == "locomo" and not query["targets"])
    ]
    selected_ordinals = {ordinal for ordinal, _ in indexed_queries}
    trace_context = execution_context or {"mode": "unspecified-local-execution"}
    metadata = trace_metadata(
        prepared, benchmark, repetitions, candidate_limit, qrel_mode, trace_context
    )
    if not selected_ordinals:
        raise BenchmarkError("benchmark metric denominator is empty")
    progress = (
        open_trace(progress_path, metadata, prepared, selected_ordinals)[0]
        if progress_path is not None
        else None
    )
    try:
        for _, query in selected_window:
            if benchmark == "locomo" and not query["targets"]:
                skipped["no-evidence"] += 1
            elif query.get("excluded") is not None:
                skipped[query["excluded"]] += 1
        for source_ordinal, query in indexed_queries:
            if benchmark == "longmemeval":
                if longmemeval_active_ids:
                    reset_longmemeval_collection(
                        client, longmemeval_active_ids, source_ordinal
                    )
                batch = {
                    "idempotency_id": 20_000_000_000 + source_ordinal + 1,
                    "documents": query["documents"],
                }
                client.search_ingest(COLLECTION, batch)
                longmemeval_ingest_commits += 1
                longmemeval_active_ids = [
                    document["object_id"] for document in query["documents"]
                ]
            document_lookup = (
                query["document_lookup"]
                if benchmark == "longmemeval"
                else prepared["document_lookup"]
            )
            warm = execute_query(
                client, query, candidate_limit, candidate_limit, slice_b,
                slice_b_quota_only, session_cover,
            )
            baseline = [document_lookup[object_id] for object_id in warm]
            latencies = []
            query_changed_rankings = 0
            for _ in range(repetitions):
                started = time.perf_counter_ns()
                object_ids = execute_query(
                    client, query, candidate_limit, candidate_limit, slice_b,
                    slice_b_quota_only, session_cover,
                )
                latencies.append(time.perf_counter_ns() - started)
                ranking = [
                    document_lookup[object_id] for object_id in object_ids
                ]
                if ranking != baseline:
                    query_changed_rankings += 1
            changed_rankings += query_changed_rankings
            result = {
                "id": query["id"],
                "sample_id": query["sample_id"],
                "conversation_id": query["conversation_id"],
                "segment": query["segment"],
                "expected_targets": list(query["targets"]),
                "scored_targets": scored_targets(query["targets"], qrel_mode),
                "logical_ranking": baseline,
                "metric_contributions": metric_function(
                    baseline, query, cutoffs, qrel_mode
                ),
                "latency": {
                    "clock": "time.perf_counter_ns",
                    "unit": "nanoseconds",
                    "warmup_excluded": True,
                    "samples": latencies,
                    "summary": latency_summary(latencies),
                },
                "changed_rankings": query_changed_rankings,
                "ranking_sha256": query_ranking_sha256(query["id"], baseline),
            }
            results.append(result)
            evaluated += 1
            if progress is not None:
                record = {
                    "record_type": "query",
                    "schema": TRACE_QUERY_SCHEMA,
                    "protocol_sha256": metadata["protocol_sha256"],
                    "source_ordinal": source_ordinal,
                    "result": result,
                }
                validate_query_trace_record(record, metadata, prepared)
                append_trace_record(progress, record)
    finally:
        if progress is not None:
            progress.close()
    return results, {
        "evaluated_queries": evaluated,
        "skipped_queries": dict(sorted(skipped.items())),
        "changed_rankings_on_repeat": changed_rankings,
        "ranking_sha256": combined_ranking_sha256(results),
        "query_corpus_ingest_commits": longmemeval_ingest_commits,
    }


def sum_telemetry(receipts: list[dict[str, Any]]) -> dict[str, Any]:
    output: dict[str, dict[str, Any]] = {}
    for receipt in receipts:
        metrics = receipt["clock_observations"]["server_telemetry_delta"]
        for name, metric in metrics.items():
            target = output.setdefault(
                name,
                {"count": 0, "sum_micros": 0, "buckets": [0] * len(metric["buckets"])},
            )
            target["count"] += metric["count"]
            target["sum_micros"] += metric["sum_micros"]
            target["buckets"] = [
                left + right for left, right in zip(target["buckets"], metric["buckets"])
            ]
    return output


def aggregate_chunk_receipts(
    benchmark: str,
    receipt_paths: list[Path],
    progress_paths: list[Path],
    dataset_path: Path | None,
) -> dict[str, Any]:
    if not receipt_paths or len(receipt_paths) != len(progress_paths):
        raise BenchmarkError("aggregate inputs must contain paired receipts and progress logs")
    receipts = [json.loads(path.read_text(encoding="utf-8")) for path in receipt_paths]
    first = receipts[0]
    compatibility_keys = (
        "candidate_limit", "cutoffs", "qrel_mode", "repetitions",
        "document_granularity", "document_text_views", "fusion",
        "document_text", "storage_scope", "identity_scope",
    )
    for receipt in receipts:
        if (
            receipt.get("benchmark") != benchmark
            or receipt.get("source") != first.get("source")
            or receipt.get("dataset") != first.get("dataset")
            or receipt.get("engine") != first.get("engine")
            or receipt.get("protocol", {}).get("trace_schema")
            != first.get("protocol", {}).get("trace_schema")
            or receipt.get("protocol", {}).get("trace_protocol_sha256")
            != first.get("protocol", {}).get("trace_protocol_sha256")
            or any(
                receipt.get("protocol", {}).get(key)
                != first.get("protocol", {}).get(key)
                for key in compatibility_keys
            )
        ):
            raise BenchmarkError("chunk receipts do not share one source and dataset identity")
        if receipt.get("protocol", {}).get("trace_enabled") is not True:
            raise BenchmarkError("chunk receipt was not produced with a query trace")
    prepared = (
        (
            prepare_locomo(
                load_dataset(dataset_path, benchmark),
                tuple(first["protocol"]["document_text_views"]),
                tuple(first["protocol"].get("rrf_weights", [])) or None,
            )
            if benchmark == "locomo"
            else prepare_longmemeval(load_dataset(dataset_path, benchmark))
        )
        if dataset_path is not None
        else None
    )
    if prepared is None and any(
        receipt["protocol"]["query_limit"] is not None for receipt in receipts
    ):
        raise BenchmarkError(
            "aggregate dataset is required when chunk source ordinals are non-contiguous"
        )
    if prepared is None and benchmark == "longmemeval":
        raise BenchmarkError(
            "aggregate dataset is required to validate LongMemEval excluded source ordinals"
        )
    trace_metadata_record = None
    progress = []
    seen_ordinals: set[int] = set()
    seen_query_ids: set[str] = set()
    for path in progress_paths:
        with path.open(encoding="utf-8") as handle:
            metadata, records = read_trace(
                handle, path, trace_metadata_record, prepared
            )
        if metadata is None:
            raise BenchmarkError(f"progress trace {path} is empty")
        trace_metadata_record = metadata
        for record in records:
            ordinal = record["source_ordinal"]
            if ordinal in seen_ordinals:
                raise BenchmarkError("progress traces contain a duplicate source ordinal")
            seen_ordinals.add(ordinal)
            query_id = record["result"]["id"]
            if query_id in seen_query_ids:
                raise BenchmarkError("progress traces contain a duplicate query ID")
            seen_query_ids.add(query_id)
            progress.append(record)
    if (
        trace_metadata_record is None
        or trace_metadata_record["protocol_sha256"]
        != first["protocol"]["trace_protocol_sha256"]
    ):
        raise BenchmarkError("progress trace metadata differs from chunk receipts")
    progress.sort(key=lambda record: record["source_ordinal"])
    results = [record["result"] for record in progress]
    expected = len(trace_metadata_record["protocol"]["eligible_source_ordinals"])
    expected_ordinals = set(trace_metadata_record["protocol"]["eligible_source_ordinals"])
    complete = seen_ordinals == expected_ordinals
    if len(results) != expected or not complete:
        raise BenchmarkError(
            f"aggregate progress is incomplete: observed {len(results)}, expected {expected}"
        )
    skipped = Counter()
    for receipt in receipts:
        skipped.update(receipt["correctness"]["skipped_queries"])
    if sum(receipt["correctness"]["evaluated_queries"] for receipt in receipts) != len(results):
        raise BenchmarkError("chunk receipt evaluated-query totals differ from traces")
    combined = json.loads(json.dumps(first))
    combined["retrieval"] = aggregate_results(results)
    changed_rankings = sum(result["changed_rankings"] for result in results)
    receipt_changed_rankings = sum(
        receipt["correctness"]["changed_rankings_on_repeat"] for receipt in receipts
    )
    if changed_rankings != receipt_changed_rankings:
        raise BenchmarkError("chunk receipt changed-ranking totals differ from traces")
    combined["correctness"] = {
        "evaluated_queries": len(results),
        "skipped_queries": dict(sorted(skipped.items())),
        "changed_rankings_on_repeat": changed_rankings,
        "ranking_sha256": combined_ranking_sha256(results),
        "query_corpus_ingest_commits": sum(
            receipt["correctness"]["query_corpus_ingest_commits"]
            for receipt in receipts
        ),
    }
    combined["preparation"] = {
        "documents": first["preparation"]["documents"],
        "ingest_commits": sum(
            receipt["preparation"]["ingest_commits"] for receipt in receipts
        ),
        "query_corpus_ingest_commits": combined["correctness"][
            "query_corpus_ingest_commits"
        ],
        "ingest_durability": "strict",
        "ingest_wall_nanos": sum(
            receipt["preparation"]["ingest_wall_nanos"] for receipt in receipts
        ),
        "maintenance_wall_nanos": sum(
            receipt["preparation"]["maintenance_wall_nanos"] for receipt in receipts
        ),
    }
    combined["clock_observations"]["server_telemetry_delta"] = sum_telemetry(receipts)
    combined["protocol"]["query_limit"] = None
    combined["protocol"]["parallel_chunks"] = len(receipts)
    combined["protocol"]["chunk_query_limits"] = [
        receipt["protocol"]["query_limit"] for receipt in receipts
    ]
    return combined


def evaluate(
    binary: Path,
    benchmark: str,
    dataset_path: Path,
    output: Path,
    repetitions: int,
    query_limit: int | None,
    require_clean: bool,
    progress_path: Path | None,
    start_after: int,
    locomo_views: tuple[str, ...],
    english_stop: bool,
    english_stem: bool,
    bm25_k1_micros: int | None,
    bm25_b_micros: int | None,
    candidate_limit: int,
    qrel_mode: str,
    rrf_weights: tuple[int, ...] | None,
    slice_b: bool = False,
    slice_b_quota_only: bool = False,
    session_cover: int = 0,
    qa: bool = False,
    judge_key: str | None = None,
) -> dict[str, Any]:
    rows = load_dataset(dataset_path, benchmark)
    prepared = (
        prepare_locomo(rows, locomo_views, rrf_weights)
        if benchmark == "locomo"
        else prepare_longmemeval(rows)
    )
    source = source_identity(binary, require_clean)
    source["harness_sha256"] = sha256_file(Path(__file__).resolve())
    version = run_binary(binary, ["version", "--json"])
    output.parent.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix=f"{benchmark}-", dir=output.parent))
    data_dir = scratch / "data"
    try:
        provision(
            binary,
            data_dir,
            benchmark,
            locomo_collection_ids(prepared) or None,
            english_stop,
            english_stem,
            bm25_k1_micros,
            bm25_b_micros,
        )
        process, endpoint = start_daemon(binary, data_dir)
        try:
            client = HyphaeClient.local(str(endpoint))
            ingest_started = time.perf_counter_ns()
            ingest_commits = (
                sum(
                    ingest(
                        client,
                        [
                            document
                            for document in prepared["documents"]
                            if document.get("collection") == collection
                        ],
                        collection,
                        collection * 1_000_000,
                    )
                    for collection in locomo_collection_ids(prepared)
                )
                if benchmark == "locomo"
                else 0
            )
            ingest_nanos = time.perf_counter_ns() - ingest_started
            client.close()
        finally:
            stop_daemon(process, endpoint)
        maintenance_started = time.perf_counter_ns()
        run_binary(binary, ["checkpoint", "--data-dir", str(data_dir)], timeout=3600)
        run_binary(binary, ["vacuum", "--data-dir", str(data_dir)], timeout=3600)
        maintenance_nanos = time.perf_counter_ns() - maintenance_started
        process, endpoint = start_daemon(binary, data_dir)
        try:
            client = HyphaeClient.local(str(endpoint))
            before = client.telemetry().value
            context = {
                "host": host_declaration(),
                "source_commit": source["commit"],
                "source_tree": source["tree"],
                "binary_sha256": source["binary_sha256"],
                "harness_sha256": source["harness_sha256"],
                "engine": version,
                "analyzer_english_stop": english_stop,
                "analyzer_english_stem": english_stem,
                "bm25_k1_micros": bm25_k1_micros,
                "bm25_b_micros": bm25_b_micros,
                "rrf_weights": list(prepared["protocol"].get("rrf_weights", [])),
                "slice_b": slice_b,
                "slice_b_quota_only": slice_b_quota_only,
            }
            results, correctness = run_queries(
                client, prepared, benchmark, repetitions, query_limit,
                progress_path, start_after, candidate_limit, context, qrel_mode,
                slice_b, slice_b_quota_only, session_cover,
            )
            qa_summary = (
                run_qa(
                    client,
                    prepared,
                    benchmark,
                    query_limit,
                    start_after,
                    candidate_limit,
                    judge_key,
                )
                if qa
                else None
            )
            after = client.telemetry().value
            client.close()
        finally:
            stop_daemon(process, endpoint)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)

    descriptor = DATASETS[benchmark]
    metadata = trace_metadata(
        prepared, benchmark, repetitions, candidate_limit, qrel_mode, context
    )
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "evidence_class": "local-diagnostic",
        "benchmark": benchmark,
        "source": source,
        "publication": {
            "authorized": False,
            "reason": "local diagnostic only; publication requires explicit review and approval",
        },
        "dataset": {
            "sha256": descriptor["sha256"],
            "upstream": descriptor["upstream"],
            "upstream_commit": descriptor["upstream_commit"],
            "license": descriptor["license"],
            "external_input_not_redistributed": True,
            **prepared["dataset_stats"],
        },
        "engine": version,
        "host": context["host"],
        "protocol": {
            "offline_execution": True,
            "network_used_during_execution": False,
            "transport": "native-local",
            "branch": "lexical-bm25" if not slice_b else "lexical-bm25+native-docvalue",
            "slice_b": slice_b,
            "slice_b_quota_only": slice_b_quota_only,
            "session_cover": session_cover,
            "candidate_limit": candidate_limit,
            "cutoffs": cutoffs_for(benchmark, candidate_limit),
            "qrel_mode": qrel_mode,
            "qrel_semantics": metadata["protocol"]["qrel_semantics"],
            "trace_enabled": progress_path is not None,
            "trace_schema": TRACE_SCHEMA,
            "trace_query_schema": TRACE_QUERY_SCHEMA,
            "trace_protocol_schema": TRACE_PROTOCOL_SCHEMA,
            "trace_protocol_sha256": metadata["protocol_sha256"],
            "trace_digest_canonicalization": CANONICAL_DIGEST,
            "warmup_per_query": 1,
            "repetitions": repetitions,
            "query_limit": query_limit,
            "models_executed": False,
            "analyzer_english_stop": english_stop,
            "analyzer_english_stem": english_stem,
            "bm25_k1_micros": bm25_k1_micros,
            "bm25_b_micros": bm25_b_micros,
            **prepared["protocol"],
        },
        "preparation": {
            "documents": (
                len(prepared["documents"])
                if benchmark == "locomo"
                else prepared["dataset_stats"]["sessions"]
            ),
            "ingest_commits": ingest_commits,
            "query_corpus_ingest_commits": correctness[
                "query_corpus_ingest_commits"
            ],
            "ingest_durability": "strict",
            "ingest_wall_nanos": ingest_nanos,
            "maintenance_wall_nanos": maintenance_nanos,
        },
        "correctness": correctness,
        "qa": qa_summary,
        "retrieval": aggregate_results(results),
        "clock_observations": {
            "server_telemetry_delta": telemetry_delta(before, after),
            "retrieval_durability": {
                "status": "not-applicable",
                "reason": "retrieval is read-only; strict ingest is reported separately",
            },
            "attribution_note": "independent telemetry aggregates are not subtracted from end-to-end latency",
        },
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--benchmark", choices=sorted(DATASETS), required=True)
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--query-limit", type=int, default=None)
    parser.add_argument("--progress", type=Path, default=None)
    parser.add_argument("--start-after", type=int, default=0)
    parser.add_argument("--require-clean-source", action="store_true")
    parser.add_argument(
        "--locomo-view",
        action="append",
        choices=("bare", "timestamp", "timestamp-previous", "centered"),
    )
    parser.add_argument("--analyzer-english-stop", action="store_true")
    parser.add_argument("--analyzer-english-stem", action="store_true")
    parser.add_argument(
        "--slice-b",
        action="store_true",
        help="add native doc-value temporal and per-session quota branches to LoCoMo fusion",
    )
    parser.add_argument(
        "--slice-b-quota-only",
        action="store_true",
        help="slice-b with only the per-session quota branch",
    )
    parser.add_argument(
        "--session-cover",
        type=int,
        default=0,
        help="prioritize covering this many distinct sessions before depth (0 disables)",
    )
    parser.add_argument(
        "--qa",
        action="store_true",
        help="run the declarative end-to-end QA phase with the extractive reader",
    )
    parser.add_argument(
        "--judge-key-file",
        type=Path,
        default=None,
        help="restricted file with one declared-judge API key (audit class only)",
    )
    parser.add_argument("--bm25-k1-micros", type=int)
    parser.add_argument("--bm25-b-micros", type=int)
    parser.add_argument("--candidate-limit", type=int, default=CANDIDATE_LIMIT)
    parser.add_argument(
        "--rrf-weight",
        type=int,
        action="append",
        help="positive RRF branch weight aligned positionally with --locomo-view",
    )
    parser.add_argument(
        "--qrel-mode", choices=QREL_MODES, default=DEFAULT_QREL_MODE,
        help="audited-v2 deduplicates expected targets; raw-compat preserves occurrences",
    )
    parser.add_argument("--aggregate-receipts", type=Path, nargs="+")
    parser.add_argument("--aggregate-progress", type=Path, nargs="+")
    parser.add_argument("--aggregate-dataset", type=Path)
    arguments = parser.parse_args()
    aggregating = arguments.aggregate_receipts is not None or arguments.aggregate_progress is not None
    if aggregating:
        if arguments.aggregate_receipts is None or arguments.aggregate_progress is None:
            print("error: aggregate receipts and progress must be supplied together", file=sys.stderr)
            return 1
        try:
            receipt = aggregate_chunk_receipts(
                arguments.benchmark,
                [path.resolve() for path in arguments.aggregate_receipts],
                [path.resolve() for path in arguments.aggregate_progress],
                arguments.aggregate_dataset.resolve()
                if arguments.aggregate_dataset is not None
                else None,
            )
        except (
            BenchmarkError, OSError, UnicodeError, json.JSONDecodeError, KeyError, TypeError
        ) as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        encoded = json.dumps(receipt, indent=2, sort_keys=True)
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(encoded + "\n", encoding="utf-8")
        print(encoded)
        return 0
    if arguments.binary is None or not arguments.binary.is_file() or not arguments.dataset.is_file():
        print("error: binary or dataset is missing", file=sys.stderr)
        return 1
    if not 1 <= arguments.repetitions <= 1_000:
        print("error: repetitions must be within 1..=1000", file=sys.stderr)
        return 1
    if arguments.query_limit is not None and arguments.query_limit < 1:
        print("error: query limit must be positive", file=sys.stderr)
        return 1
    if arguments.start_after < 0:
        print("error: start-after must be nonnegative", file=sys.stderr)
        return 1
    if (arguments.bm25_k1_micros is None) != (arguments.bm25_b_micros is None):
        print("error: both BM25 parameters must be supplied together", file=sys.stderr)
        return 1
    if not 1 <= arguments.candidate_limit <= 10_000:
        print("error: candidate-limit must be within 1..=10000", file=sys.stderr)
        return 1
    if not 0 <= arguments.session_cover <= 8:
        print("error: session-cover must be within 0..=8", file=sys.stderr)
        return 1
    locomo_views = tuple(arguments.locomo_view or ["bare"])
    rrf_weights = (
        tuple(arguments.rrf_weight)
        if arguments.rrf_weight is not None
        else None
    )
    if rrf_weights is not None and (
        len(rrf_weights) != len(locomo_views)
        or any(not 1 <= weight <= 1_000 for weight in rrf_weights)
    ):
        print("error: each LoCoMo view requires one RRF weight within 1..=1000", file=sys.stderr)
        return 1
    try:
        receipt = evaluate(
            arguments.binary.resolve(), arguments.benchmark, arguments.dataset.resolve(),
            arguments.output.resolve(), arguments.repetitions, arguments.query_limit,
            arguments.require_clean_source,
            arguments.progress.resolve() if arguments.progress is not None else None,
            arguments.start_after,
            locomo_views,
            arguments.analyzer_english_stop,
            arguments.analyzer_english_stem,
            arguments.bm25_k1_micros,
            arguments.bm25_b_micros,
            arguments.candidate_limit,
            arguments.qrel_mode,
            rrf_weights,
            arguments.slice_b,
            arguments.slice_b_quota_only,
            arguments.session_cover,
            arguments.qa,
            arguments.judge_key_file.read_text(encoding="utf-8").strip()
            if arguments.judge_key_file is not None
            else None,
        )
    except (BenchmarkError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0
if __name__ == "__main__":
    sys.exit(main())
