#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Mutation tests for the shared Claude Code and Codex plugin bundle."""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from tools.check_agent_plugins import AgentPluginValidationError, ROOT, validate


class AgentPluginContractTests(unittest.TestCase):
    def fixture(self) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        for relative in ("plugins/hyphae", ".claude-plugin", ".agents/plugins"):
            source = ROOT / relative
            target = root / relative
            if source.is_dir():
                shutil.copytree(source, target)
            else:
                target.mkdir(parents=True, exist_ok=True)
        (root / "contracts").mkdir(parents=True)
        shutil.copy2(
            ROOT / "contracts/native-mcp-v2.json",
            root / "contracts/native-mcp-v2.json",
        )
        shutil.copytree(
            ROOT / "conformance/mcp",
            root / "conformance/mcp",
            ignore=shutil.ignore_patterns("node_modules"),
        )
        return directory

    def test_checked_in_plugins_share_one_server(self) -> None:
        self.assertEqual(
            validate(),
            {
                "status": "passed",
                "hosts": ["claude-code", "codex"],
                "mcp_servers": 1,
                "tools": 3,
            },
        )

    def test_host_cannot_fork_the_mcp_command(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "plugins/hyphae/.mcp.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["mcpServers"]["hyphae"]["command"] = "other-hyphae"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "canonical hyphae stdio"):
                validate(root)

    def test_mcp_environment_allowlist_is_exact_and_never_contains_secrets(self) -> None:
        mutations = (["HYPHAE_NATIVE_API_KEY_FILE", "HOME"], ["hyp1_" + "a" * 32 + "_" + "b" * 64])
        for env_vars in mutations:
            with self.subTest(env_vars=env_vars), self.fixture() as directory:
                root = Path(directory)
                path = root / "plugins/hyphae/.mcp.json"
                value = json.loads(path.read_text(encoding="utf-8"))
                value["mcpServers"]["hyphae"]["env_vars"] = env_vars
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaises(AgentPluginValidationError):
                    validate(root)

    def test_marketplace_version_cannot_drift(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".claude-plugin/marketplace.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["plugins"][0]["version"] = "9.9.9"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "Claude Code marketplace"):
                validate(root)

    def test_plugin_version_is_the_bounded_slice_version(self) -> None:
        self.assertEqual(
            json.loads(
                (ROOT / "plugins/hyphae/.codex-plugin/plugin.json").read_text(encoding="utf-8")
            )["version"],
            "1.2.2",
        )

    def test_plugin_version_cannot_remain_on_the_legacy_bundle(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            for relative in (
                "plugins/hyphae/.codex-plugin/plugin.json",
                "plugins/hyphae/.claude-plugin/plugin.json",
                ".claude-plugin/marketplace.json",
            ):
                path = root / relative
                path.write_text(path.read_text(encoding="utf-8").replace("1.2.2", "0.2.0"), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "bounded 1.2"):
                validate(root)

    def test_credential_material_is_rejected_everywhere(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "plugins/hyphae/skills/use-hyphae/SKILL.md"
            path.write_text(
                path.read_text(encoding="utf-8")
                + "\nhyp1_0123456789abcdef0123456789abcdef_"
                + "0" * 64,
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentPluginValidationError, "credential material"):
                validate(root)

    def test_skill_cannot_reintroduce_legacy_or_write_tools(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "plugins/hyphae/skills/use-hyphae/SKILL.md"
            path.write_text(
                path.read_text(encoding="utf-8") + "\nUse hyphae_put for this write.\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentPluginValidationError, "diverges"):
                validate(root)

    def test_plugin_readme_cannot_restore_the_legacy_bearer(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "plugins/hyphae/README.md"
            path.write_text(
                path.read_text(encoding="utf-8")
                + "\nSet HYPHAE_BEARER_TOKEN_FILE for compatibility.\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentPluginValidationError, "managed Native v2"):
                validate(root)

    def test_tool_page_size_is_exact(self) -> None:
        for page_size in (1, 3, 100.0, "100"):
            with self.subTest(page_size=page_size), self.fixture() as directory:
                root = Path(directory)
                path = root / "contracts/native-mcp-v2.json"
                value = json.loads(path.read_text(encoding="utf-8"))
                value["tool_page_size"] = page_size
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(AgentPluginValidationError, "page size"):
                    validate(root)

    def test_tool_hints_are_exact(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "contracts/native-mcp-v2.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["tools"][0]["annotations"]["idempotentHint"] = False
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "annotations"):
                validate(root)

    def test_resource_and_cancellation_limits_are_exact(self) -> None:
        mutations = (
            ("resource_limits", "active_tool_calls", 2, "resource limits"),
            ("resource_limits", "pending_responses", 0, "resource limits"),
            ("resource_limits", "input_bytes", 4194305, "resource limits"),
            ("cancellation", "idempotent", False, "cancellation"),
        )
        for section, field, replacement, error in mutations:
            with self.subTest(field=field), self.fixture() as directory:
                root = Path(directory)
                path = root / "contracts/native-mcp-v2.json"
                value = json.loads(path.read_text(encoding="utf-8"))
                value[section][field] = replacement
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(AgentPluginValidationError, error):
                    validate(root)

    def test_shared_host_corpus_cannot_add_a_tool(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "conformance/mcp/corpus.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["tools"].append("hyphae_native_write")
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "conformance corpus"):
                validate(root)

    def test_shared_host_corpus_exact_cases_are_frozen_and_unique(self) -> None:
        mutations = (
            lambda cases: cases[0].__setitem__("id", cases[1]["id"]),
            lambda cases: cases[0].__setitem__("tool", "hyphae_native_security_status"),
            lambda cases: cases[2].__setitem__("arguments", {"limit": 2}),
            lambda cases: cases[3].__setitem__("expect", "success"),
            lambda cases: cases[1].__setitem__("assert", {"pointer": "/schema", "equals": "other"}),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate), self.fixture() as directory:
                root = Path(directory)
                path = root / "conformance/mcp/corpus.json"
                value = json.loads(path.read_text(encoding="utf-8"))
                mutate(value["cases"])
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(AgentPluginValidationError, "conformance corpus"):
                    validate(root)

    def test_success_schema_cannot_expose_credential_material(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "contracts/native-mcp-v2.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            output = value["tools"][1]["outputSchema"]
            success = output.get("oneOf", [output])[0]
            success["properties"]["api_key"] = {"type": "string"}
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "output schema"):
                validate(root)

    def test_typed_error_schema_is_mandatory(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "contracts/native-mcp-v2.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            output = value["tools"][0]["outputSchema"]
            if "oneOf" in output:
                output["oneOf"].pop()
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "output schema"):
                validate(root)

    def test_output_schema_root_object_is_mandatory(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / "contracts/native-mcp-v2.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["tools"][0]["outputSchema"].pop("type")
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "output schema"):
                validate(root)

    def test_plugin_recommends_auditor_instead_of_reader(self) -> None:
        for relative in (
            "plugins/hyphae/README.md",
            "plugins/hyphae/skills/use-hyphae/SKILL.md",
            "plugins/hyphae/.codex-plugin/plugin.json",
            "plugins/hyphae/.claude-plugin/plugin.json",
            ".claude-plugin/marketplace.json",
        ):
            with self.subTest(relative=relative), self.fixture() as directory:
                root = Path(directory)
                path = root / relative
                path.write_text(
                    path.read_text(encoding="utf-8").replace("Auditor", "Reader"),
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(AgentPluginValidationError, "Auditor"):
                    validate(root)

    def test_plugin_cannot_recommend_reader_alongside_auditor(self) -> None:
        for relative, field_path in (
            ("plugins/hyphae/README.md", None),
            ("plugins/hyphae/skills/use-hyphae/SKILL.md", None),
            ("plugins/hyphae/.codex-plugin/plugin.json", ("interface", "longDescription")),
            ("plugins/hyphae/.claude-plugin/plugin.json", ("description",)),
            (".claude-plugin/marketplace.json", ("plugins", 0, "description")),
        ):
            with self.subTest(relative=relative), self.fixture() as directory:
                root = Path(directory)
                path = root / relative
                if field_path is None:
                    path.write_text(
                        path.read_text(encoding="utf-8") + "\nUse a Reader API key.\n",
                        encoding="utf-8",
                    )
                else:
                    value = json.loads(path.read_text(encoding="utf-8"))
                    target = value
                    for field in field_path[:-1]:
                        target = target[field]
                    target[field_path[-1]] += " Reader"
                    path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(AgentPluginValidationError, "Reader"):
                    validate(root)


if __name__ == "__main__":
    unittest.main()
