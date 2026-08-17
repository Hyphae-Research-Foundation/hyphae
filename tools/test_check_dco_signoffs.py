#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_dco_signoffs import ADOPTION_COMMIT, validate_range


class DcoSignoffTests(unittest.TestCase):
    def test_range_requires_matching_signoff_and_documents_bot_exception(self) -> None:
        calls: list[tuple[str, ...]] = []

        def fake_git(_root: Path, *arguments: str) -> str:
            calls.append(arguments)
            if arguments[:2] == ("rev-parse", "--verify"):
                return f"{ADOPTION_COMMIT}\n"
            if arguments[:1] == ("merge-base",) and "--is-ancestor" not in arguments:
                return "base\n"
            if arguments[:1] == ("rev-list",):
                return "good\nmissing\n"
            if arguments[-1] == "good" and "--format=%an <%ae>" in arguments:
                return "Legal Name <legal@example.com>\n"
            if arguments[-1] == "missing" and "--format=%an <%ae>" in arguments:
                return "Other Name <other@example.com>\n"
            if arguments[-1] == "good":
                return "subject\n\nSigned-off-by: Legal Name <legal@example.com>\n"
            if arguments[-1] == "missing":
                return "subject without trailer\n"
            return ""

        completed = subprocess.CompletedProcess([], 0, b"", b"")
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.check_dco_signoffs.git", side_effect=fake_git
        ), patch("tools.check_dco_signoffs.subprocess.run", return_value=completed):
            failures = validate_range("base", "head", Path(directory))
        self.assertEqual(len(failures), 1)
        self.assertIn("missing", failures[0])
        self.assertTrue(calls)

    def test_adoption_commit_itself_is_outside_the_required_range(self) -> None:
        def fake_git(_root: Path, *arguments: str) -> str:
            if arguments[:2] == ("rev-parse", "--verify"):
                return f"{ADOPTION_COMMIT}\n"
            if arguments[:1] == ("merge-base",):
                return "base\n"
            if arguments[:1] == ("rev-list",):
                return f"{ADOPTION_COMMIT}\n"
            raise AssertionError(arguments)

        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.check_dco_signoffs.git", side_effect=fake_git
        ):
            self.assertEqual(validate_range("base", "head", Path(directory)), [])

    def test_bot_signoff_must_match_the_exact_bot_identity(self) -> None:
        def fake_git(_root: Path, *arguments: str) -> str:
            if arguments[:2] == ("rev-parse", "--verify"):
                return f"{ADOPTION_COMMIT}\n"
            if arguments[:1] == ("merge-base",) and "--is-ancestor" not in arguments:
                return "base\n"
            if arguments[:1] == ("rev-list",):
                return "bot\n"
            if arguments[-1] == "bot" and "--format=%an <%ae>" in arguments:
                return "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>\n"
            if arguments[-1] == "bot":
                return "update\n\nSigned-off-by: Dependabot <wrong@example.com>\n"
            raise AssertionError(arguments)

        completed = subprocess.CompletedProcess([], 0, "", "")
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.check_dco_signoffs.git", side_effect=fake_git
        ), patch("tools.check_dco_signoffs.subprocess.run", return_value=completed):
            failures = validate_range("base", "head", Path(directory))
        self.assertEqual(len(failures), 1)
        self.assertIn("approved bot sign-off", failures[0])

    def test_shallow_history_fails_instead_of_silently_skipping_commits(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            origin = root / "origin"
            clone = root / "clone"
            subprocess.run(["git", "init", "-q", str(origin)], check=True)
            subprocess.run(["git", "-C", str(origin), "config", "user.name", "Owner"], check=True)
            subprocess.run(
                ["git", "-C", str(origin), "config", "user.email", "owner@example.test"],
                check=True,
            )
            (origin / "content").write_text("adoption\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(origin), "add", "content"], check=True)
            subprocess.run(
                ["git", "-C", str(origin), "commit", "-qm", "adoption"], check=True
            )
            adoption = subprocess.check_output(
                ["git", "-C", str(origin), "rev-parse", "HEAD"], text=True
            ).strip()
            (origin / "content").write_text("signed\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(origin), "add", "content"], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(origin),
                    "commit",
                    "-qm",
                    "signed\n\nSigned-off-by: Owner <owner@example.test>",
                ],
                check=True,
            )
            head = subprocess.check_output(
                ["git", "-C", str(origin), "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(
                ["git", "clone", "-q", "--depth=1", origin.as_uri(), str(clone)],
                check=True,
            )
            with patch("tools.check_dco_signoffs.ADOPTION_COMMIT", adoption):
                with self.assertRaises(subprocess.CalledProcessError):
                    validate_range(adoption, head, clone)
                subprocess.run(
                    ["git", "-C", str(clone), "fetch", "-q", "--unshallow", "origin"],
                    check=True,
                )
                self.assertEqual(validate_range(adoption, head, clone), [])


if __name__ == "__main__":
    unittest.main()
