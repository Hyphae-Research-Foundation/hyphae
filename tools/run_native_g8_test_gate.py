#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Execute fixed corruption or migration suites and emit exact-SHA G8 evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SUITES = {
    "corruption-matrix": (
        (
            "pages-wal-manifest-blobs",
            "cargo", "test", "--locked", "-p", "hyphae-native-product",
            "--test", "administration_surfaces",
            "doctor_rejects_format_page_wal_manifest_and_blob_corruption", "--", "--exact",
        ),
        (
            "indexes",
            "cargo", "test", "--locked", "-p", "hyphae-native-runtime",
            "--test", "search_maintenance_g4",
            "structured_corruption_matrix_has_zero_silent_acceptance_or_partial_writes", "--", "--exact",
        ),
        (
            "proof-envelope-payload",
            "cargo", "test", "--locked", "-p", "hyphae-native-product",
            "--test", "native_proof_g6", "envelope_and_payload_tampering_is_rejected", "--", "--exact",
        ),
        (
            "proof-truncation",
            "cargo", "test", "--locked", "-p", "hyphae-native-product",
            "--test", "native_proof_g6", "every_proof_and_witness_truncation_is_rejected", "--", "--exact",
        ),
    ),
    "format2-to-native-migration": (
        (
            "equivalence-and-promotion",
            "cargo", "test", "--locked", "-p", "hyphae-cli", "--test", "native_cli",
            "format2_migration_runs_verifies_promotes_and_keeps_source_unchanged", "--", "--exact",
        ),
        (
            "overlap-and-rollback",
            "cargo", "test", "--locked", "-p", "hyphae-cli", "--test", "native_cli",
            "migration_rejects_source_output_overlap_and_rolls_back_pending_target", "--", "--exact",
        ),
    ),
}


def sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def verify_source(commit: str) -> str:
    if len(commit) != 40 or any(character not in "0123456789abcdef" for character in commit):
        raise ValueError("source commit must be a canonical lowercase SHA-1")
    head = subprocess.run(
        ("git", "rev-parse", "HEAD"), cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.strip()
    if head != commit:
        raise RuntimeError("source commit differs from checked-out HEAD")
    dirty = subprocess.run(
        ("git", "status", "--porcelain", "--untracked-files=no"),
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout.strip()
    if dirty:
        raise RuntimeError("tracked source worktree must be clean")
    return subprocess.run(
        ("git", "rev-parse", "HEAD^{tree}"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()


def execute(requirement: str, platform: str, commit: str) -> dict:
    source_tree = verify_source(commit)
    checks = []
    for row in SUITES[requirement]:
        name, *command = row
        started = time.monotonic()
        completed = subprocess.run(
            command, cwd=ROOT, check=False, capture_output=True, text=True, timeout=900
        )
        check = {
            "name": name,
            "command": list(command),
            "status": "passed" if completed.returncode == 0 else "failed",
            "exit_code": completed.returncode,
            "duration_millis": round((time.monotonic() - started) * 1000),
            "stdout_sha256": sha256(completed.stdout),
            "stderr_sha256": sha256(completed.stderr),
        }
        checks.append(check)
        if completed.returncode != 0:
            raise RuntimeError(
                f"{requirement}/{name} failed: {completed.stderr[-2000:]}"
            )
    return {
        "schema": "hyphae-native-g8-fixed-suite-v1",
        "status": "passed",
        "source_commit": commit,
        "source_tree": source_tree,
        "requirement": requirement,
        "platform": platform,
        "checks": checks,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--requirement", choices=tuple(SUITES), required=True)
    parser.add_argument("--platform", choices=("linux", "macos", "windows"), required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    result = execute(arguments.requirement, arguments.platform, arguments.source_commit)
    arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
