#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Produce and qualify reproducible local durable ANN smoke evidence."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path
from typing import Any, Mapping, Sequence

from tools.check_native_ann_durable_qualification import GateFailure, validate


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "conformance" / "g7" / "runners" / "rust" / "Cargo.toml"
BINARY_NAME = "native_ann_durable_qualification"
METRICS = ("squared-l2", "cosine", "negative-dot")
LOCAL_MAXIMUM_VECTORS = 4_096
# Frozen from the Rust generator's versioned canonical corpus encoding.
CORPUS_IDENTITIES = {
    "squared-l2": "00cc1e9902b7fe1e2510b4186f09f273a9879ffcb4e242a22e26484d1f539de2",
    "cosine": "50159308b293f85b9d792675353862be9da6e792899b0b526ac3b83201b0b464",
    "negative-dot": "261c30bd71097e4d8191bc8ad27a08e457ad55d701b9583ded96c799f06d8656",
}


class QualificationSuiteFailure(ValueError):
    """The local three-metric suite is incomplete or cannot qualify."""


def _git(*arguments: str) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def source_authority(expected_commit: str) -> tuple[str, str]:
    commit = _git("rev-parse", "HEAD")
    tree = _git("rev-parse", "HEAD^{tree}")
    if commit != expected_commit:
        raise QualificationSuiteFailure(
            "checked-out source commit differs from expected SHA"
        )
    dirty = _git("status", "--porcelain")
    if dirty:
        raise QualificationSuiteFailure(
            "local qualification requires a clean exact source tree"
        )
    return commit, tree


def validate_suite(
    receipts: Sequence[dict[str, Any]],
    expected_commit: str,
    expected_tree: str,
    expected_corpora: Mapping[str, str],
) -> dict[str, Any]:
    if set(expected_corpora) != set(METRICS):
        raise QualificationSuiteFailure("expected corpus map has the wrong metric set")
    by_metric: dict[str, dict[str, Any]] = {}
    for payload in receipts:
        try:
            metric = payload["dataset"]["metric"]
        except (KeyError, TypeError) as error:
            raise QualificationSuiteFailure("receipt omitted its metric") from error
        if metric not in METRICS or metric in by_metric:
            raise QualificationSuiteFailure(
                "suite must contain the exact metric set once"
            )
        by_metric[metric] = payload
    if set(by_metric) != set(METRICS):
        raise QualificationSuiteFailure("suite must contain the exact metric set once")

    audits = []
    for metric in METRICS:
        payload = by_metric[metric]
        source = payload.get("source")
        if not isinstance(source, dict) or source.get("tree") != expected_tree:
            raise QualificationSuiteFailure(
                f"{metric} receipt targets another source tree"
            )
        dataset = payload.get("dataset")
        if not isinstance(dataset, dict):
            raise QualificationSuiteFailure(f"{metric} receipt omitted its dataset")
        vectors = dataset.get("vectors")
        if (
            not isinstance(vectors, int)
            or isinstance(vectors, bool)
            or not 1 <= vectors <= LOCAL_MAXIMUM_VECTORS
        ):
            raise QualificationSuiteFailure(
                f"{metric} receipt exceeds the local smoke bound"
            )
        try:
            audit = validate(
                payload,
                expected_commit,
                mode="qualification",
                expected_corpus_identity=expected_corpora[metric],
            )
        except GateFailure as error:
            raise QualificationSuiteFailure(f"{metric}: {error}") from error
        audits.append(audit)

    return {
        "schema": "hyphae-native-ann-durable-local-suite-v1",
        "status": "passed",
        "scope": "local-durable-ann-smoke",
        "evidence_kind": "correctness-qualification",
        "source_commit": expected_commit,
        "source_tree": expected_tree,
        "metrics": list(METRICS),
        "receipts": audits,
        "closure_declared": False,
        "g7_closure_declared": False,
    }


def _binary_path() -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return MANIFEST.parent / "target" / "release" / f"{BINARY_NAME}{suffix}"


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def _build_runner() -> None:
    subprocess.run(
        [
            "cargo",
            "build",
            "--manifest-path",
            str(MANIFEST),
            "--locked",
            "--release",
            "--bin",
            BINARY_NAME,
        ],
        cwd=ROOT,
        check=True,
    )


def _run_metric(binary: Path, commit: str, tree: str, metric: str) -> dict[str, Any]:
    completed = subprocess.run(
        [
            str(binary),
            "--source-commit",
            commit,
            "--source-tree",
            tree,
            "--metric",
            metric,
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=300,
    )
    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise QualificationSuiteFailure(
            f"{metric} runner returned invalid JSON: {completed.stderr.strip()}"
        ) from error
    if completed.returncode != 0:
        raise QualificationSuiteFailure(
            f"{metric} runner failed ({completed.returncode}): "
            f"{completed.stderr.strip()}"
        )
    if not isinstance(payload, dict):
        raise QualificationSuiteFailure(f"{metric} runner did not return one receipt")
    return payload


def run_suite(
    output_directory: Path,
    expected_commit: str,
) -> dict[str, Any]:
    commit, tree = source_authority(expected_commit)
    _build_runner()
    if source_authority(expected_commit) != (commit, tree):
        raise QualificationSuiteFailure("source authority changed during runner build")
    executable = _binary_path()
    if not executable.is_file():
        raise QualificationSuiteFailure(f"qualification runner not found: {executable}")
    receipts = []
    for metric in METRICS:
        payload = _run_metric(executable, commit, tree, metric)
        _write_json(output_directory / f"{metric}.json", payload)
        receipts.append(payload)
    if source_authority(expected_commit) != (commit, tree):
        raise QualificationSuiteFailure("source authority changed during qualification")
    audit = validate_suite(receipts, commit, tree, CORPUS_IDENTITIES)
    _write_json(output_directory / "audit.json", audit)
    return audit


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--expected-commit", required=True)
    arguments = parser.parse_args()
    try:
        audit = run_suite(arguments.output_dir, arguments.expected_commit)
    except (OSError, subprocess.SubprocessError, QualificationSuiteFailure) as error:
        parser.error(str(error))
    print(json.dumps(audit, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
