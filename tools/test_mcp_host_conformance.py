#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Unit tests for real-host adapters, runner provenance, and receipt mutations."""

from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from conformance.mcp.adapters.claude_code import discovered_tools as claude_tools
from conformance.mcp.adapters.codex import discovered_tools as codex_tools
from conformance.mcp.adapters.common import AdapterFailure, JsonlControlPlane, structured_case
from tools.check_mcp_host_receipts import McpReceiptError, validate, validate_corpus
from tools.run_mcp_host_conformance import (
    MissingEvidence,
    ROOT,
    adapter_digest,
    digest,
    git_identity,
    package_evidence,
    run,
)


class McpHostConformanceTests(unittest.TestCase):
    def test_runner_uses_only_internal_adapter_and_repo_install(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            with self.assertRaisesRegex(MissingEvidence, "adapter|repository-installed"):
                run("codex", target / "receipt.json", target / "transcript.json", root=target)

    def test_codex_status_parser_requires_exact_server_shape(self) -> None:
        self.assertEqual(
            codex_tools(
                {
                    "data": [
                        {
                            "name": "hyphae",
                            "tools": {
                                "hyphae_native_capabilities": {"name": "hyphae_native_capabilities"},
                                "hyphae_native_security_status": {"name": "hyphae_native_security_status"},
                            },
                        }
                    ]
                }
            ),
            ["hyphae_native_capabilities", "hyphae_native_security_status"],
        )
        with self.assertRaises(RuntimeError):
            codex_tools({"data": [{"name": "other", "tools": {}}]})

    def test_claude_status_parser_requires_connected_namespaced_plugin(self) -> None:
        self.assertEqual(
            claude_tools(
                {
                    "mcpServers": [
                        {
                            "name": "plugin:hyphae:hyphae",
                            "status": "connected",
                            "tools": [{"name": "hyphae_native_capabilities"}],
                        }
                    ]
                }
            ),
            ["hyphae_native_capabilities"],
        )
        self.assertEqual(
            claude_tools(
                {
                    "mcpServers": [
                        {
                            "name": "plugin:hyphae:hyphae",
                            "status": "connecting",
                        }
                    ]
                }
            ),
            [],
        )
        with self.assertRaises(RuntimeError):
            claude_tools(
                {
                    "mcpServers": [
                        {
                            "name": "plugin:hyphae:hyphae",
                            "status": "connected",
                        }
                    ]
                }
            )
        self.assertEqual(
            codex_tools(
                {
                    "data": [
                        {
                            "name": "hyphae",
                            "tools": {
                                "hyphae_native_capabilities": {},
                                "unexpected_tool": {},
                            },
                        }
                    ]
                }
            ),
            ["hyphae_native_capabilities", "unexpected_tool"],
        )

    def test_structured_content_is_mandatory(self) -> None:
        case = {
            "id": "capabilities-read",
            "tool": "hyphae_native_capabilities",
            "arguments": {},
            "expect": "success",
        }
        with self.assertRaisesRegex(AdapterFailure, "structuredContent"):
            structured_case({"content": []}, case)
        observed = structured_case(
            {"structuredContent": {"product_api_version": 1}, "isError": False}, case
        )
        self.assertEqual(observed["result"], {"product_api_version": 1})

    def test_jsonl_control_plane_parses_codex_and_claude_response_envelopes(self) -> None:
        plane = object.__new__(JsonlControlPlane)
        plane.responses = __import__("queue").Queue()
        plane.notifications = []
        plane.frames = []
        plane.responses.put({"method": "progress", "params": {}})
        plane.responses.put({"id": "codex-1", "result": {"data": []}})
        self.assertEqual(plane.wait("codex-1"), {"data": []})
        plane.responses.put(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "claude-1",
                    "response": {"mcpServers": []},
                },
            }
        )
        self.assertEqual(plane.wait("claude-1"), {"mcpServers": []})

    def test_checker_requires_both_host_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(
                McpReceiptError, "required evidence input|invalid evidence|missing evidence file"
            ):
                validate(Path(directory), allow_integration_tree=True)

    def test_corpus_shared_config_and_exact_host_locks_are_present(self) -> None:
        corpus = json.loads((ROOT / "conformance/mcp/corpus.json").read_text(encoding="utf-8"))
        config = json.loads((ROOT / corpus["mcp_config"]).read_text(encoding="utf-8"))
        package = json.loads(
            (ROOT / "conformance/mcp/hosts/package.json").read_text(encoding="utf-8")
        )
        lock = json.loads(
            (ROOT / "conformance/mcp/hosts/package-lock.json").read_text(encoding="utf-8")
        )
        self.assertEqual(len(corpus["tools"]), 7)
        self.assertEqual(
            [(case["id"], case["tool"], case["arguments"], case["expect"], case["assert"]) for case in corpus["cases"]],
            [
                ("capabilities-read", "hyphae_native_capabilities", {}, "success", {"pointer": "/product_api_version", "type": "integer"}),
                ("security-status-read", "hyphae_native_security_status", {}, "success", {"pointer": "/schema", "equals": "hyphae-native-access-control-status-v1"}),
                ("principal-page-read", "hyphae_native_security_principals", {"limit": 1}, "success", {"pointer": "/schema", "equals": "hyphae-native-security-principals-v1"}),
                ("prompt-authority-rejected", "hyphae_native_security_status", {"role": "owner"}, "invalid_request", {"pointer": "/error/code", "equals": "invalid_request"}),
                ("search-lexical-requires-search-authority", "hyphae_native_search_lexical", {"index": 1, "kind": "term", "query": "rust"}, "authorization_denied", {"pointer": "/error/code", "equals": "authorization_denied"}),
                ("search-collection-requires-search-authority", "hyphae_native_search_collection", {"collection": 1, "lexical": {"query": "rust"}}, "authorization_denied", {"pointer": "/error/code", "equals": "authorization_denied"}),
                ("prove-search-requires-proof-authority", "hyphae_native_prove_search", {"collection": 1, "lexical": {"query": "rust"}}, "authorization_denied", {"pointer": "/error/code", "equals": "authorization_denied"}),
                ("verify-proof-rejects-malformed-artifacts", "hyphae_native_verify_proof", {"proof_hex": "00", "witness_hex": "00", "anchor_hex": "00"}, "invalid_request", {"pointer": "/error/code", "equals": "invalid_request"}),
            ],
        )
        self.assertEqual(len({case["id"] for case in corpus["cases"]}), 8)
        validate_corpus(corpus)
        drifted = json.loads(json.dumps(corpus))
        drifted["cases"][2]["arguments"] = {"limit": 2}
        with self.assertRaisesRegex(McpReceiptError, "corpus"):
            validate_corpus(drifted)
        self.assertEqual(set(config["mcpServers"]), {"hyphae"})
        self.assertEqual(
            package["devDependencies"],
            {"@anthropic-ai/claude-code": "2.1.233", "@openai/codex": "0.147.0"},
        )
        self.assertEqual(lock["packages"]["node_modules/@openai/codex"]["version"], "0.147.0")
        self.assertEqual(
            lock["packages"]["node_modules/@anthropic-ai/claude-code"]["version"], "2.1.233"
        )

    def complete_evidence(self, directory: Path) -> None:
        corpus_path = ROOT / "conformance/mcp/corpus.json"
        config_path = ROOT / "plugins/hyphae/.mcp.json"
        corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
        commit, tree, source_mode = git_identity(ROOT, True)
        for host in ("claude-code", "codex"):
            install_lock = json.loads(
                (ROOT / "conformance/mcp/hosts/install-lock.json").read_text(encoding="utf-8")
            )
            from tools.check_mcp_host_receipts import current_platform_key

            host_entry = install_lock["hosts"][host]
            host_digest = host_entry["sha256"][current_platform_key()]
            transcript = {
                "schema": "hyphae-mcp-host-transcript-v1",
                "host": host,
                "host_version": host_entry["version_output"],
                "host_platform": current_platform_key(),
                "host_executable_sha256": host_digest,
                "installed_mcp_config_sha256": digest(config_path),
                "tools": corpus["tools"],
                "cases": [
                    {
                        "id": case["id"],
                        "tool": case["tool"],
                        "arguments": case["arguments"],
                        "outcome": case["expect"],
                        "result": (
                            {"product_api_version": 1}
                            if case["id"] == "capabilities-read"
                            else {"schema": "hyphae-native-access-control-status-v1"}
                            if case["id"] == "security-status-read"
                            else {"schema": "hyphae-native-security-principals-v1"}
                            if case["id"] == "principal-page-read"
                            else {"error": {"code": "authorization_denied"}}
                            if case["expect"] == "authorization_denied"
                            else {"error": {"code": "invalid_request"}}
                        ),
                    }
                    for case in corpus["cases"]
                ],
            }
            transcript_path = directory / f"{host}.transcript.json"
            transcript_path.write_text(json.dumps(transcript), encoding="utf-8")
            receipt = {
                "schema": "hyphae-mcp-host-conformance-receipt-v1",
                "status": "passed",
                "host": host,
                "host_version": host_entry["version_output"],
                "host_platform": current_platform_key(),
                "source_commit": commit,
                "source_tree": tree,
                "source_mode": source_mode,
                "adapter_sha256": adapter_digest(host),
                "host_executable_sha256": host_digest,
                **package_evidence(host),
                "host_lock_sha256": digest(ROOT / "conformance/mcp/hosts/install-lock.json"),
                "host_package_lock_sha256": digest(
                    ROOT / "conformance/mcp/hosts/package-lock.json"
                ),
                "mcp_config_sha256": digest(config_path),
                "installed_mcp_config_sha256": digest(config_path),
                "corpus_sha256": digest(corpus_path),
                "tool_schema_version": corpus["tool_schema_version"],
                "tools": corpus["tools"],
                "cases": [case["id"] for case in corpus["cases"]],
                "transcript_sha256": hashlib.sha256(transcript_path.read_bytes()).hexdigest(),
            }
            (directory / f"{host}.receipt.json").write_text(
                json.dumps(receipt), encoding="utf-8"
            )

    def test_checker_accepts_complete_integration_tree_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            self.complete_evidence(evidence)
            source_mode = git_identity(ROOT, True)[2]
            self.assertEqual(
                validate(evidence, allow_integration_tree=True),
                {
                    "status": "passed",
                    "hosts": ["claude-code", "codex"],
                    "tools": 7,
                    "cases": 8,
                    "source_mode": source_mode,
                },
            )

    def test_checker_rejects_provenance_and_transcript_mutations(self) -> None:
        fields = (
            "adapter_sha256",
            "host_executable_sha256",
            "host_package_integrity",
            "host_lock_sha256",
            "installed_mcp_config_sha256",
            "source_tree",
        )
        for field in fields:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                evidence = Path(directory)
                self.complete_evidence(evidence)
                path = evidence / "codex.receipt.json"
                receipt = json.loads(path.read_text(encoding="utf-8"))
                receipt[field] = "0" * 64 if field != "host_package_integrity" else "sha512-fake"
                path.write_text(json.dumps(receipt), encoding="utf-8")
                with self.assertRaises(McpReceiptError):
                    validate(evidence, allow_integration_tree=True)

        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            self.complete_evidence(evidence)
            transcript_path = evidence / "codex.transcript.json"
            transcript = json.loads(transcript_path.read_text(encoding="utf-8"))
            transcript["cases"][0].pop("result")
            transcript_path.write_text(json.dumps(transcript), encoding="utf-8")
            with self.assertRaises(McpReceiptError):
                validate(evidence, allow_integration_tree=True)

        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            self.complete_evidence(evidence)
            transcript_path = evidence / "codex.transcript.json"
            transcript_path.write_text(
                json.dumps({"leaked": "hyp1_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(McpReceiptError, "credential-shaped"):
                validate(evidence, allow_integration_tree=True)

    def test_checker_rejects_fake_exact_host_identity(self) -> None:
        for field in ("host_version", "host_executable_sha256"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                evidence = Path(directory)
                self.complete_evidence(evidence)
                receipt_path = evidence / "codex.receipt.json"
                transcript_path = evidence / "codex.transcript.json"
                receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
                transcript = json.loads(transcript_path.read_text(encoding="utf-8"))
                fake = "fake-version" if field == "host_version" else "0" * 64
                receipt[field] = fake
                transcript[field] = fake
                transcript_path.write_text(json.dumps(transcript), encoding="utf-8")
                receipt["transcript_sha256"] = hashlib.sha256(
                    transcript_path.read_bytes()
                ).hexdigest()
                receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
                with self.assertRaisesRegex(McpReceiptError, "install lock"):
                    validate(evidence, allow_integration_tree=True)

    def test_dirty_source_requires_explicit_integration_mode(self) -> None:
        with mock.patch(
            "tools.run_mcp_host_conformance.git", side_effect=["f" * 40, " M file"]
        ):
            with self.assertRaisesRegex(MissingEvidence, "not clean"):
                git_identity(ROOT, False)


if __name__ == "__main__":
    unittest.main()
