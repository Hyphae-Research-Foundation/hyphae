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
        return directory

    def test_checked_in_plugins_share_one_server(self) -> None:
        self.assertEqual(
            validate(),
            {"status": "passed", "hosts": ["claude-code", "codex"], "mcp_servers": 1},
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


if __name__ == "__main__":
    unittest.main()
