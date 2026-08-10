#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Produce one suite- and corpus-bound exact-SHA G4 receipt."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
TEST_RESULT = re.compile(r"test result: ok\. ([0-9]+) passed; 0 failed;")
UNITTEST_RESULT = re.compile(r"Ran ([0-9]+) tests? in ")


class GateFailure(ValueError):
    pass


def _digest(raw: bytes, supplied: str, label: str) -> None:
    if HEX64.fullmatch(supplied) is None or hashlib.sha256(raw).hexdigest() != supplied:
        raise GateFailure(f"{label} digest mismatch")


def build_receipt(
    source_commit: str,
    requirement: str,
    suite_raw: bytes,
    suite_sha256: str,
    corpus_raw: bytes,
    corpus_sha256: str,
    platform: str,
    toolchain: str,
    logs: list[tuple[str, bytes]],
    root: Path | None = None,
) -> dict[str, Any]:
    if HEX40.fullmatch(source_commit) is None or not platform or not toolchain:
        raise GateFailure("exact commit, platform, and toolchain are required")
    _digest(suite_raw, suite_sha256, "suite manifest")
    _digest(corpus_raw, corpus_sha256, "corpus manifest")
    suite_manifest = json.loads(suite_raw)
    corpus_manifest = json.loads(corpus_raw)
    if suite_manifest.get("schema") != "hyphae-native-g4-suite-manifest-v1" or suite_manifest.get("gate") != "G4":
        raise GateFailure("unsupported suite manifest")
    if corpus_manifest.get("schema") != "hyphae-native-g4-corpus-manifest-v1" or corpus_manifest.get("gate") != "G4":
        raise GateFailure("unsupported corpus manifest")
    for manifest in (suite_manifest, corpus_manifest):
        if manifest.get("claims") != [] or manifest.get("closure_declared") is not False:
            raise GateFailure("manifests must not make claims or declare closure")
    rows = suite_manifest.get("requirements")
    matches = [row for row in rows if row.get("id") == requirement] if isinstance(rows, list) else []
    if len(matches) != 1:
        raise GateFailure("requirement is absent or duplicated")
    row = matches[0]
    corpus_rows = corpus_manifest.get("corpora")
    if not isinstance(corpus_rows, list) or not corpus_rows or any(not isinstance(item, dict) for item in corpus_rows):
        raise GateFailure("corpus manifest rows are invalid")
    corpus_ids = [item.get("id") for item in corpus_rows]
    if any(not isinstance(identifier, str) or not identifier for identifier in corpus_ids) or len(corpus_ids) != len(set(corpus_ids)):
        raise GateFailure("corpus identities are invalid or duplicated")
    required_corpora = row.get("corpora")
    if not isinstance(required_corpora, list) or not required_corpora or len(required_corpora) != len(set(required_corpora)) or not set(required_corpora) <= set(corpus_ids):
        raise GateFailure("authorized corpus set is invalid")
    if any(requirement not in item.get("requirements", []) for item in corpus_rows if item.get("id") in required_corpora):
        raise GateFailure("corpus does not authorize requirement")
    if root is not None:
        resolved_root = root.resolve()
        for item in corpus_rows:
            source = item.get("source")
            expected_digest = item.get("sha256")
            if not isinstance(source, str) or not isinstance(expected_digest, str) or HEX64.fullmatch(expected_digest) is None:
                raise GateFailure("corpus source binding is invalid")
            artifact = (resolved_root / source).resolve()
            try:
                artifact.relative_to(resolved_root)
            except ValueError as error:
                raise GateFailure("corpus source escapes repository root") from error
            if not artifact.is_file() or hashlib.sha256(artifact.read_bytes()).hexdigest() != expected_digest:
                raise GateFailure("corpus source digest mismatch")
    suites = row.get("suites")
    expected = {suite.get("name"): suite.get("command") for suite in suites} if isinstance(suites, list) else {}
    supplied = {name: raw for name, raw in logs}
    if not expected or None in expected or len(supplied) != len(logs) or set(supplied) != set(expected):
        raise GateFailure("supplied logs do not exactly match authorized suites")
    audits = []
    total = 0
    for name in sorted(expected):
        command = expected[name]
        if not isinstance(command, list) or not command:
            raise GateFailure("authorized suite command is invalid")
        raw = supplied[name]
        text = raw.decode("utf-8")
        marker = "G4_COMMAND: " + json.dumps(command, separators=(",", ":"))
        if marker not in text or "test result: FAILED" in text:
            raise GateFailure(f"suite {name} does not prove its authorized command")
        counts = [int(value) for value in TEST_RESULT.findall(text)]
        if not counts and "\nOK\n" in text:
            counts = [int(value) for value in UNITTEST_RESULT.findall(text)]
        if not counts or any(value <= 0 for value in counts):
            raise GateFailure(f"suite {name} has no positive successful test result")
        count = sum(counts)
        total += count
        audits.append({"name": name, "test_count": count, "log_sha256": hashlib.sha256(raw).hexdigest()})
    return {
        "schema": "hyphae-native-g4-receipt-v1", "status": "passed",
        "requirement": requirement, "source_commit": source_commit,
        "suite_manifest_sha256": suite_sha256, "corpus_manifest_sha256": corpus_sha256,
        "corpora": required_corpora, "platform": platform, "toolchain": toolchain,
        "suites": audits, "test_count": total, "scope": "bounded-correctness",
        "production_scale": False, "claims": [], "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--suite-manifest", type=Path, required=True)
    parser.add_argument("--suite-manifest-sha256", required=True)
    parser.add_argument("--corpus-manifest", type=Path, required=True)
    parser.add_argument("--corpus-manifest-sha256", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--suite-log", action="append", nargs=2, metavar=("NAME", "PATH"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = build_receipt(
            args.source_commit, args.requirement, args.suite_manifest.read_bytes(),
            args.suite_manifest_sha256, args.corpus_manifest.read_bytes(),
            args.corpus_manifest_sha256, args.platform, args.toolchain,
            [(name, Path(path).read_bytes()) for name, path in args.suite_log],
            args.root,
        )
    except (GateFailure, OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"native G4 receipt failed: {error}")
        return 2
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
