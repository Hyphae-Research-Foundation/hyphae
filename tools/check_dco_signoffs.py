#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Require DCO sign-offs on commits introduced after policy adoption."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ADOPTION_COMMIT = "fcf2f918e1539cfb7d67fd52abf0c7d57169ec18"
SIGNOFF = re.compile(r"(?m)^Signed-off-by: ([^<>\n]+) <([^<>\s]+@[^<>\s]+)>$")
BOT_AUTHORS = {
    "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>",
}


def git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=30,
    ).stdout


def validate_range(base: str, head: str, root: Path = ROOT) -> list[str]:
    git(root, "rev-parse", "--verify", f"{ADOPTION_COMMIT}^{{commit}}")
    merge_base = git(root, "merge-base", base, head).strip()
    commits = git(root, "rev-list", "--reverse", f"{merge_base}..{head}").splitlines()
    failures: list[str] = []
    for commit in commits:
        if commit == ADOPTION_COMMIT:
            continue
        ancestry = subprocess.run(
            ["git", "-C", str(root), "merge-base", "--is-ancestor", ADOPTION_COMMIT, commit],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
        )
        if ancestry.returncode == 1:
            continue
        if ancestry.returncode != 0:
            raise subprocess.CalledProcessError(
                ancestry.returncode,
                ancestry.args,
                output=ancestry.stdout,
                stderr=ancestry.stderr,
            )
        author = git(root, "show", "-s", "--format=%an <%ae>", commit).strip()
        message = git(root, "show", "-s", "--format=%B", commit)
        signoffs = SIGNOFF.findall(message)
        if not signoffs:
            label = "approved bot commit" if author in BOT_AUTHORS else "commit"
            failures.append(f"{commit}: {label} lacks Signed-off-by trailer")
            continue
        author_name, separator, author_email = author.rpartition(" <")
        author_email = author_email.removesuffix(">") if separator else ""
        if not any(
            name.strip() == author_name.strip() and email.strip() == author_email
            for name, email in signoffs
        ):
            label = "approved bot sign-off" if author in BOT_AUTHORS else "sign-off"
            failures.append(f"{commit}: {label} does not match commit author identity")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    arguments = parser.parse_args()
    try:
        failures = validate_range(arguments.base, arguments.head)
    except subprocess.SubprocessError as error:
        print(f"error: DCO range inventory failed: {error}", file=sys.stderr)
        return 1
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("DCO sign-off check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
