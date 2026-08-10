#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Validate local documentation links, coverage, examples, and CLI drift."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import urllib.parse
from pathlib import Path

LINK = re.compile(r"(?<!!)\[[^\]]+\]\(([^)]+)\)")
HEADING = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$")
TOP_START = "<!-- cli-commands:start -->"
TOP_END = "<!-- cli-commands:end -->"
REMOTE_START = "<!-- remote-commands:start -->"
REMOTE_END = "<!-- remote-commands:end -->"
IGNORED_DIRECTORIES = {".git", "node_modules", "target"}


def markdown_files(root: Path) -> list[Path]:
    return sorted(
        path
        for path in root.rglob("*.md")
        if not any(part in IGNORED_DIRECTORIES for part in path.relative_to(root).parts)
    )


def github_anchors(path: Path) -> set[str]:
    anchors: set[str] = set()
    counts: dict[str, int] = {}
    fenced = False
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        if raw_line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        match = HEADING.match(raw_line)
        if match is None:
            continue
        heading = re.sub(r"<[^>]+>", "", match.group(1))
        heading = heading.replace("`", "").strip().lower()
        slug = re.sub(r"[^\w\- ]", "", heading, flags=re.UNICODE)
        slug = re.sub(r"\s+", "-", slug)
        duplicate = counts.get(slug, 0)
        counts[slug] = duplicate + 1
        anchors.add(slug if duplicate == 0 else f"{slug}-{duplicate}")
    return anchors


def link_targets(path: Path) -> list[str]:
    targets: list[str] = []
    fenced = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if not fenced:
            targets.extend(match.group(1).strip() for match in LINK.finditer(line))
    return targets


def validate_links(root: Path, files: list[Path]) -> list[str]:
    errors: list[str] = []
    for source in files:
        for target in link_targets(source):
            if target.startswith(("https://", "http://", "mailto:")):
                continue
            target = target.strip("<>")
            raw_path, separator, raw_anchor = target.partition("#")
            decoded = urllib.parse.unquote(raw_path)
            destination = source if decoded == "" else (source.parent / decoded)
            destination = destination.resolve()
            try:
                destination.relative_to(root.resolve())
            except ValueError:
                errors.append(f"{source.relative_to(root)}: link escapes repository: {target}")
                continue
            if not destination.exists():
                errors.append(f"{source.relative_to(root)}: missing link target: {target}")
                continue
            if separator and raw_anchor and destination.suffix.lower() == ".md":
                anchor = urllib.parse.unquote(raw_anchor).lower()
                if anchor not in github_anchors(destination):
                    errors.append(
                        f"{source.relative_to(root)}: missing heading #{raw_anchor} in "
                        f"{destination.relative_to(root)}"
                    )
    return errors


def validate_index(root: Path) -> list[str]:
    docs = root / "docs"
    hub = docs / "README.md"
    indexed = {
        urllib.parse.unquote(target.partition("#")[0])
        for target in link_targets(hub)
        if not target.startswith(("https://", "http://", "mailto:", "#", "../"))
    }
    errors: list[str] = []
    for path in sorted(docs.rglob("*.md")):
        if path == hub:
            continue
        relative = path.relative_to(docs).as_posix()
        if relative not in indexed:
            errors.append(f"docs/README.md: documentation page is not indexed: {relative}")
    return errors


def validate_json_examples(root: Path) -> list[str]:
    errors: list[str] = []
    for path in sorted((root / "examples").rglob("*.json")):
        try:
            json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            errors.append(f"{path.relative_to(root)}: invalid JSON: {error}")
    return errors


def help_commands(binary: Path, prefix: list[str]) -> list[str]:
    completed = subprocess.run(
        [str(binary), *prefix, "--help"],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    commands: list[str] = []
    inside = False
    for line in completed.stdout.splitlines():
        if line == "Commands:":
            inside = True
            continue
        if inside and line == "":
            break
        if inside:
            match = re.match(r"^\s{2}([a-z][a-z0-9-]*)\s", line)
            if match is not None and match.group(1) != "help":
                commands.append(match.group(1))
    return commands


def marked_commands(document: str, start: str, end: str) -> list[str]:
    try:
        section = document.split(start, 1)[1].split(end, 1)[0]
    except IndexError as error:
        raise ValueError(f"missing documentation markers {start} / {end}") from error
    return re.findall(r"`([a-z][a-z0-9-]*)(?:\s[^`]*)?`", section)


def validate_cli(root: Path, binary: Path) -> list[str]:
    document = (root / "docs/cli/reference.md").read_text(encoding="utf-8")
    errors: list[str] = []
    try:
        documented_top = marked_commands(document, TOP_START, TOP_END)
        documented_remote = marked_commands(document, REMOTE_START, REMOTE_END)
    except ValueError as error:
        return [str(error)]
    actual_top = help_commands(binary, [])
    actual_remote = help_commands(binary, ["remote"])
    if documented_top != actual_top:
        errors.append(
            f"CLI command drift: documented={documented_top!r}, actual={actual_top!r}"
        )
    if documented_remote != actual_remote:
        errors.append(
            "remote CLI command drift: "
            f"documented={documented_remote!r}, actual={actual_remote!r}"
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary",
        type=Path,
        help="built hyphae executable used to verify the documented command inventory",
    )
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    files = markdown_files(root)
    errors = validate_links(root, files)
    errors.extend(validate_index(root))
    errors.extend(validate_json_examples(root))
    if arguments.binary is not None:
        binary = arguments.binary.resolve()
        if not binary.is_file():
            errors.append(f"Hyphae binary does not exist: {binary}")
        else:
            errors.extend(validate_cli(root, binary))
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(
        f"documentation ok: {len(files)} Markdown files, "
        f"{len(list((root / 'examples').rglob('*.json')))} JSON examples"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
