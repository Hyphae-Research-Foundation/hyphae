#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed checker for pinned real Codex and Claude Code MCP receipts."""

from __future__ import annotations

import argparse
import platform
import json
import re
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tools.run_mcp_host_conformance import (
    ROOT,
    TRANSCRIPT_KEYS,
    adapter_digest,
    digest,
    git_identity,
    load_object,
    package_evidence,
    validate_transcript,
)


HOSTS = ("claude-code", "codex")
EXPECTED_TOOLS = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
    "hyphae_native_search_lexical",
    "hyphae_native_search_collection",
    "hyphae_native_prove_search",
    "hyphae_native_verify_proof",
]
EXPECTED_CASES = [
    {
        "id": "capabilities-read",
        "tool": "hyphae_native_capabilities",
        "arguments": {},
        "expect": "success",
        "assert": {"pointer": "/product_api_version", "type": "integer"},
    },
    {
        "id": "security-status-read",
        "tool": "hyphae_native_security_status",
        "arguments": {},
        "expect": "success",
        "assert": {
            "pointer": "/schema",
            "equals": "hyphae-native-access-control-status-v1",
        },
    },
    {
        "id": "principal-page-read",
        "tool": "hyphae_native_security_principals",
        "arguments": {"limit": 1},
        "expect": "success",
        "assert": {
            "pointer": "/schema",
            "equals": "hyphae-native-security-principals-v1",
        },
    },
    {
        "id": "prompt-authority-rejected",
        "tool": "hyphae_native_security_status",
        "arguments": {"role": "owner"},
        "expect": "invalid_request",
        "assert": {"pointer": "/error/code", "equals": "invalid_request"},
    },
    {
        "id": "search-lexical-requires-search-authority",
        "tool": "hyphae_native_search_lexical",
        "arguments": {"index": 1, "kind": "term", "query": "rust"},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "search-collection-requires-search-authority",
        "tool": "hyphae_native_search_collection",
        "arguments": {"collection": 1, "lexical": {"query": "rust"}},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "prove-search-requires-proof-authority",
        "tool": "hyphae_native_prove_search",
        "arguments": {"collection": 1, "lexical": {"query": "rust"}},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "verify-proof-rejects-malformed-artifacts",
        "tool": "hyphae_native_verify_proof",
        "arguments": {"proof_hex": "00", "witness_hex": "00", "anchor_hex": "00"},
        "expect": "invalid_request",
        "assert": {"pointer": "/error/code", "equals": "invalid_request"},
    },
]
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
RECEIPT_KEYS = {
    "schema",
    "status",
    "host",
    "host_version",
    "host_platform",
    "source_commit",
    "source_tree",
    "source_mode",
    "adapter_sha256",
    "host_executable_sha256",
    "host_package_name",
    "host_package_version",
    "host_package_integrity",
    "host_lock_sha256",
    "host_package_lock_sha256",
    "mcp_config_sha256",
    "installed_mcp_config_sha256",
    "corpus_sha256",
    "tool_schema_version",
    "tools",
    "cases",
    "transcript_sha256",
}


class McpReceiptError(ValueError):
    """Host evidence is missing, stale, incomplete, or fabricated."""


def fail(message: str) -> None:
    raise McpReceiptError(message)


def checked_object(path: Path) -> dict[str, Any]:
    try:
        return load_object(path)
    except Exception as error:
        fail(str(error))


def require_secret_free(path: Path) -> None:
    if not path.is_file():
        fail(f"missing evidence file: {path}")
    data = path.read_bytes()
    if re.search(rb"hyp1_[A-Za-z0-9_-]{16,}", data):
        fail(f"credential-shaped material is forbidden in evidence: {path}")


def current_platform_key() -> str:
    systems = {"Darwin": "darwin", "Linux": "linux"}
    machines = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64", "AMD64": "x64"}
    try:
        return f"{systems[platform.system()]}-{machines[platform.machine()]}"
    except KeyError:
        fail("current host platform is absent from the install lock")


def validate_corpus(corpus: dict[str, Any]) -> None:
    if (
        corpus.get("schema") != "hyphae-mcp-host-corpus-v1"
        or corpus.get("mcp_config") != "plugins/hyphae/.mcp.json"
        or corpus.get("tool_schema_version") != "hyphae-native-mcp-tools-v3"
        or corpus.get("tools") != EXPECTED_TOOLS
        or corpus.get("cases") != EXPECTED_CASES
        or len({case["id"] for case in EXPECTED_CASES}) != 8
    ):
        fail("shared MCP corpus tools, cases, arguments, expectations, or assertions drifted")


def validate(
    evidence: Path,
    root: Path = ROOT,
    expected_commit: str | None = None,
    allow_integration_tree: bool = False,
) -> dict[str, Any]:
    corpus_path = root / "conformance/mcp/corpus.json"
    config_path = root / "plugins/hyphae/.mcp.json"
    corpus = checked_object(corpus_path)
    validate_corpus(corpus)
    install_lock = checked_object(root / "conformance/mcp/hosts/install-lock.json")
    platform_lane = current_platform_key()
    try:
        commit, tree, source_mode = git_identity(root, allow_integration_tree)
    except Exception as error:
        fail(str(error))
    expected_commit = expected_commit or commit
    if HEX40.fullmatch(expected_commit) is None:
        fail("expected source commit is invalid")
    expected_tools = corpus.get("tools")
    expected_cases = [case["id"] for case in corpus.get("cases", [])]
    observed_hosts: list[str] = []
    for host in HOSTS:
        receipt_path = evidence / f"{host}.receipt.json"
        transcript_path = evidence / f"{host}.transcript.json"
        require_secret_free(receipt_path)
        require_secret_free(transcript_path)
        receipt = checked_object(receipt_path)
        if set(receipt) != RECEIPT_KEYS:
            fail(f"{host} receipt has missing or unknown fields")
        if (
            receipt.get("schema") != "hyphae-mcp-host-conformance-receipt-v1"
            or receipt.get("status") != "passed"
            or receipt.get("host") != host
            or not isinstance(receipt.get("host_version"), str)
            or not receipt["host_version"]
        ):
            fail(f"{host} receipt identity or status is invalid")
        if receipt.get("host_platform") != platform_lane:
            fail(f"{host} platform differs from the exact install-lock lane")
        if (
            receipt.get("source_commit") != expected_commit
            or receipt.get("source_tree") != tree
            or receipt.get("source_mode") != source_mode
        ):
            fail(f"{host} receipt is not bound to the exact source identity")
        if receipt.get("adapter_sha256") != adapter_digest(host, root):
            fail(f"{host} receipt does not bind the in-repository adapter bytes")
        host_lock = install_lock.get("hosts", {}).get(host)
        if not isinstance(host_lock, dict):
            fail(f"{host} is absent from the install lock")
        locked_digest = host_lock.get("sha256", {}).get(platform_lane)
        if receipt.get("host_executable_sha256") != locked_digest:
            fail(f"{host} executable digest differs from the exact platform install lock")
        if receipt.get("host_version") != host_lock.get("version_output"):
            fail(f"{host} version differs from the exact install lock")
        for field in (
            "host_executable_sha256",
            "host_lock_sha256",
            "host_package_lock_sha256",
            "mcp_config_sha256",
            "installed_mcp_config_sha256",
            "corpus_sha256",
            "transcript_sha256",
        ):
            if not isinstance(receipt.get(field), str) or HEX64.fullmatch(receipt[field]) is None:
                fail(f"{host} {field} is invalid")
        expected_package = package_evidence(host, root)
        if any(receipt.get(field) != value for field, value in expected_package.items()):
            fail(f"{host} receipt package identity differs from package-lock.json")
        if receipt.get("host_lock_sha256") != digest(root / "conformance/mcp/hosts/install-lock.json"):
            fail(f"{host} receipt does not bind the host install lock")
        if receipt.get("host_package_lock_sha256") != digest(root / "conformance/mcp/hosts/package-lock.json"):
            fail(f"{host} receipt does not bind package-lock.json")
        if receipt.get("mcp_config_sha256") != digest(config_path):
            fail(f"{host} receipt does not bind the shared .mcp.json")
        if receipt.get("installed_mcp_config_sha256") != receipt.get("mcp_config_sha256"):
            fail(f"{host} installed plugin MCP config differs from the checked-in config")
        if receipt.get("corpus_sha256") != digest(corpus_path):
            fail(f"{host} receipt does not bind the shared corpus")
        if (
            receipt.get("tool_schema_version") != corpus.get("tool_schema_version")
            or receipt.get("tools") != expected_tools
            or receipt.get("cases") != expected_cases
        ):
            fail(f"{host} receipt coverage is incomplete or divergent")
        if receipt["transcript_sha256"] != digest(transcript_path):
            fail(f"{host} transcript evidence does not match its receipt")
        transcript = checked_object(transcript_path)
        if set(transcript) != TRANSCRIPT_KEYS:
            fail(f"{host} transcript has missing or unknown fields")
        try:
            validate_transcript(transcript, host, corpus)
        except Exception as error:
            fail(f"{host} transcript is invalid: {error}")
        if (
            transcript.get("host_version") != receipt["host_version"]
            or transcript.get("host_platform") != receipt["host_platform"]
            or transcript.get("host_executable_sha256") != receipt["host_executable_sha256"]
            or transcript.get("installed_mcp_config_sha256") != receipt["installed_mcp_config_sha256"]
        ):
            fail(f"{host} transcript provenance differs from its receipt")
        observed_hosts.append(host)
    return {
        "status": "passed",
        "hosts": observed_hosts,
        "tools": len(expected_tools),
        "cases": len(expected_cases),
        "source_mode": source_mode,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-commit")
    parser.add_argument("--allow-integration-tree", action="store_true")
    arguments = parser.parse_args()
    try:
        result = validate(
            arguments.evidence,
            expected_commit=arguments.expected_commit,
            allow_integration_tree=arguments.allow_integration_tree,
        )
    except McpReceiptError as error:
        print(json.dumps({"status": "failed", "error": str(error)}, sort_keys=True))
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
