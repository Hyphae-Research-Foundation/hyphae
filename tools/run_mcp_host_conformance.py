#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run one pinned in-repository MCP host adapter and emit exact-source evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
HOSTS_DIR = ROOT / "conformance/mcp/hosts"
ADAPTERS = {
    "codex": ROOT / "conformance/mcp/adapters/codex.py",
    "claude-code": ROOT / "conformance/mcp/adapters/claude_code.py",
}
HOST_BINARIES = {"codex": "codex", "claude-code": "claude"}
TRANSCRIPT_KEYS = {
    "schema",
    "host",
    "host_version",
    "host_platform",
    "host_executable_sha256",
    "installed_mcp_config_sha256",
    "tools",
    "cases",
}
CREDENTIAL = re.compile(rb"hyp1_[A-Za-z0-9_-]{16,}")


class MissingEvidence(RuntimeError):
    """The installed host did not expose complete deterministic evidence."""


def digest(path: Path) -> str:
    if not path.is_file():
        raise MissingEvidence(f"required evidence input is missing: {path}")
    return hashlib.sha256(path.read_bytes()).hexdigest()


def adapter_digest(host: str, root: Path = ROOT) -> str:
    hasher = hashlib.sha256()
    paths = [root / "conformance/mcp/adapters/common.py", root / ADAPTERS[host].relative_to(ROOT)]
    for path in paths:
        data = path.read_bytes()
        hasher.update(path.name.encode())
        hasher.update(b"\0")
        hasher.update(len(data).to_bytes(8, "big"))
        hasher.update(data)
    return hasher.hexdigest()


def git(root: Path, *arguments: str, environment: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=root,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def worktree_tree(root: Path) -> str:
    with tempfile.TemporaryDirectory(prefix="hyphae-mcp-source-") as directory:
        index = Path(directory) / "index"
        environment = {**os.environ, "GIT_INDEX_FILE": str(index)}
        git(root, "read-tree", "HEAD", environment=environment)
        git(root, "add", "--all", environment=environment)
        return git(root, "write-tree", environment=environment)


def git_identity(root: Path, allow_integration_tree: bool) -> tuple[str, str, str]:
    commit = git(root, "rev-parse", "HEAD")
    status = git(root, "status", "--porcelain=v1", "--untracked-files=all")
    if not status:
        return commit, git(root, "rev-parse", "HEAD^{tree}"), "clean"
    if not allow_integration_tree:
        raise MissingEvidence(
            "source worktree is not clean; pass --allow-integration-tree to bind the exact current bytes"
        )
    return commit, worktree_tree(root), "integration"


def load_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise MissingEvidence(f"invalid evidence JSON {path}") from error
    if not isinstance(value, dict):
        raise MissingEvidence(f"deterministic evidence is not one JSON object: {path}")
    return value


def package_evidence(host: str, root: Path = ROOT) -> dict[str, str]:
    install_lock = load_object(root / "conformance/mcp/hosts/install-lock.json")
    package_lock = load_object(root / "conformance/mcp/hosts/package-lock.json")
    host_lock = install_lock.get("hosts", {}).get(host)
    if not isinstance(host_lock, dict):
        raise MissingEvidence("host install lock entry is missing")
    package_name = host_lock.get("package")
    package = package_lock.get("packages", {}).get(f"node_modules/{package_name}")
    if not isinstance(package, dict):
        raise MissingEvidence("host package entry is missing from package-lock.json")
    version = host_lock.get("version")
    integrity = package.get("integrity")
    if package.get("version") != version or not isinstance(integrity, str):
        raise MissingEvidence("host package identity differs between install and npm locks")
    return {
        "host_package_name": str(package_name),
        "host_package_version": str(version),
        "host_package_integrity": integrity,
    }


def selected_pointer(value: Any, pointer: str) -> Any:
    selected = value
    for raw_component in pointer.split("/")[1:]:
        component = raw_component.replace("~1", "/").replace("~0", "~")
        if not isinstance(selected, dict) or component not in selected:
            raise MissingEvidence("host result assertion is missing")
        selected = selected[component]
    return selected


def validate_transcript(value: dict[str, Any], host: str, corpus: dict[str, Any]) -> None:
    if set(value) != TRANSCRIPT_KEYS:
        raise MissingEvidence("host transcript has missing or unknown evidence fields")
    if value.get("schema") != "hyphae-mcp-host-transcript-v1" or value.get("host") != host:
        raise MissingEvidence("host transcript identity is invalid")
    if not isinstance(value.get("host_version"), str) or not value["host_version"]:
        raise MissingEvidence("host version evidence is missing")
    for field in ("host_executable_sha256", "installed_mcp_config_sha256"):
        observed = value.get(field)
        if not isinstance(observed, str) or re.fullmatch(r"[0-9a-f]{64}", observed) is None:
            raise MissingEvidence(f"host transcript {field} is invalid")
    if value.get("tools") != corpus["tools"]:
        raise MissingEvidence("host tool discovery evidence differs from the shared corpus")
    expected_cases = [case["id"] for case in corpus["cases"]]
    cases = value.get("cases")
    if not isinstance(cases, list) or [case.get("id") for case in cases if isinstance(case, dict)] != expected_cases:
        raise MissingEvidence("host case evidence is missing, reordered, or incomplete")
    for expected, observed in zip(corpus["cases"], cases, strict=True):
        if not isinstance(observed, dict) or set(observed) != {
            "id",
            "tool",
            "arguments",
            "outcome",
            "result",
        }:
            raise MissingEvidence("host case evidence has missing or unknown fields")
        if observed.get("tool") != expected["tool"] or observed.get("arguments") != expected["arguments"]:
            raise MissingEvidence(f"host case input drifted: {expected['id']}")
        if observed.get("outcome") != expected["expect"]:
            raise MissingEvidence(f"host case did not satisfy the corpus: {expected['id']}")
        assertion = expected["assert"]
        try:
            selected = selected_pointer(observed.get("result"), assertion["pointer"])
        except MissingEvidence as error:
            raise MissingEvidence(f"host result assertion is missing: {expected['id']}") from error
        if "equals" in assertion and selected != assertion["equals"]:
            raise MissingEvidence(f"host result assertion differs: {expected['id']}")
        if assertion.get("type") == "integer" and type(selected) is not int:
            raise MissingEvidence(f"host result assertion has wrong type: {expected['id']}")


def credential_canary() -> bytes | None:
    path = os.environ.get("HYPHAE_NATIVE_API_KEY_FILE")
    if not path:
        return None
    try:
        return Path(path).read_bytes().strip()
    except OSError as error:
        raise MissingEvidence("Native API-key file is unavailable") from error


def require_secret_free(value: bytes, canary: bytes | None) -> None:
    if (canary and canary in value) or CREDENTIAL.search(value):
        raise MissingEvidence("host adapter output contained credential material")


def run(
    host: str,
    output: Path,
    transcript: Path,
    root: Path = ROOT,
    allow_integration_tree: bool = False,
) -> dict[str, Any]:
    adapter = root / ADAPTERS[host].relative_to(ROOT)
    common = root / "conformance/mcp/adapters/common.py"
    executable = root / f"conformance/mcp/hosts/node_modules/.bin/{HOST_BINARIES[host]}"
    if not adapter.is_file() or not common.is_file():
        raise MissingEvidence(f"in-repository {host} adapter is missing")
    if not executable.exists() or executable.name != HOST_BINARIES[host]:
        raise MissingEvidence(f"repository-installed {HOST_BINARIES[host]} host is missing; run npm ci")
    config = root / "plugins/hyphae/.mcp.json"
    corpus_path = root / "conformance/mcp/corpus.json"
    corpus = load_object(corpus_path)
    transcript.parent.mkdir(parents=True, exist_ok=True)
    environment = {
        **os.environ,
        "HYPHAE_MCP_HOST_EXECUTABLE": str(executable.absolute()),
        "HYPHAE_MCP_CONFIG": str(config),
        "HYPHAE_MCP_CORPUS": str(corpus_path),
        "HYPHAE_MCP_TRANSCRIPT": str(transcript),
    }
    completed = subprocess.run(
        [sys.executable, str(adapter)],
        cwd=root,
        env=environment,
        capture_output=True,
        timeout=180,
    )
    canary = credential_canary()
    require_secret_free(completed.stdout + completed.stderr, canary)
    if completed.returncode != 0:
        diagnostic = completed.stderr.decode("utf-8", errors="replace").strip()[:1000]
        raise MissingEvidence(
            f"{host} adapter did not produce passing deterministic evidence: {diagnostic}"
        )
    if not transcript.is_file():
        raise MissingEvidence(f"{host} adapter did not write the required transcript")
    require_secret_free(transcript.read_bytes(), canary)
    transcript_value = load_object(transcript)
    validate_transcript(transcript_value, host, corpus)
    commit, tree, source_mode = git_identity(root, allow_integration_tree)
    receipt = {
        "schema": "hyphae-mcp-host-conformance-receipt-v1",
        "status": "passed",
        "host": host,
        "host_version": transcript_value["host_version"],
        "host_platform": transcript_value["host_platform"],
        "source_commit": commit,
        "source_tree": tree,
        "source_mode": source_mode,
        "adapter_sha256": adapter_digest(host, root),
        "host_executable_sha256": transcript_value["host_executable_sha256"],
        **package_evidence(host, root),
        "host_lock_sha256": digest(root / "conformance/mcp/hosts/install-lock.json"),
        "host_package_lock_sha256": digest(root / "conformance/mcp/hosts/package-lock.json"),
        "mcp_config_sha256": digest(config),
        "installed_mcp_config_sha256": transcript_value["installed_mcp_config_sha256"],
        "corpus_sha256": digest(corpus_path),
        "tool_schema_version": corpus["tool_schema_version"],
        "tools": corpus["tools"],
        "cases": [case["id"] for case in corpus["cases"]],
        "transcript_sha256": digest(transcript),
    }
    encoded = (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode()
    require_secret_free(encoded, canary)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(encoded)
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", choices=sorted(HOST_BINARIES), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--transcript", type=Path, required=True)
    parser.add_argument("--allow-integration-tree", action="store_true")
    arguments = parser.parse_args()
    try:
        receipt = run(
            arguments.host,
            arguments.output,
            arguments.transcript,
            allow_integration_tree=arguments.allow_integration_tree,
        )
    except (MissingEvidence, OSError, subprocess.SubprocessError) as error:
        print(
            json.dumps(
                {"status": "missing_evidence", "host": arguments.host, "error": str(error)},
                sort_keys=True,
            )
        )
        return 2
    print(json.dumps(receipt, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
