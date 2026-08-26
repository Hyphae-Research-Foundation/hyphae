#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Declarative extractive reader for head-to-head memory QA.

All retrieval systems in a comparison answer through this identical reader:
the answer is the retrieved text itself (or its overlap-reduced entity
list), never generated. F1 and BLEU-1 follow the LoCoMo evaluation
conventions token-for-token, so our end-to-end numbers sit on the same axis
as published systems while remaining fully deterministic, offline, and free
of any LLM in the loop.
"""

from __future__ import annotations

import re
import string
from collections import Counter
from dataclasses import dataclass

try:
    from blake3 import blake3
except ImportError:  # pragma: no cover - judge is optional
    import hashlib

    def blake3(data: bytes):  # type: ignore[no-redef]
        return hashlib.sha256(data)


ARTICLES = {"a", "an", "the"}


def normalize_tokens(text: str) -> list[str]:
    lowered = text.lower()
    cleaned = "".join(
        ch for ch in lowered if ch not in string.punctuation or ch == "'"
    )
    tokens = [
        token
        for token in cleaned.replace("'", " ").split()
        if token not in ARTICLES and token
    ]
    return tokens


def token_f1(prediction: str, reference: str) -> float:
    predicted = normalize_tokens(prediction)
    truth = normalize_tokens(reference)
    if not predicted or not truth:
        return 1.0 if not predicted and not truth else 0.0
    overlap = Counter(predicted) & Counter(truth)
    shared = sum(overlap.values())
    if shared == 0:
        return 0.0
    precision = shared / len(predicted)
    recall = shared / len(truth)
    return 2 * precision * recall / (precision + recall)


def bleu1(prediction: str, reference: str) -> float:
    predicted = normalize_tokens(prediction)
    truth = Counter(normalize_tokens(reference))
    if not predicted or not truth:
        return 0.0
    overlap = sum(min(Counter(predicted)[token], count) for token, count in truth.items())
    return overlap / len(predicted)


_ENTITIES = re.compile(
    r"(?:"
    r"[A-Z][a-zA-Z]*(?:\s+[A-Z][a-zA-Z]*)*"
    r"|\b\d+(?:\.\d+)?\s*(?:%|percent|am|pm|years?|months?|weeks?|days?|hours?|minutes?|miles?|km|dollars?|bucks)?"
    r")"
)


def extract_entities(text: str, limit: int = 8) -> list[str]:
    seen: list[str] = []
    for match in _ENTITIES.finditer(text):
        entity = match.group(0).strip(" .,;:")
        key = entity.lower()
        if len(entity) > 1 and key not in {value.lower() for value in seen}:
            seen.append(entity)
        if len(seen) >= limit:
            break
    return seen


def read_single(retrieved_text: str) -> str:
    return retrieved_text.strip()


_SPANS = re.compile(
    r"(?:"
    r"\d{1,2}:\d{2}\s?(?:am|pm)?"
    r"|\d{1,2}(?:st|nd|rd|th)?\s+(?:January|February|March|April|May|June|July|August|September|October|November|December)(?:,?\s+\d{4})?"
    r"|[A-Z][a-z]+\s+\d{1,2}(?:st|nd|rd|th)?"
    r"|\b(?:January|February|March|April|May|June|July|August|September|October|November|December)\b(?:\s+\d{1,2})?(?:,?\s+\d{4})?"
    r"|[A-Z][a-zA-Z]+(?:\s+[A-Z][a-zA-Z]+)+"
    r"|[A-Z][a-zA-Z]+"
    r"|\b\d+(?:\.\d+)?\b"
    r")"
)


def read_span(retrieved_text: str, limit: int = 6) -> str:
    """Extracts the shortest candidate answer spans from retrieved text.

    Declarative and deterministic: the prediction is the retrieved text's own
    entity/date/number spans, ordered by first appearance and bounded, which
    mirrors how LoCoMo references are phrased without any generation.
    """
    seen: list[str] = []
    for match in _SPANS.finditer(retrieved_text):
        span = match.group(0).strip(" .,;:")
        lowered = span.lower()
        if len(span) > 2 and lowered not in {value.lower() for value in seen}:
            seen.append(span)
        if len(seen) >= limit:
            break
    return ", ".join(seen)


def read_list(retrieved_text: str, limit: int = 8) -> str:
    return ", ".join(extract_entities(retrieved_text, limit))


def qa_metrics(
    prediction: str,
    references: list[str],
) -> dict[str, float]:
    if not references:
        raise ValueError("qa metrics require at least one reference answer")
    return {
        "f1": max(token_f1(prediction, reference) for reference in references),
        "bleu1": max(bleu1(prediction, reference) for reference in references),
    }


JUDGE_MODEL = "openai-gpt-oss-120b"
JUDGE_BASE_URL = "https://inference.do-ai.run/v1"

_JUDGE_PROMPT = (
    "You are an answer-equivalence judge for a long-term memory benchmark. "
    "Given a question, a correct reference answer, and a system answer, decide "
    "whether the system answer is correct: it must convey the reference "
    "answer's key fact(s) without contradicting them. Extra harmless detail "
    "is allowed; wrong facts are not. Respond with JSON only: "
    "{\"correct\": true} or {\"correct\": false}."
)


def judge_correct(
    question: str,
    reference: str,
    prediction: str,
    api_key: str,
    model: str = JUDGE_MODEL,
    timeout: float = 60.0,
) -> tuple[bool, "DeclaredJudgeRecord"]:
    """Runs the declared LLM judge and returns (correct, audit record)."""
    import json
    import urllib.request

    payload = {
        "model": model,
        "temperature": 0,
        "messages": [
            {"role": "system", "content": _JUDGE_PROMPT},
            {
                "role": "user",
                "content": json.dumps(
                    {
                        "question": question,
                        "reference": reference,
                        "system_answer": prediction,
                    },
                    sort_keys=True,
                ),
            },
        ],
    }
    request_bytes = json.dumps(payload, sort_keys=True).encode("utf-8")
    request = urllib.request.Request(
        f"{JUDGE_BASE_URL}/chat/completions",
        data=request_bytes,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    import time as _time
    import urllib.error as _urllib_error

    response_bytes = b""
    for attempt in range(5):
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                response_bytes = response.read()
            break
        except _urllib_error.HTTPError as error:
            if error.code != 429 or attempt == 4:
                raise
            _time.sleep(2.0 * (2 ** attempt))
    body = json.loads(response_bytes)
    message = body["choices"][0]["message"]
    content = ""
    raw_content = message.get("content")
    if isinstance(raw_content, str):
        content = raw_content
    elif isinstance(raw_content, list):
        content = " ".join(
            part.get("text", "") for part in raw_content if isinstance(part, dict)
        )
    if not content:
        reasoning = message.get("reasoning") or message.get("reasoning_content") or ""
        content = reasoning if isinstance(reasoning, str) else ""
    content = content.strip()
    try:
        verdict = json.loads(content)
        correct = bool(verdict.get("correct"))
    except (json.JSONDecodeError, AttributeError, TypeError):
        lowered = content.lower()
        correct = (
            '"correct": true' in lowered
            or lowered.endswith("true")
            or ("true" in lowered and "false" not in lowered)
        )
    record = DeclaredJudgeRecord(
        model=model,
        request_digest=blake3(request_bytes).hexdigest(),
        response_digest=blake3(response_bytes).hexdigest(),
    )
    return correct, record


@dataclass(frozen=True)
class DeclaredJudgeRecord:
    """Audit-only evidence class: the judge never authorizes a claim."""

    model: str
    request_digest: str
    response_digest: str
