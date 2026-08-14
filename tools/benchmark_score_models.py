#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Compare candidate vector encodings for Hyphae's canonical 0.2 scorer."""

from __future__ import annotations

import argparse
import json
import math
import platform
import random
import struct
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

SCORE_SCALE = 1_000_000_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seed", type=int, default=20260720)
    parser.add_argument("--candidates", type=int, default=2_048)
    parser.add_argument("--queries", type=int, default=32)
    parser.add_argument("--dimensions", type=int, default=128)
    parser.add_argument("--top-k", type=int, default=10)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def normalize(values: Sequence[float]) -> list[float]:
    norm = math.sqrt(sum(value * value for value in values))
    if norm == 0.0:
        raise ValueError("generated a zero vector")
    return [value / norm for value in values]


def f32(value: float) -> float:
    return struct.unpack("<f", struct.pack("<f", value))[0]


def round_half_away(value: float) -> int:
    if value >= 0.0:
        return math.floor(value + 0.5)
    return math.ceil(value - 0.5)


def quantize_unit(values: Sequence[float], scale: int) -> list[int]:
    maximum = max(abs(value) for value in values)
    if maximum == 0.0:
        raise ValueError("cannot quantize a zero vector")
    return [
        max(-scale, min(scale, round_half_away(value / maximum * scale)))
        for value in values
    ]


def float_score(left: Sequence[float], right: Sequence[float]) -> float:
    dot = sum(a * b for a, b in zip(left, right, strict=True))
    left_norm = math.sqrt(sum(value * value for value in left))
    right_norm = math.sqrt(sum(value * value for value in right))
    return max(-1.0, min(1.0, dot / (left_norm * right_norm)))


def integer_score(left: Sequence[int], right: Sequence[int]) -> int:
    dot = sum(a * b for a, b in zip(left, right, strict=True))
    left_squared = sum(value * value for value in left)
    right_squared = sum(value * value for value in right)
    denominator = math.isqrt(left_squared * right_squared)
    if denominator == 0:
        raise ValueError("canonical integer vector has zero magnitude")
    magnitude = (abs(dot) * SCORE_SCALE + denominator // 2) // denominator
    magnitude = min(SCORE_SCALE, magnitude)
    return -magnitude if dot < 0 else magnitude


def ranked(
    query: Sequence[float] | Sequence[int],
    candidates: Sequence[Sequence[float] | Sequence[int]],
    scorer: Callable[[Sequence, Sequence], float | int],
) -> list[int]:
    scored = [(scorer(query, candidate), index) for index, candidate in enumerate(candidates)]
    scored.sort(key=lambda item: (-item[0], item[1]))
    return [index for _, index in scored]


@dataclass
class ModelResult:
    name: str
    element_bytes: int
    deterministic_integer_scoring: bool
    elapsed_seconds: float
    top_1_agreement: float
    mean_top_k_overlap: float


def evaluate_model(
    *,
    name: str,
    element_bytes: int,
    deterministic_integer_scoring: bool,
    encoded_candidates: Sequence[Sequence[float] | Sequence[int]],
    encoded_queries: Sequence[Sequence[float] | Sequence[int]],
    scorer: Callable[[Sequence, Sequence], float | int],
    oracle_rankings: Sequence[Sequence[int]],
    top_k: int,
) -> ModelResult:
    started = time.perf_counter()
    rankings = [
        ranked(query, encoded_candidates, scorer)
        for query in encoded_queries
    ]
    elapsed = time.perf_counter() - started
    top_1 = sum(
        ranking[0] == oracle[0]
        for ranking, oracle in zip(rankings, oracle_rankings, strict=True)
    ) / len(rankings)
    overlap = sum(
        len(set(ranking[:top_k]) & set(oracle[:top_k])) / top_k
        for ranking, oracle in zip(rankings, oracle_rankings, strict=True)
    ) / len(rankings)
    return ModelResult(
        name=name,
        element_bytes=element_bytes,
        deterministic_integer_scoring=deterministic_integer_scoring,
        elapsed_seconds=elapsed,
        top_1_agreement=top_1,
        mean_top_k_overlap=overlap,
    )


def main() -> int:
    args = parse_args()
    if min(args.candidates, args.queries, args.dimensions, args.top_k) <= 0:
        raise SystemExit("all shape arguments must be positive")
    if args.top_k > args.candidates:
        raise SystemExit("top-k cannot exceed candidate count")

    random_source = random.Random(args.seed)
    candidates = [
        normalize([random_source.gauss(0.0, 1.0) for _ in range(args.dimensions)])
        for _ in range(args.candidates)
    ]
    queries = []
    for query_index in range(args.queries):
        base = candidates[(query_index * 61) % len(candidates)]
        queries.append(
            normalize(
                [
                    value + random_source.gauss(0.0, 0.035)
                    for value in base
                ]
            )
        )

    oracle_started = time.perf_counter()
    oracle_rankings = [
        ranked(query, candidates, float_score)
        for query in queries
    ]
    oracle_elapsed = time.perf_counter() - oracle_started

    f32_candidates = [[f32(value) for value in vector] for vector in candidates]
    f32_queries = [[f32(value) for value in vector] for vector in queries]
    q15_candidates = [quantize_unit(vector, 32_767) for vector in candidates]
    q15_queries = [quantize_unit(vector, 32_767) for vector in queries]
    q6_candidates = [quantize_unit(vector, 1_000_000) for vector in candidates]
    q6_queries = [quantize_unit(vector, 1_000_000) for vector in queries]

    results = [
        evaluate_model(
            name="f32-f64-cosine",
            element_bytes=4,
            deterministic_integer_scoring=False,
            encoded_candidates=f32_candidates,
            encoded_queries=f32_queries,
            scorer=float_score,
            oracle_rankings=oracle_rankings,
            top_k=args.top_k,
        ),
        evaluate_model(
            name="q15-i16-cosine-nanos",
            element_bytes=2,
            deterministic_integer_scoring=True,
            encoded_candidates=q15_candidates,
            encoded_queries=q15_queries,
            scorer=integer_score,
            oracle_rankings=oracle_rankings,
            top_k=args.top_k,
        ),
        evaluate_model(
            name="q6-i32-cosine-nanos",
            element_bytes=4,
            deterministic_integer_scoring=True,
            encoded_candidates=q6_candidates,
            encoded_queries=q6_queries,
            scorer=integer_score,
            oracle_rankings=oracle_rankings,
            top_k=args.top_k,
        ),
    ]

    report = {
        "schema": "hyphae-score-model-benchmark-v1",
        "parameters": {
            "seed": args.seed,
            "candidates": args.candidates,
            "queries": args.queries,
            "dimensions": args.dimensions,
            "top_k": args.top_k,
            "score_scale": SCORE_SCALE,
        },
        "environment": {
            "python": platform.python_version(),
            "implementation": platform.python_implementation(),
            "machine": platform.machine(),
            "platform": platform.platform(),
        },
        "oracle": {
            "name": "f64-cosine",
            "elapsed_seconds": round(oracle_elapsed, 6),
        },
        "models": [
            {
                "name": result.name,
                "element_bytes": result.element_bytes,
                "deterministic_integer_scoring": result.deterministic_integer_scoring,
                "elapsed_seconds": round(result.elapsed_seconds, 6),
                "top_1_agreement": round(result.top_1_agreement, 6),
                "mean_top_k_overlap": round(result.mean_top_k_overlap, 6),
            }
            for result in results
        ],
        "decision": {
            "selected": "q15-i16-cosine-nanos",
            "reasons": [
                "integer-only canonical scoring after ingestion",
                "two bytes per vector element",
                "ranking quality measured against the f64 brute-force oracle",
                "portable proof reexecution without implicit floating-point bytes",
            ],
        },
    }
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    else:
        print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
