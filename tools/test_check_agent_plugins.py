#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
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

    def test_marketplace_version_cannot_drift(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".claude-plugin/marketplace.json"
            value = json.loads(path.read_text(encoding="utf-8"))
            value["plugins"][0]["version"] = "9.9.9"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(AgentPluginValidationError, "Claude Code marketplace"):
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
        for page_size in (1, 3, 2.0, "2"):
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
