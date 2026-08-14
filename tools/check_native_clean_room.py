#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed validation for the native clean-room provenance ledger."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path
from typing import Any

SCHEMA = "hyphae-native-clean-room-profile-v1"
RECEIPT_SCHEMA = "hyphae-native-clean-room-receipt-v1"
SHA40 = re.compile(r"[0-9a-f]{40}\Z")
DECISIONS = {"Exclude", "Rewrite", "Defer"}


class GateFailure(RuntimeError):
    """The clean-room profile or repository state is invalid."""


def validate_profile(root: Path, profile: dict[str, Any]) -> dict[str, Any]:
    if profile.get("schema") != SCHEMA:
        raise GateFailure("unsupported clean-room profile")
    ledger_path = profile.get("ledger")
    if not isinstance(ledger_path, str) or Path(ledger_path).is_absolute():
        raise GateFailure("ledger must be repository-relative")
    ledger = root / ledger_path
    if not ledger.is_file():
        raise GateFailure("clean-room ledger is missing")
    text = ledger.read_text(encoding="utf-8")
    if "## Accepted ports\n\nNone." not in text:
        raise GateFailure("accepted ports must remain an explicit None declaration")
    inputs = profile.get("historical_inputs")
    if not isinstance(inputs, list) or not inputs:
        raise GateFailure("historical inputs must be nonempty")
    seen: set[str] = set()
    for entry in inputs:
        if not isinstance(entry, dict) or set(entry) != {"source", "revision", "decision"}:
            raise GateFailure("invalid historical input fields")
        source = entry.get("source")
        revision = entry.get("revision")
        decision = entry.get("decision")
        if not isinstance(source, str) or not source or source in seen:
            raise GateFailure("historical input source must be unique")
        if not isinstance(revision, str) or SHA40.fullmatch(revision) is None:
            raise GateFailure(f"invalid immutable revision for {source}")
        if decision not in DECISIONS:
            raise GateFailure(f"invalid clean-room decision for {source}")
        if source not in text or revision not in text:
            raise GateFailure(f"ledger does not bind historical input {source}")
        seen.add(source)
    reviewers = profile.get("human_reviewers")
    if not isinstance(reviewers, list) or not reviewers:
        raise GateFailure("human clean-room review is required")
    for reviewer in reviewers:
        if not isinstance(reviewer, dict) or set(reviewer) != {
            "github_login", "reviewed_commit", "scope", "decision"
        }:
            raise GateFailure("invalid human reviewer record")
        if not isinstance(reviewer.get("github_login"), str) or not reviewer["github_login"]:
            raise GateFailure("reviewer login is required")
        if not isinstance(reviewer.get("reviewed_commit"), str) or SHA40.fullmatch(reviewer["reviewed_commit"]) is None:
            raise GateFailure("reviewed commit must be immutable")
        if reviewer.get("decision") != "approved":
            raise GateFailure("clean-room reviewer must explicitly approve")
        if not isinstance(reviewer.get("scope"), str) or not reviewer["scope"]:
            raise GateFailure("clean-room review scope is required")
    return {
        "ledger": ledger_path,
        "ledger_sha256": hashlib.sha256(ledger.read_bytes()).hexdigest(),
        "historical_inputs": len(inputs),
        "human_reviewers": len(reviewers),
    }


def build_receipt(root: Path, profile_path: Path) -> dict[str, Any]:
    profile_bytes = profile_path.read_bytes()
    profile = json.loads(profile_bytes)
    summary = validate_profile(root, profile)
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    reviewed = {entry["reviewed_commit"] for entry in profile["human_reviewers"]}
    if not all(
        subprocess.run(
            ["git", "merge-base", "--is-ancestor", revision, commit],
            cwd=root,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0
        for revision in reviewed
    ):
        raise GateFailure("human review is not bound to an ancestor of the evaluated commit")
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "passed",
        "source_commit": commit,
        "profile_sha256": hashlib.sha256(profile_bytes).hexdigest(),
        **summary,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    profile = args.profile if args.profile.is_absolute() else root / args.profile
    try:
        receipt = build_receipt(root, profile)
    except (OSError, json.JSONDecodeError, subprocess.CalledProcessError, GateFailure) as error:
        print(f"native clean-room gate failed: {error}")
        return 1
    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
