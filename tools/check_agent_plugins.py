#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed structural checker for the Claude Code and Codex plugins."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
CANONICAL_MCP_ARGS = ["mcp", "--base-url", "http://127.0.0.1:8787"]
API_KEY = re.compile(r"hyp1_[0-9a-f]{32}_[0-9a-f]{64}")


class AgentPluginValidationError(ValueError):
    """A checked-in agent plugin is incomplete, unsafe, or divergent."""


def fail(message: str) -> None:
    raise AgentPluginValidationError(message)


def load_object(path: Path, root: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"{path.relative_to(root)} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{path.relative_to(root)} must contain one JSON object")
    return value


def validate_mcp(value: dict[str, Any]) -> None:
    if set(value) != {"mcpServers"}:
        fail("shared MCP config must contain only mcpServers")
    servers = value["mcpServers"]
    if not isinstance(servers, dict) or set(servers) != {"hyphae"}:
        fail("shared MCP config must define exactly one hyphae server")
    server = servers["hyphae"]
    expected = {
        "type": "stdio",
        "command": "hyphae",
        "args": CANONICAL_MCP_ARGS,
    }
    if server != expected:
        fail("Claude Code and Codex must use the canonical hyphae stdio server")


def validate_codex(value: dict[str, Any]) -> str:
    if value.get("name") != "hyphae" or value.get("mcpServers") != "./.mcp.json":
        fail("Codex plugin identity or MCP binding is invalid")
    version = value.get("version")
    if not isinstance(version, str) or re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("Codex plugin version must be strict semver")
    if value.get("license") != "AGPL-3.0-only":
        fail("Codex plugin license must match the repository")
    interface = value.get("interface")
    if not isinstance(interface, dict) or interface.get("developerName") != "Celiums Solutions LLC":
        fail("Codex plugin interface metadata is incomplete")
    prompts = interface.get("defaultPrompt")
    if (
        not isinstance(prompts, list)
        or not 1 <= len(prompts) <= 3
        or any(not isinstance(prompt, str) or not prompt or len(prompt) > 128 for prompt in prompts)
    ):
        fail("Codex starter prompts must be one to three bounded strings")
    return version


def validate_claude(value: dict[str, Any], version: str) -> None:
    if value.get("name") != "hyphae" or value.get("version") != version:
        fail("Claude Code plugin identity must match the Codex bundle")
    if value.get("license") != "AGPL-3.0-only":
        fail("Claude Code plugin license must match the repository")


def validate_marketplaces(root: Path, version: str) -> None:
    claude = load_object(root / ".claude-plugin/marketplace.json", root)
    plugins = claude.get("plugins")
    if (
        claude.get("name") != "hyphae"
        or not isinstance(plugins, list)
        or len(plugins) != 1
        or plugins[0].get("name") != "hyphae"
        or plugins[0].get("source") != "./plugins/hyphae"
        or plugins[0].get("version") != version
    ):
        fail("Claude Code marketplace is not bound to the checked-in plugin")

    codex = load_object(root / ".agents/plugins/marketplace.json", root)
    entries = codex.get("plugins")
    if (
        not isinstance(entries, list)
        or len(entries) != 1
        or entries[0].get("name") != "hyphae"
        or entries[0].get("source")
        != {"source": "local", "path": "./plugins/hyphae"}
        or entries[0].get("policy")
        != {"installation": "AVAILABLE", "authentication": "ON_INSTALL"}
    ):
        fail("Codex marketplace is not bound to the checked-in plugin")


def validate_skill(plugin: Path) -> None:
    skill = plugin / "skills/use-hyphae/SKILL.md"
    text = skill.read_text(encoding="utf-8")
    if not text.startswith("---\n") or "name: use-hyphae" not in text:
        fail("shared Hyphae skill metadata is invalid")
    required = {"hyphae_capabilities", "hyphae_query", "hyphae_retrieve_hybrid"}
    if not required.issubset(set(re.findall(r"hyphae_[a-z_]+", text))):
        fail("shared Hyphae skill omits required bounded workflows")


def validate(root: Path = ROOT) -> dict[str, Any]:
    plugin = root / "plugins/hyphae"
    files = [
        plugin / ".mcp.json",
        plugin / ".codex-plugin/plugin.json",
        plugin / ".claude-plugin/plugin.json",
        plugin / "skills/use-hyphae/SKILL.md",
        root / ".claude-plugin/marketplace.json",
        root / ".agents/plugins/marketplace.json",
    ]
    for path in files:
        if not path.is_file():
            fail(f"required plugin file is missing: {path.relative_to(root)}")
        if API_KEY.search(path.read_text(encoding="utf-8")) is not None:
            fail(f"credential material is forbidden in {path.relative_to(root)}")
    validate_mcp(load_object(plugin / ".mcp.json", root))
    version = validate_codex(load_object(plugin / ".codex-plugin/plugin.json", root))
    validate_claude(load_object(plugin / ".claude-plugin/plugin.json", root), version)
    validate_marketplaces(root, version)
    validate_skill(plugin)
    return {"status": "passed", "hosts": ["claude-code", "codex"], "mcp_servers": 1}


def main() -> int:
    print(json.dumps(validate(), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
