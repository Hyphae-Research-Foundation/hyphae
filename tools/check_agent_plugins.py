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
EXPECTED_TOOL_NAMES = (
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
)
EXPECTED_ANNOTATIONS = {
    "readOnlyHint": True,
    "destructiveHint": False,
    "idempotentHint": True,
    "openWorldHint": False,
}
EXPECTED_EXECUTION = {"taskSupport": "forbidden"}
EMPTY_INPUT_SCHEMA = {
    "type": "object",
    "properties": {},
    "additionalProperties": False,
}
CURSOR_SCHEMA = {
    "type": ["string", "null"],
    "maxLength": 128,
    "pattern": r"^hysec1:[1-9][0-9]*:principal:[0-9a-f]{32}$",
}
PRINCIPAL_INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "properties": {
        "cursor": CURSOR_SCHEMA,
        "limit": {
            "type": "integer",
            "minimum": 1,
            "maximum": 1000,
            "default": 100,
        },
    },
}
ERROR_OUTPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["schema", "error"],
    "properties": {
        "schema": {"const": "hyphae-native-mcp-tool-error-v1"},
        "error": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "code",
                "category",
                "message",
                "retry",
                "transaction_state",
                "request_id",
                "trace_id",
                "object_id",
                "transaction_id",
            ],
            "properties": {
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": r"^[a-z][a-z0-9_]*$",
                },
                "category": {
                    "enum": [
                        "invalid-request",
                        "not-found",
                        "conflict",
                        "limit",
                        "deadline",
                        "cancelled",
                        "authorization",
                        "corruption",
                        "unavailable",
                        "io",
                        "internal",
                    ]
                },
                "message": {"type": "string", "minLength": 1, "maxLength": 256},
                "retry": {
                    "enum": [
                        "never",
                        "same-request",
                        "new-snapshot",
                        "after-backoff",
                        "after-recovery",
                        "unknown-commit",
                    ]
                },
                "transaction_state": {
                    "enum": [
                        "none",
                        "active",
                        "rolled-back",
                        "committed",
                        "outcome-unknown",
                    ]
                },
                "request_id": {
                    "type": ["string", "null"],
                    "maxLength": 39,
                    "pattern": r"^(0|[1-9][0-9]{0,38})$",
                },
                "trace_id": {
                    "type": ["string", "null"],
                    "maxLength": 39,
                    "pattern": r"^(0|[1-9][0-9]{0,38})$",
                },
                "object_id": {
                    "type": ["string", "null"],
                    "maxLength": 20,
                    "pattern": r"^[1-9][0-9]{0,19}$",
                },
                "transaction_id": {
                    "type": ["string", "null"],
                    "maxLength": 20,
                    "pattern": r"^[1-9][0-9]{0,19}$",
                },
            },
        },
    },
}


def success_schemas() -> dict[str, dict[str, Any]]:
    positive_integer = {"type": "integer", "minimum": 1}
    nonnegative_integer = {"type": "integer", "minimum": 0}
    return {
        "hyphae_native_capabilities": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "product_api_version",
                "native_directory_format",
                "logical_catalog_codec_version",
                "catalog_tree_format_version",
                "limits",
            ],
            "properties": {
                "product_api_version": positive_integer,
                "native_directory_format": positive_integer,
                "logical_catalog_codec_version": positive_integer,
                "catalog_tree_format_version": positive_integer,
                "limits": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": [
                        "catalog_items",
                        "catalog_visits",
                        "catalog_bytes",
                        "sql_statement_bytes",
                        "sql_parameters",
                        "sql_rows",
                    ],
                    "properties": {
                        field: positive_integer
                        for field in (
                            "catalog_items",
                            "catalog_visits",
                            "catalog_bytes",
                            "sql_statement_bytes",
                            "sql_parameters",
                            "sql_rows",
                        )
                    },
                },
            },
        },
        "hyphae_native_security_status": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "schema",
                "bootstrapped",
                "authorization_epoch",
                "principals",
                "assignments",
                "custom_roles",
                "custom_assignments",
                "keys",
                "pending_keys",
                "audit_events",
            ],
            "properties": {
                "schema": {"const": "hyphae-native-access-control-status-v1"},
                "bootstrapped": {"type": "boolean"},
                **{
                    field: nonnegative_integer
                    for field in (
                        "authorization_epoch",
                        "principals",
                        "assignments",
                        "custom_roles",
                        "custom_assignments",
                        "keys",
                        "pending_keys",
                        "audit_events",
                    )
                },
            },
        },
        "hyphae_native_security_principals": {
            "type": "object",
            "additionalProperties": False,
            "required": ["schema", "authorization_epoch", "items", "next_cursor"],
            "properties": {
                "schema": {"const": "hyphae-native-security-principals-v1"},
                "authorization_epoch": nonnegative_integer,
                "items": {
                    "type": "array",
                    "maxItems": 1000,
                    "items": {
                        "type": "object",
                        "additionalProperties": False,
                        "required": ["id", "display_name", "enabled"],
                        "properties": {
                            "id": {"type": "string", "pattern": r"^[0-9a-f]{32}$"},
                            "display_name": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 128,
                            },
                            "enabled": {"type": "boolean"},
                        },
                    },
                },
                "next_cursor": CURSOR_SCHEMA,
            },
        },
    }


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
    if interface.get("capabilities") != ["Read"]:
        fail("Codex plugin must advertise the exact read-only MCP capability")
    long_description = str(interface.get("longDescription", ""))
    if "Auditor" not in long_description or "Instance" not in long_description:
        fail("Codex plugin must recommend an Instance-scoped Auditor API key")
    if "Reader" in long_description:
        fail("Codex plugin must not recommend a Reader API key")
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
    description = str(value.get("description", ""))
    if "Auditor" not in description or "Instance" not in description:
        fail("Claude Code plugin must recommend an Instance-scoped Auditor API key")
    if "Reader" in description:
        fail("Claude Code plugin must not recommend a Reader API key")


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
    description = str(plugins[0].get("description", ""))
    if "Auditor" not in description or "Instance" not in description:
        fail("Claude Code marketplace must recommend an Instance-scoped Auditor API key")
    if "Reader" in description:
        fail("Claude Code marketplace must not recommend a Reader API key")

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


def validate_skill(plugin: Path, expected_tools: set[str]) -> None:
    skill = plugin / "skills/use-hyphae/SKILL.md"
    text = skill.read_text(encoding="utf-8")
    if not text.startswith("---\n") or "name: use-hyphae" not in text:
        fail("shared Hyphae skill metadata is invalid")
    mentioned_tools = set(re.findall(r"hyphae_[a-z_]+", text))
    if mentioned_tools != expected_tools:
        fail("shared Hyphae skill diverges from the Native MCP tool registry")
    if any(term in text for term in ("hyphae_put", "hyphae_delete", "hyphae_query")):
        fail("shared Hyphae skill advertises an unavailable MCP mutation or query")
    if "Auditor" not in text or "Instance" not in text:
        fail("shared Hyphae skill must recommend an Auditor API key")
    if "Reader" in text:
        fail("shared Hyphae skill must not recommend a Reader API key")


def validate_plugin_readme(plugin: Path) -> None:
    text = (plugin / "README.md").read_text(encoding="utf-8")
    if (
        "HYPHAE_NATIVE_API_KEY_FILE" not in text
        or "Native HTTP v2" not in text
        or "read-only" not in text
        or "HYPHAE_BEARER_TOKEN_FILE" in text
        or "targets the shipped `/v1`" in text
    ):
        fail("plugin setup must document the managed Native v2 read-only boundary")
    if "Auditor" not in text or "Instance" not in text:
        fail("plugin setup must recommend an Instance-scoped Auditor API key")
    if "Reader" in text:
        fail("plugin setup must not recommend a Reader API key")


def validate_contract(contract: dict[str, Any]) -> tuple[str, ...]:
    if set(contract) != {
        "schema",
        "mcp_protocol",
        "tool_schema_version",
        "tool_page_size",
        "tools",
    }:
        fail("Native MCP contract envelope is invalid")
    if (
        contract.get("schema") != "hyphae-native-mcp-contract-v1"
        or contract.get("mcp_protocol") != "2025-11-25"
        or contract.get("tool_schema_version") != "hyphae-native-mcp-tools-v1"
    ):
        fail("Native MCP contract versions are invalid")
    if type(contract.get("tool_page_size")) is not int or contract.get("tool_page_size") != 2:
        fail("Native MCP tool page size must be exactly two")

    tools = contract.get("tools")
    if not isinstance(tools, list) or len(tools) != len(EXPECTED_TOOL_NAMES):
        fail("Native MCP contract tool registry is invalid")
    schemas = success_schemas()
    for index, expected_name in enumerate(EXPECTED_TOOL_NAMES):
        tool = tools[index]
        if not isinstance(tool, dict) or set(tool) != {
            "name",
            "description",
            "inputSchema",
            "outputSchema",
            "annotations",
            "execution",
        }:
            fail("Native MCP tool definition is invalid")
        description = tool.get("description")
        if (
            tool.get("name") != expected_name
            or not isinstance(description, str)
            or not 1 <= len(description) <= 256
        ):
            fail("Native MCP contract tool identities are invalid")
        if tool.get("annotations") != EXPECTED_ANNOTATIONS:
            fail("Native MCP tool annotations must be exact read-only hints")
        if tool.get("execution") != EXPECTED_EXECUTION:
            fail("Native MCP tasks must be forbidden")
        expected_input = (
            PRINCIPAL_INPUT_SCHEMA
            if expected_name == "hyphae_native_security_principals"
            else EMPTY_INPUT_SCHEMA
        )
        if tool.get("inputSchema") != expected_input:
            fail(f"Native MCP {expected_name} input schema is invalid")
        expected_output = {"oneOf": [schemas[expected_name], ERROR_OUTPUT_SCHEMA]}
        if tool.get("outputSchema") != expected_output:
            fail(f"Native MCP {expected_name} output schema is invalid or unredacted")
    return EXPECTED_TOOL_NAMES


def validate(root: Path = ROOT) -> dict[str, Any]:
    plugin = root / "plugins/hyphae"
    files = [
        plugin / ".mcp.json",
        plugin / ".codex-plugin/plugin.json",
        plugin / ".claude-plugin/plugin.json",
        plugin / "README.md",
        plugin / "skills/use-hyphae/SKILL.md",
        root / ".claude-plugin/marketplace.json",
        root / ".agents/plugins/marketplace.json",
        root / "contracts/native-mcp-v2.json",
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
    expected_tools = set(
        validate_contract(load_object(root / "contracts/native-mcp-v2.json", root))
    )
    validate_skill(plugin, expected_tools)
    validate_plugin_readme(plugin)
    return {
        "status": "passed",
        "hosts": ["claude-code", "codex"],
        "mcp_servers": 1,
        "tools": len(expected_tools),
    }


def main() -> int:
    print(json.dumps(validate(), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
