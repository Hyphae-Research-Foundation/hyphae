#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run relicensing checks without changing evidence unless explicitly requested."""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKS = (
    "tools/check_license_policy.py",
    "tools/check_dependency_license_aggregate.py",
    "tools/check_relicensing_preflight.py",
)
TRANSITION_CHECK = "tools/check_relicensing_transition.py"


def repository_state() -> str:
    inventory = subprocess.run(
        [
            "git",
            "-C",
            str(ROOT),
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout
    digest = hashlib.sha256(inventory)
    for relative in sorted(
        path
        for path in inventory.decode("utf-8", errors="surrogateescape").split("\0")
        if len(path) >= 4
    ):
        status_path = relative[3:]
        if " -> " in status_path:
            status_path = status_path.rsplit(" -> ", 1)[1]
        path = ROOT / status_path
        if path.is_file() and not path.is_symlink():
            digest.update(status_path.encode("utf-8", errors="surrogateescape"))
            digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def _run_checks(checks: tuple[str, ...]) -> int:
    for check in checks:
        completed = subprocess.run([sys.executable, check], cwd=ROOT, check=False)
        if completed.returncode != 0:
            return completed.returncode
    return 0


def _validate_readonly() -> int:
    state = repository_state()
    result = _run_checks((*CHECKS, TRANSITION_CHECK))
    if state != repository_state():
        print("error: read-only relicensing checks changed the repository", file=sys.stderr)
        return 1
    return result


def _refresh() -> int:
    result = _run_checks(CHECKS[:2])
    if result != 0:
        return result
    completed = subprocess.run(
        [sys.executable, TRANSITION_CHECK, "--refresh"],
        cwd=ROOT,
        check=False,
    )
    if completed.returncode != 0:
        return completed.returncode
    state = repository_state()
    result = _run_checks((CHECKS[2], TRANSITION_CHECK))
    if state != repository_state():
        print("error: repository changed after final transition refresh", file=sys.stderr)
        return 1
    return result


def main(arguments: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--readonly",
        action="store_true",
        help="validate without refreshing evidence (the default)",
    )
    mode.add_argument(
        "--refresh",
        action="store_true",
        help="explicitly refresh the transition receipt after prerequisite checks",
    )
    args = parser.parse_args(arguments)
    if args.refresh:
        return _refresh()
    return _validate_readonly()


if __name__ == "__main__":
    raise SystemExit(main())
