#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Create and verify exact-source PyPI distribution receipts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


PROJECT = "hyphae-sdk"
REPOSITORIES = {
    "pypi": "https://pypi.org/pypi/hyphae-sdk",
    "testpypi": "https://test.pypi.org/pypi/hyphae-sdk",
}


class PythonReceiptError(ValueError):
    """A Python distribution receipt or registry response is invalid."""


def fail(message: str) -> None:
    raise PythonReceiptError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def build_receipt(
    directory: Path,
    version: str,
    source_tag: str,
    source_commit: str,
) -> dict[str, Any]:
    if source_tag != f"v{version}":
        fail("Python version and immutable source tag differ")
    if re.fullmatch(r"[0-9a-f]{40}", source_commit) is None:
        fail("source commit must be one full lowercase Git object ID")
    files = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.name != ".gitignore"
    )
    expected = {
        f"hyphae_sdk-{version}-py3-none-any.whl",
        f"hyphae_sdk-{version}.tar.gz",
    }
    if {path.name for path in files} != expected:
        fail("receipt input must contain the exact wheel and sdist")
    return {
        "schema_version": 1,
        "status": "built",
        "project": PROJECT,
        "version": version,
        "source_tag": source_tag,
        "source_commit": source_commit,
        "files": [
            {"filename": path.name, "sha256": sha256(path), "bytes": path.stat().st_size}
            for path in files
        ],
    }


def validate_receipt(receipt: dict[str, Any]) -> tuple[str, dict[str, str]]:
    if set(receipt) != {
        "schema_version",
        "status",
        "project",
        "version",
        "source_tag",
        "source_commit",
        "files",
    }:
        fail("Python distribution receipt has unknown or missing fields")
    version = receipt.get("version")
    source_commit = receipt.get("source_commit")
    if (
        receipt.get("schema_version") != 1
        or receipt.get("status") != "built"
        or receipt.get("project") != PROJECT
        or not isinstance(version, str)
        or re.fullmatch(r"\d+\.\d+\.\d+", version) is None
        or receipt.get("source_tag") != f"v{version}"
        or not isinstance(source_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", source_commit) is None
    ):
        fail("Python distribution receipt identity is invalid")
    files = receipt.get("files")
    if not isinstance(files, list) or len(files) != 2:
        fail("Python distribution receipt must bind exactly two files")
    expected_names = {
        f"hyphae_sdk-{version}-py3-none-any.whl",
        f"hyphae_sdk-{version}.tar.gz",
    }
    expected: dict[str, str] = {}
    for entry in files:
        if not isinstance(entry, dict) or set(entry) != {"filename", "sha256", "bytes"}:
            fail("Python distribution receipt file entry is invalid")
        filename = entry.get("filename")
        digest = entry.get("sha256")
        size = entry.get("bytes")
        if (
            not isinstance(filename, str)
            or filename not in expected_names
            or not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", digest) is None
            or isinstance(size, bool)
            or not isinstance(size, int)
            or size <= 0
        ):
            fail("Python distribution receipt file identity is invalid")
        if filename in expected:
            fail("Python distribution receipt contains a duplicate file")
        expected[filename] = digest
    if set(expected) != expected_names:
        fail("Python distribution receipt filenames are incomplete")
    return version, expected


def verify_registry(
    receipt: dict[str, Any],
    release: dict[str, Any],
    repository: str,
    python_versions: tuple[str, ...] = (),
) -> dict[str, Any]:
    version, expected = validate_receipt(receipt)
    if repository not in REPOSITORIES:
        fail("unknown Python package repository")
    info = release.get("info")
    if not isinstance(info, dict) or info.get("name") != PROJECT or info.get("version") != version:
        fail("registry project/version differs from the receipt")
    urls = release.get("urls")
    if not isinstance(urls, list):
        fail("registry release file inventory is invalid")
    actual: dict[object, object] = {}
    for entry in urls:
        if not isinstance(entry, dict) or not isinstance(entry.get("digests"), dict):
            fail("registry release file entry is invalid")
        filename = entry.get("filename")
        if filename in actual:
            fail("registry release contains a duplicate filename")
        actual[filename] = entry["digests"].get("sha256")
    if expected != actual:
        fail("registry filenames or SHA-256 digests differ from built distributions")
    verified = dict(receipt)
    verified["status"] = "published"
    verified["repository"] = repository
    verified["verified_python_versions"] = list(python_versions)
    return verified


def fetch_release(repository: str, version: str, attempts: int = 12) -> dict[str, Any]:
    url = f"{REPOSITORIES[repository]}/{version}/json"
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=20) as response:
                value = json.load(response)
            if not isinstance(value, dict):
                fail("registry response must be one JSON object")
            return value
        except urllib.error.HTTPError as error:
            if error.code != 404 or attempt + 1 == attempts:
                raise
        if attempt + 1 < attempts:
            time.sleep(5)
    fail("registry release did not become visible")


def write_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    build = subcommands.add_parser("build")
    build.add_argument("--directory", type=Path, required=True)
    build.add_argument("--version", required=True)
    build.add_argument("--source-tag", required=True)
    build.add_argument("--source-commit", required=True)
    build.add_argument("--output", type=Path, required=True)
    verify = subcommands.add_parser("verify")
    verify.add_argument("--receipt", type=Path, required=True)
    verify.add_argument("--repository", choices=sorted(REPOSITORIES), required=True)
    verify.add_argument("--python-version", action="append", default=[])
    verify.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "build":
        receipt = build_receipt(args.directory, args.version, args.source_tag, args.source_commit)
    else:
        receipt = json.loads(args.receipt.read_text(encoding="utf-8"))
        release = fetch_release(args.repository, receipt["version"])
        receipt = verify_registry(
            receipt,
            release,
            args.repository,
            tuple(args.python_version),
        )
    write_json(args.output, receipt)
    print(json.dumps(receipt, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
