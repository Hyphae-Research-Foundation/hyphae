#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive Claude Code's stream-JSON MCP control plane without a model call."""

from __future__ import annotations

import os
import re
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
    safe_environment,
    sha256,
    structured_case,
    verify_host,
    write_transcript,
)


SERVER_NAME = "plugin:hyphae:hyphae"


def discovered_tools(status: dict[str, Any]) -> list[str]:
    servers = status.get("mcpServers")
    if not isinstance(servers, list):
        raise AdapterFailure("Claude Code mcp_status omitted server inventory")
    hyphae = [server for server in servers if isinstance(server, dict) and server.get("name") == SERVER_NAME]
    if len(hyphae) != 1 or hyphae[0].get("status") != "connected":
        return []
    tools = hyphae[0].get("tools")
    if not isinstance(tools, list):
        raise AdapterFailure("Claude Code connected status omitted tools")
    names = []
    for tool in tools:
        name = tool.get("name") if isinstance(tool, dict) else None
        if not isinstance(name, str):
            raise AdapterFailure("Claude Code emitted invalid tool discovery evidence")
        names.append(name)
    if len(names) != len(set(names)):
        raise AdapterFailure("Claude Code repeated a discovered tool")
    return sorted(names)


EXPECTED_TOOLS = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
]


def namespaced_tool(name: str) -> str:
    namespace = re.sub(r"[^A-Za-z0-9_-]", "_", SERVER_NAME)
    return f"mcp__{namespace}__{name}"


def run() -> None:
    provenance = verify_host("claude-code", os.environ["HYPHAE_MCP_HOST_EXECUTABLE"])
    corpus = load_object(Path(os.environ["HYPHAE_MCP_CORPUS"]))
    canary = credential_canary()
    plugin = Path(os.environ["HYPHAE_MCP_CONFIG"]).parent
    installed_digest = sha256(plugin / ".mcp.json")
    provenance["installed_mcp_config_sha256"] = installed_digest
    with tempfile.TemporaryDirectory(
        prefix="hyphae-claude-mcp-", ignore_cleanup_errors=True
    ) as directory:
        home = Path(directory)
        environment = safe_environment(
            HOME=str(home),
            CLAUDE_CONFIG_DIR=str(home / ".claude"),
            CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
            CLAUDE_CODE_ENABLE_TELEMETRY="0",
            ENABLE_TOOL_SEARCH="0",
        )
        if canary is not None:
            environment["HYPHAE_NATIVE_API_KEY_FILE"] = os.environ[
                "HYPHAE_NATIVE_API_KEY_FILE"
            ]
        plane = JsonlControlPlane(
            [
                provenance["executable"],
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--plugin-dir",
                str(plugin),
                "--no-session-persistence",
                "--verbose",
            ],
            environment,
            canary,
        )
        cases: list[dict[str, Any]] = []
        try:
            initialized = plane.control("initialize-1", "initialize")
            commands = initialized.get("commands")
            if not isinstance(commands, list) or not any(
                command.get("name") == "hyphae:use-hyphae"
                for command in commands
                if isinstance(command, dict)
            ):
                raise AdapterFailure("Claude Code did not load the real Hyphae plugin")
            deadline = time.monotonic() + 30
            status: dict[str, Any] = {}
            tools: list[str] = []
            attempt = 0
            launch_requested = False
            while time.monotonic() < deadline and attempt < 100:
                attempt += 1
                status = plane.control(f"mcp-status-{attempt}", "mcp_status")
                tools = discovered_tools(status)
                if tools:
                    break
                if not launch_requested:
                    plane.control(
                        "mcp-launch-1",
                        "mcp_call",
                        tool=namespaced_tool("hyphae_native_capabilities"),
                        arguments={},
                    )
                    launch_requested = True
            if tools != sorted(corpus["tools"]):
                raise AdapterFailure(
                    f"Claude Code tool discovery differs from the shared corpus: {status!r}"
                )
            for index, case in enumerate(corpus["cases"], start=1):
                result = plane.control(
                    f"mcp-call-{index}",
                    "mcp_call",
                    tool=namespaced_tool(case["tool"]),
                    arguments=case["arguments"],
                )
                cases.append(structured_case(result, case))
            forbidden = {"assistant", "user", "result"}
            if any(frame.get("type") in forbidden for frame in plane.frames):
                raise AdapterFailure("Claude Code adapter entered a model conversation")
            plane.send(
                {
                    "type": "control_request",
                    "request_id": "end-session-1",
                    "request": {"subtype": "end_session", "reason": "conformance_complete"},
                }
            )
        finally:
            plane.close()
    write_transcript("claude-code", provenance, corpus["tools"], cases)


if __name__ == "__main__":
    try:
        run()
    except (AdapterFailure, KeyError, OSError, ValueError) as error:
        print(f"Claude Code MCP adapter failed: {error}", file=sys.stderr)
        raise SystemExit(2) from error
