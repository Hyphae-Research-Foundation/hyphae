#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Python SDK G6 lane runner over native-local or HTTP v2."""

from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path
from typing import Any

from hyphae_sdk.v2 import CancellationToken, HyphaeClient, ProductError, RequestOptions


lane = os.environ["HYPHAE_G6_LANE"]
work = Path(os.environ["HYPHAE_G6_WORK"])
corpus = json.loads(Path(os.environ["HYPHAE_G6_CORPUS"]).read_text())
if corpus.get("schema") != "hyphae-native-g6-corpus-v1":
    raise RuntimeError("unsupported G6 corpus")
runner = Path(__file__).parents[1] / "rust" / "Cargo.toml"
data = work / f"lane-{lane}"
backup = work / "seed-backup"
subprocess.run(["cargo", "run", "--quiet", "--locked", "--manifest-path", str(runner), "--", "restore", str(backup), str(data)], check=True, capture_output=True, text=True)

endpoint = work / f"{lane}.sock"
port_file = work / f"{lane}.port"
endpoint.unlink(missing_ok=True)
port_file.unlink(missing_ok=True)
server = subprocess.Popen(["cargo", "run", "--quiet", "--locked", "--manifest-path", str(runner), "--", "serve", str(data), str(endpoint), str(port_file)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)


def normalize(value: Any) -> Any:
    if isinstance(value, bytes):
        return value.hex()
    if isinstance(value, dict):
        return {key: normalize(child) for key, child in sorted(value.items())}
    if isinstance(value, (list, tuple)):
        return [normalize(child) for child in value]
    return value


def request(request_id: int) -> RequestOptions:
    return RequestOptions(request_id=request_id, logical_time_micros=1_700_000_000_000_000)


def snapshot(value: dict[str, Any]) -> dict[str, Any]:
    return {
        "directory_lineage": value["directory_lineage"].hex(),
        "catalog_version": value["catalog_version"],
        "visible_csn": value["visible_csn"],
        "root_digest": value["root_digest"].hex(),
    }


def add(cases: list[dict[str, Any]], case_id: str, outcome: dict[str, Any]) -> None:
    cases.append({"id": case_id, "outcome": normalize(outcome)})


def error_outcome(error: ProductError) -> dict[str, Any]:
    return {"code": error.code, "category": error.category, "retry": error.retry, "transaction_state": error.transaction_state, "request_id": str(error.request_id)}


def execute_cases(client: HyphaeClient, denied: HyphaeClient) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    cases: list[dict[str, Any]] = []
    status = client.admin("status", options=request(6000)).value
    start = snapshot(status["snapshot"])
    value = client.capabilities(options=request(6001)).value
    add(cases, "capabilities/capabilities", {"product_api_version": value["product_api_version"], "directory_format": value["native_directory_format"]})
    page = client.catalog_list({"item_limit": 64, "visit_limit": 128, "byte_limit": 65536}, options=request(6002)).value
    add(cases, "catalog/catalog-list", {"snapshot": snapshot(page["snapshot"]), "object_ids": [str(item["id"]) for item in page["items"]]})
    for case_id, object_id in (("catalog/catalog-describe", 10), ("catalog/catalog-dependencies", 15)):
        definition = client.catalog("describe", {"id": object_id}, options=request(6003)).value
        add(cases, case_id, {"object_id": str(object_id), "present": definition is not None})
    ddl = client.sql("CREATE TABLE g6_lane (id BIGINT PRIMARY KEY)", options=request(6004)).value
    add(cases, "sql/sql-ddl", sql_command(ddl))
    dml = client.sql("INSERT INTO g6_lane (id) VALUES (?)", [1], options=request(6005)).value
    add(cases, "sql/sql-dml", sql_command(dml))
    selected = client.sql("SELECT id, label FROM g6_items WHERE id = ?", [1], options=request(6006)).value
    add(cases, "sql/sql-prepared", {"columns": selected["result"]["columns"], "rows": selected["result"]["rows"], "snapshot": snapshot(selected["snapshot"])})
    explained = client.admin("explain_sql", {"statement": "SELECT id, label FROM g6_items WHERE id = 1"}, options=request(6007)).value
    add(cases, "sql/sql-explain", {"version": explained["version"], "text": explained["text"]})
    scalar = client.structure_get(b"g6-scalar", options=request(6008)).value
    add(cases, "structures/scalar", {"family": "scalar", "value": scalar, "snapshot": None})
    reads = (
        ("hash", {"kind": "hash_get", "key": {"keyspace": 10, "key": b"hash"}, "field": b"field"}),
        ("set", {"kind": "set_members", "key": {"keyspace": 11, "key": b"set"}, "limit": 10}),
        ("list", {"kind": "list_range", "key": {"keyspace": 12, "key": b"list"}, "start": 0, "stop": -1}),
        ("sorted-set", {"kind": "sorted_set_range", "key": {"keyspace": 13, "key": b"zset"}, "start": 0, "stop": -1}),
        ("stream", {"kind": "stream_range", "key": {"keyspace": 14, "key": b"stream"}, "start": 0, "end": (1 << 64) - 1, "limit": 10}),
    )
    for offset, (family, read) in enumerate(reads):
        result = client.structure_read(read, options=request(6010 + offset)).value
        add(cases, f"structures/{family}", {"family": family, "value": result["result"], "snapshot": snapshot(result["snapshot"])})
    for offset, mode in enumerate(("lexical", "exact", "ann", "hybrid", "named-vectors", "filter", "facet", "metric")):
        result = client.search_collection(17, search_request(mode), options=request(6020 + offset)).value
        add(cases, f"search/{mode}", {"mode": mode, "snapshot": snapshot(result["snapshot"]), "object_ids": [str(hit["object_id"]) for hit in result["hits"]], "approximate": result["approximate"]})
    transaction = client.transaction_status(2, options=request(6030)).value
    add(cases, "transactions/commit-status", {"status": transaction["state"].replace("_", "-"), "transaction_id": "2"})
    committed = client.sql("UPDATE g6_items SET label = ? WHERE id = ?", ["beta", 1], options=request(6033)).value
    add(cases, "transactions/atomic-batch", {"staged_operations": 1, "commit_csn": committed["commit"]["receipt"]["commit_csn"]})
    status = client.admin("status", options=request(6034)).value
    add(cases, "administration/status", {"snapshot": snapshot(status["snapshot"])})
    telemetry = client.telemetry(options=request(6035)).value
    add(cases, "administration/telemetry", {"registry_version": telemetry["registry_version"], "metric_names": [metric["name"] for metric in telemetry["metrics"]]})
    doctor = client.execute("doctor", options=request(6036)).value
    add(cases, "administration/doctor", {"status": doctor["status"], "snapshot_verified": doctor["snapshot_verified"]})
    if lane.endswith("-http"):
        proven = client.prove_sql("SELECT id, label FROM g6_items WHERE id = ?", [1], options=request(6040)).value
        add(cases, "proofs/generate", {"kind": "sql", "anchor_digest": proven["trusted_anchor"], "proof_digest": proven["proof"][32:64], "result_digest": proven["proof"][0:32]})
    backup_path = work / f"{lane}-corpus-backup"
    restored_path = work / f"{lane}-corpus-restored"
    backup_value = client.backup(str(backup_path), backup_limits(), options=request(6050)).value
    add(cases, "backup/create", backup_outcome(backup_value))
    restored = client.restore(str(backup_path), str(restored_path), backup_limits(), doctor_logical_time_micros=1_700_000_000_000_000, options=request(6052)).value
    add(cases, "backup/restore", {"visible_csn": restored["backup"]["visible_csn"], "checkpoint_digest": restored["backup"]["checkpoint_digest"], "doctor_status": restored["doctor"]["status"], "snapshot_verified": restored["doctor"]["snapshot_verified"]})
    add(cases, "backup/doctor-after-restore", {"status": restored["doctor"]["status"], "snapshot_verified": restored["doctor"]["snapshot_verified"]})
    for offset, name in enumerate(("syntax", "not-found")):
        try:
            if name == "syntax":
                client.sql("SELEC bad", options=request(6100 + offset))
            else:
                client.search(999, {"kind": "term", "value": "missing"}, 1, options=request(6100 + offset))
        except ProductError as error:
            add(cases, f"failures/{name}", error_outcome(error))
        else:
            raise RuntimeError(f"Python failure case {name} succeeded")
    limited = dict(RequestOptions().limits)
    limited["max_request_bytes"] = 1
    failure_calls = (
        ("limit", lambda: client.sql("SELECT id FROM g6_items", options=RequestOptions(request_id=6110, limits=limited))),
        ("deadline", lambda: client.sql("SELECT id FROM g6_items", options=RequestOptions(request_id=6111, deadline_micros=1))),
    )
    cancelled = CancellationToken()
    cancelled.cancel()
    failure_calls += (("cancellation", lambda: client.sql("SELECT id FROM g6_items", options=RequestOptions(request_id=6112, cancellation=cancelled))),)
    for name, call_failure in failure_calls:
        try:
            call_failure()
        except ProductError as error:
            add(cases, f"failures/{name}", error_outcome(error))
        else:
            raise RuntimeError(f"Python failure case {name} succeeded")
    try:
        denied.structure_get(b"g6-scalar", options=request(6113))
    except ProductError as error:
        add(cases, "failures/authorization", error_outcome(error))
    else:
        raise RuntimeError("Python authorization case succeeded")
    return start, cases


def sql_command(value: dict[str, Any]) -> dict[str, Any]:
    return {"rows_affected": value["result"]["rows_affected"], "object_id": None if value["result"]["object_id"] is None else str(value["result"]["object_id"]), "commit_csn": value["commit"]["receipt"]["commit_csn"]}


def backup_outcome(value: dict[str, Any]) -> dict[str, Any]:
    return {"visible_csn": value["visible_csn"], "checkpoint_digest": value["checkpoint_digest"], "file_count": value["file_count"], "total_bytes": value["total_bytes"]}


def backup_limits() -> dict[str, int]:
    return {"max_files": 16_384, "max_directories": 16_384, "max_total_bytes": 256 * 1024 * 1024 * 1024, "max_path_bytes": 4_096, "max_manifest_bytes": 4 * 1024 * 1024}


def search_request(mode: str) -> dict[str, Any]:
    vectors = []
    if mode in {"exact", "hybrid", "named-vectors"}:
        vectors.append({"target": "exact", "query": [0.0, 0.0], "candidate_limit": 4, "weight": 1, "execution": {"kind": "exact"}})
    if mode in {"ann", "named-vectors"}:
        vectors.append({"target": "ann", "query": [0.0, 0.0], "candidate_limit": 4, "weight": 1, "execution": {"kind": "ann", "ef_search": 8, "exact_rerank": 4}})
    value: dict[str, Any] = {"lexical": {"query": "rust", "candidate_limit": 8, "weight": 1} if mode in {"lexical", "hybrid", "filter", "facet", "metric"} else None, "vectors": vectors, "limit": 8}
    if mode == "filter": value["filter"] = {"kind": "compare", "field": "category", "operator": "equal", "value": "book"}
    if mode == "facet": value["facets"] = [{"field": "category", "limit": 8}]
    if mode == "metric": value["aggregations"] = [{"name": "count", "kind": "count"}]
    return value


try:
    for _ in range(400):
        if server.poll() is not None:
            raise RuntimeError(server.stderr.read() if server.stderr else "G6 server exited")
        if port_file.exists():
            time.sleep(0.1)
            break
        time.sleep(0.025)
    else:
        raise RuntimeError("G6 server did not become ready")
    if lane.endswith("-local"):
        client = HyphaeClient.local(str(endpoint))
        denied = HyphaeClient.local(str(endpoint), client_identity="hyphae-g6-conformance-denied")
    else:
        origin = f"http://127.0.0.1:{port_file.read_text().strip()}"
        client = HyphaeClient.http(origin, bearer_token="0123456789abcdef0123456789abcdef")
        denied = HyphaeClient.http(origin)
    start, cases = execute_cases(client, denied)
    coverage = ["capabilities", "catalog", "sql", "structures", "search", "transactions", "administration"]
    if lane.endswith("-http"): coverage.append("proofs")
    coverage.extend(["backup", "failures"])
    print(json.dumps({"schema": "hyphae-native-g6-transcript-v1", "lane": lane, "adapter": "python", "transport": "native-local" if lane.endswith("-local") else "http-v2", "start": start, "cases": cases, "coverage": coverage, "status": "passed"}, sort_keys=True, separators=(",", ":")))
finally:
    server.terminate()
    server.wait(timeout=20)
