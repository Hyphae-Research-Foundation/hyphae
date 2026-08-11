#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Live MCP stdio runner for the shared version 1 fixture."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = json.loads((ROOT / "conformance" / "v1" / "cases.json").read_text("utf-8"))

process = subprocess.Popen(
    [
        os.environ["HYPHAE_MCP_BIN"],
        "mcp",
        "--base-url",
        os.environ["HYPHAE_BASE_URL"],
    ],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    encoding="utf-8",
    bufsize=1,
)
assert process.stdin is not None and process.stdout is not None
responses: queue.Queue[str] = queue.Queue()


def read_responses() -> None:
    assert process.stdout is not None
    for line in process.stdout:
        responses.put(line)


reader = threading.Thread(target=read_responses, daemon=True)
reader.start()
next_id = 1


def send(message: object) -> None:
    assert process.stdin is not None
    process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    process.stdin.flush()


def request(method: str, params: object | None = None) -> dict[str, Any]:
    global next_id
    identifier = next_id
    next_id += 1
    message: dict[str, Any] = {"jsonrpc": "2.0", "id": identifier, "method": method}
    if params is not None:
        message["params"] = params
    send(message)
    try:
        response = json.loads(responses.get(timeout=10))
    except queue.Empty as error:
        stderr = process.stderr.read() if process.poll() is not None and process.stderr else ""
        raise RuntimeError(f"MCP response timeout: {stderr}") from error
    assert response["jsonrpc"] == "2.0" and response["id"] == identifier
    if "error" in response:
        raise RuntimeError(f"MCP protocol error: {response['error']}")
    return response["result"]


def call(name: str, arguments: object) -> dict[str, Any]:
    result = request("tools/call", {"name": name, "arguments": arguments})
    assert result.get("isError") is False, result
    assert json.loads(result["content"][0]["text"]) == result["structuredContent"]
    return result["structuredContent"]


try:
    initialized = request(
        "initialize",
        {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "hyphae-conformance", "version": "1"},
        },
    )
    assert initialized["protocolVersion"] == "2025-11-25"
    assert initialized["capabilities"]["tools"]["listChanged"] is False
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

    listed = request("tools/list", {})["tools"]
    assert [tool["name"] for tool in listed] == [
        "hyphae_capabilities",
        "hyphae_put",
        "hyphae_get",
        "hyphae_delete",
        "hyphae_query",
        "hyphae_define_vector_space",
        "hyphae_put_vectors",
        "hyphae_delete_vectors",
        "hyphae_retrieve_exact",
        "hyphae_define_lexical_index",
        "hyphae_retrieve_lexical",
        "hyphae_retrieve_hybrid",
    ]
    assert all(tool["inputSchema"]["type"] == "object" for tool in listed)
    assert all(tool["outputSchema"]["type"] == "object" for tool in listed)

    capabilities = call("hyphae_capabilities", {})
    assert capabilities["api_version"] == "v1"
    assert capabilities["features"] == sorted(set(capabilities["features"]))
    assert call("hyphae_put", FIXTURE["put_request"])["status"] == "committed"
    assert call("hyphae_put", FIXTURE["put_request"])["status"] == "existing"
    conflict = request(
        "tools/call",
        {"name": "hyphae_put", "arguments": FIXTURE["conflict_put_request"]},
    )
    assert conflict["isError"] is True
    assert "idempotency_conflict" in conflict["content"][0]["text"]

    present = call("hyphae_get", FIXTURE["present_get_request"])
    assert present["found"] is True and present["record"]["key_hex"] == "61"
    absent = call("hyphae_get", FIXTURE["absent_get_request"])
    assert absent["found"] is False and absent.get("record") is None

    first_page = call("hyphae_query", FIXTURE["query_request"])
    assert [record["key_hex"] for record in first_page["rows"]] == FIXTURE["expected"]["first_page_keys"]
    assert first_page["matched_records"] == FIXTURE["expected"]["matched_records"]
    assert first_page.get("aggregation") == FIXTURE["expected"]["aggregation"]
    second_page = call(
        "hyphae_query",
        {**FIXTURE["query_request"], "cursor": first_page.get("next_cursor")},
    )
    assert [record["key_hex"] for record in second_page["rows"]] == FIXTURE["expected"]["second_page_keys"]
    assert second_page.get("next_cursor") is None

    assert call("hyphae_define_vector_space", FIXTURE["define_vector_space_request"])["status"] == "committed"
    assert call("hyphae_define_vector_space", FIXTURE["define_vector_space_request"])["status"] == "existing"
    invalid_vectors = request(
        "tools/call",
        {"name": "hyphae_put_vectors", "arguments": FIXTURE["invalid_put_vectors_request"]},
    )
    assert invalid_vectors["isError"] is True
    assert "invalid_request" in invalid_vectors["content"][0]["text"]
    assert call("hyphae_put_vectors", FIXTURE["put_vectors_request"])["status"] == "committed"
    assert call("hyphae_define_lexical_index", FIXTURE["define_lexical_index_request"])["status"] == "committed"

    exact = call("hyphae_retrieve_exact", FIXTURE["exact_retrieval_request"])
    assert exact["outcome"]["status"] == "matches"
    assert [item["key_hex"] for item in exact["outcome"]["matches"]] == FIXTURE["expected"]["exact_retrieval_keys"]
    ambiguous = call("hyphae_retrieve_exact", FIXTURE["ambiguous_exact_retrieval_request"])
    assert ambiguous["outcome"]["status"] == "abstained"
    assert ambiguous["outcome"]["abstention"]["reason"] == FIXTURE["expected"]["ambiguous_exact_reason"]
    wrong_dimension = request(
        "tools/call",
        {
            "name": "hyphae_retrieve_exact",
            "arguments": FIXTURE["wrong_dimension_exact_retrieval_request"],
        },
    )
    assert wrong_dimension["isError"] is True
    assert "invalid_request" in wrong_dimension["content"][0]["text"]

    lexical = call("hyphae_retrieve_lexical", FIXTURE["lexical_retrieval_request"])
    assert lexical["outcome"]["status"] == "matches"
    assert lexical["outcome"]["matches"][0]["key_hex"] == FIXTURE["expected"]["lexical_first_key"]
    invalid_lexical = request(
        "tools/call",
        {
            "name": "hyphae_retrieve_lexical",
            "arguments": FIXTURE["invalid_lexical_retrieval_request"],
        },
    )
    assert invalid_lexical["isError"] is True
    assert "invalid_request" in invalid_lexical["content"][0]["text"]

    hybrid = call("hyphae_retrieve_hybrid", FIXTURE["hybrid_retrieval_request"])
    assert hybrid["outcome"]["status"] == "matches"
    assert hybrid["outcome"]["matches"][0]["key_hex"] == FIXTURE["expected"]["hybrid_first_key"]
    assert call("hyphae_delete_vectors", FIXTURE["delete_vectors_request"])["status"] == "committed"

    call("hyphae_delete", FIXTURE["delete_request"])
    assert call("hyphae_get", {"key_hex": "62"})["found"] is False
    print(json.dumps({"client": "mcp", "status": "ok"}, separators=(",", ":")))
finally:
    if process.stdin is not None:
        process.stdin.close()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
