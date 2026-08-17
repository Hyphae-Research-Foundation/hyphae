#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive the pinned Codex app-server MCP control plane without a model turn."""

from __future__ import annotations

import os
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402
    AdapterFailure,
    JsonlControlPlane,
    ROOT,
    credential_canary,
    load_object,
    run_json,
    safe_environment,
    sha256,
    structured_case,
    verify_host,
    write_transcript,
)


def discovered_tools(status: dict[str, Any]) -> list[str]:
    data = status.get("data")
    if not isinstance(data, list):
        raise AdapterFailure("Codex MCP status omitted server inventory")
    servers = [entry for entry in data if isinstance(entry, dict) and entry.get("name") == "hyphae"]
    if len(servers) != 1 or not isinstance(servers[0].get("tools"), dict):
        raise AdapterFailure("Codex did not load exactly one Hyphae MCP server")
    names = list(servers[0]["tools"])
    if len(names) != len(set(names)):
        raise AdapterFailure("Codex MCP status repeated a tool")
    return sorted(names)


EXPECTED_TOOLS = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
]


def validate_plugin_list(value: dict[str, Any]) -> str:
    marketplaces = value.get("marketplaces")
    if not isinstance(marketplaces, list):
        raise AdapterFailure("Codex plugin/list omitted marketplaces")
    personal = [item for item in marketplaces if isinstance(item, dict) and item.get("name") == "personal"]
    if len(personal) != 1 or not isinstance(personal[0].get("plugins"), list):
        raise AdapterFailure("Codex did not discover the local plugin marketplace")
    plugins = [item for item in personal[0]["plugins"] if item.get("name") == "hyphae"]
    if len(plugins) != 1 or plugins[0].get("installed") is not True or plugins[0].get("enabled") is not True:
        raise AdapterFailure("Codex did not discover the installed Hyphae plugin")
    path = personal[0].get("path")
    if not isinstance(path, str):
        raise AdapterFailure("Codex marketplace path evidence is missing")
    return path


def run() -> None:
    provenance = verify_host("codex", os.environ["HYPHAE_MCP_HOST_EXECUTABLE"])
    corpus = load_object(Path(os.environ["HYPHAE_MCP_CORPUS"]))
    canary = credential_canary()
    plugin_config = Path(os.environ["HYPHAE_MCP_CONFIG"])
    with tempfile.TemporaryDirectory(
        prefix="hyphae-codex-mcp-", ignore_cleanup_errors=True
    ) as directory:
        codex_home = Path(directory) / "codex-home"
        codex_home.mkdir()
        environment = safe_environment(
            CODEX_HOME=str(codex_home),
            CODEX_ANALYTICS_ENABLED="false",
        )
        if canary is not None:
            environment["HYPHAE_NATIVE_API_KEY_FILE"] = os.environ[
                "HYPHAE_NATIVE_API_KEY_FILE"
            ]
        executable = provenance["executable"]
        run_json(
            [executable, "plugin", "marketplace", "add", str(ROOT), "--json"],
            environment,
            canary,
        )
        installed = run_json(
            [executable, "plugin", "add", "hyphae@personal", "--json"],
            environment,
            canary,
        )
        installed_path = installed.get("installedPath")
        if not isinstance(installed_path, str):
            raise AdapterFailure("Codex plugin installation path evidence is missing")
        installed_config = Path(installed_path) / ".mcp.json"
        installed_digest = sha256(installed_config)
        if installed_digest != sha256(plugin_config):
            raise AdapterFailure("Codex installed MCP config differs from the shared plugin config")
        provenance["installed_mcp_config_sha256"] = installed_digest
        config_path = codex_home / "config.toml"
        if config_path.is_file() and "[mcp_servers.hyphae]" in config_path.read_text(encoding="utf-8"):
            raise AdapterFailure("Codex plugin conformance forbids a duplicate manual MCP server")
        plane = JsonlControlPlane(
            [executable, "app-server", "--stdio", "--disable", "responses_websockets", "--disable", "responses_websockets_v2"],
            environment,
            canary,
        )
        try:
            initialized = plane.request(
                "initialize-1",
                "initialize",
                {
                    "clientInfo": {"name": "hyphae-mcp-conformance", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                },
            )
            observed_home = initialized.get("codexHome")
            if not isinstance(observed_home, str) or Path(observed_home).resolve() != codex_home.resolve():
                raise AdapterFailure("Codex app-server did not use the isolated CODEX_HOME")
            listed = plane.request(
                "plugin-list-1",
                "plugin/list",
                {"cwds": [str(ROOT)], "forceRefetch": False, "marketplaceKinds": ["local"]},
            )
            marketplace_path = validate_plugin_list(listed)
            plugin = plane.request(
                "plugin-read-1",
                "plugin/read",
                {"pluginName": "hyphae", "marketplacePath": marketplace_path},
            ).get("plugin")
            if not isinstance(plugin, dict) or plugin.get("mcpServers") != ["hyphae"]:
                raise AdapterFailure("Codex plugin/read did not expose the Hyphae MCP server")
            thread = plane.request(
                "thread-start-1",
                "thread/start",
                {
                    "cwd": str(ROOT),
                    "ephemeral": True,
                    "modelProvider": "ollama",
                },
            ).get("thread")
            if not isinstance(thread, dict) or thread.get("ephemeral") is not True:
                raise AdapterFailure("Codex did not create an ephemeral control-plane thread")
            thread_id = thread.get("id")
            if not isinstance(thread_id, str):
                raise AdapterFailure("Codex thread identity is missing")

            deadline = time.monotonic() + 30
            status: dict[str, Any] = {}
            tools: list[str] = []
            attempt = 0
            while time.monotonic() < deadline and attempt < 100:
                attempt += 1
                status = plane.request(
                    f"mcp-status-{attempt}",
                    "mcpServerStatus/list",
                    {"threadId": thread_id, "detail": "toolsAndAuthOnly", "limit": 10},
                )
                tools = discovered_tools(status)
                if tools:
                    break
            if tools != sorted(corpus["tools"]):
                startup = [
                    frame
                    for frame in plane.notifications
                    if frame.get("method") == "mcpServer/startupStatus/updated"
                ]
                raise AdapterFailure(
                    "Codex tool discovery differs from the shared corpus: "
                    f"{status!r}; startup={startup[-3:]!r}"
                )

            cases = []
            for index, case in enumerate(corpus["cases"], start=1):
                result = plane.request(
                    f"mcp-call-{index}",
                    "mcpServer/tool/call",
                    {
                        "threadId": thread_id,
                        "server": "hyphae",
                        "tool": case["tool"],
                        "arguments": case["arguments"],
                    },
                )
                cases.append(structured_case(result, case))
            if any(frame.get("method") == "turn/start" for frame in plane.frames):
                raise AdapterFailure("Codex adapter attempted a model turn")
        finally:
            plane.close()
    write_transcript("codex", provenance, corpus["tools"], cases)


if __name__ == "__main__":
    try:
        run()
    except (AdapterFailure, KeyError, OSError, ValueError) as error:
        print(f"codex MCP adapter failed: {error}", file=sys.stderr)
        raise SystemExit(2) from error
