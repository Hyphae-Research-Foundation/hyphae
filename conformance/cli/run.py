#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Live command-line-client runner for the shared version 1 fixture."""

from __future__ import annotations

import base64
import json
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = json.loads((ROOT / "conformance" / "v1" / "cases.json").read_text("utf-8"))


def call(operation: str, request: object | None = None) -> dict[str, Any]:
    binary = os.environ["HYPHAE_CLI_BIN"]
    command = [
        binary,
        "remote",
        "--base-url",
        os.environ["HYPHAE_BASE_URL"],
        operation,
    ]
    if request is None:
        completed = subprocess.run(
            command, check=True, capture_output=True, text=True, timeout=60
        )
    else:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", suffix=".json"
        ) as request_file:
            json.dump(request, request_file, separators=(",", ":"))
            request_file.flush()
            completed = subprocess.run(
                [*command, "--request", request_file.name],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
    return json.loads(completed.stdout)


assert FIXTURE["version"] == 1
assert call("liveness")["status"] == "live"
capabilities = call("capabilities")
assert capabilities["api_version"] == "v1"
assert capabilities["features"] == sorted(set(capabilities["features"]))

assert call("put", FIXTURE["put_request"])["status"] == "committed"
assert call("put", FIXTURE["put_request"])["status"] == "existing"
try:
    call("put", FIXTURE["conflict_put_request"])
except subprocess.CalledProcessError as error:
    assert "idempotency_conflict" in error.stderr
else:
    raise AssertionError("idempotency conflict was accepted")

present = call("get", FIXTURE["present_get_request"])
assert present["found"] is True and present["record"]["key_hex"] == "61"
with tempfile.TemporaryDirectory() as directory:
    proof_path = Path(directory) / "proof.json"
    witness_path = Path(directory) / "snapshot.hysnap"
    proof_path.write_text(json.dumps(present["proof"]), encoding="utf-8")
    completed = subprocess.run(
        [
            os.environ["HYPHAE_CLI_BIN"],
            "remote",
            "--base-url",
            os.environ["HYPHAE_BASE_URL"],
            "witness",
            "--proof",
            str(proof_path),
            "--out",
            str(witness_path),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    metadata = json.loads(completed.stdout)
    assert metadata["file_bytes"] == witness_path.stat().st_size
    assert witness_path.read_bytes().startswith(b"HYSNAP01")

absent = call("get", FIXTURE["absent_get_request"])
assert absent["found"] is False and absent.get("record") is None

first_page = call("query", FIXTURE["query_request"])
assert [record["key_hex"] for record in first_page["rows"]] == FIXTURE["expected"]["first_page_keys"]
assert first_page["matched_records"] == FIXTURE["expected"]["matched_records"]
assert first_page.get("aggregation") == FIXTURE["expected"]["aggregation"]
second_page = call(
    "query", {**FIXTURE["query_request"], "cursor": first_page.get("next_cursor")}
)
assert [record["key_hex"] for record in second_page["rows"]] == FIXTURE["expected"]["second_page_keys"]
assert second_page.get("next_cursor") is None

assert call("define-vector-space", FIXTURE["define_vector_space_request"])["status"] == "committed"
assert call("define-vector-space", FIXTURE["define_vector_space_request"])["status"] == "existing"
try:
    call("put-vectors", FIXTURE["invalid_put_vectors_request"])
except subprocess.CalledProcessError as error:
    assert "invalid_request" in error.stderr
else:
    raise AssertionError("mixed-validity vector batch was accepted")
assert call("put-vectors", FIXTURE["put_vectors_request"])["status"] == "committed"
assert call("define-lexical-index", FIXTURE["define_lexical_index_request"])["status"] == "committed"

exact = call("retrieve-exact", FIXTURE["exact_retrieval_request"])
assert exact["outcome"]["status"] == "matches"
assert [item["key_hex"] for item in exact["outcome"]["matches"]] == FIXTURE["expected"]["exact_retrieval_keys"]
with tempfile.TemporaryDirectory() as directory:
    proof_json_path = Path(directory) / "proof.json"
    proof_binary_path = Path(directory) / "proof.hyrproof"
    witness_path = Path(directory) / "snapshot.hysnap"
    proof_json_path.write_text(json.dumps(exact["proof"]), encoding="utf-8")
    proof_binary_path.write_bytes(base64.b64decode(exact["proof"]["data"], validate=True))
    completed = subprocess.run(
        [
            os.environ["HYPHAE_CLI_BIN"],
            "remote",
            "--base-url",
            os.environ["HYPHAE_BASE_URL"],
            "witness",
            "--proof",
            str(proof_json_path),
            "--out",
            str(witness_path),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    metadata = json.loads(completed.stdout)
    assert metadata["file_bytes"] == witness_path.stat().st_size
    verified = subprocess.run(
        [
            os.environ["HYPHAE_CLI_BIN"],
            "verify-retrieval",
            "--kind",
            "exact",
            "--proof",
            str(proof_binary_path),
            "--snapshot",
            str(witness_path),
            "--anchor",
            exact["proof"]["anchor_digest"],
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    report = json.loads(verified.stdout)
    assert report["status"] == "verified" and report["operation"] == "exact"
ambiguous = call("retrieve-exact", FIXTURE["ambiguous_exact_retrieval_request"])
assert ambiguous["outcome"]["status"] == "abstained"
assert ambiguous["outcome"]["abstention"]["reason"] == FIXTURE["expected"]["ambiguous_exact_reason"]
try:
    call("retrieve-exact", FIXTURE["wrong_dimension_exact_retrieval_request"])
except subprocess.CalledProcessError as error:
    assert "invalid_request" in error.stderr
else:
    raise AssertionError("wrong-dimension query was accepted")

lexical = call("retrieve-lexical", FIXTURE["lexical_retrieval_request"])
assert lexical["outcome"]["status"] == "matches"
assert lexical["outcome"]["matches"][0]["key_hex"] == FIXTURE["expected"]["lexical_first_key"]
try:
    call("retrieve-lexical", FIXTURE["invalid_lexical_retrieval_request"])
except subprocess.CalledProcessError as error:
    assert "invalid_request" in error.stderr
else:
    raise AssertionError("empty lexical query was accepted")

hybrid = call("retrieve-hybrid", FIXTURE["hybrid_retrieval_request"])
assert hybrid["outcome"]["status"] == "matches"
assert hybrid["outcome"]["matches"][0]["key_hex"] == FIXTURE["expected"]["hybrid_first_key"]
assert call("delete-vectors", FIXTURE["delete_vectors_request"])["status"] == "committed"

call("delete", FIXTURE["delete_request"])
assert call("get", {"key_hex": "62"})["found"] is False

print(json.dumps({"client": "cli", "status": "ok"}, separators=(",", ":")))
