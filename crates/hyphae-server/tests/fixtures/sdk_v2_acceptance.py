from __future__ import annotations

import os
import threading
import time

from hyphae_sdk.v2 import CancellationToken, HyphaeClient, ProductError, RequestOptions


def fields(error: ProductError) -> dict[str, object]:
    return error.fields.__dict__


local = HyphaeClient.local(os.environ["HYPHAE_SOCKET"])
denied_local = HyphaeClient.local(
    os.environ["HYPHAE_SOCKET"], client_identity=os.environ["HYPHAE_DENIED_IDENTITY"]
)
http = HyphaeClient.http(os.environ["HYPHAE_ORIGIN"], bearer_token=os.environ["HYPHAE_TOKEN"])
denied_http = HyphaeClient.http(os.environ["HYPHAE_ORIGIN"])

cases = [
    ("sql_invalid_syntax", lambda client, options: client.sql("SELEC bad", options=options)),
    ("catalog_object_not_found", lambda client, options: client.catalog_object(999, options=options)),
    ("limit_exceeded", lambda client, options: client.sql("SELECT id FROM proof_items", options=options)),
]
for offset, (code, call) in enumerate(cases):
    request_id = 20_100 + offset
    limits = dict(RequestOptions().limits)
    if code == "limit_exceeded":
        limits["max_request_bytes"] = 1
    options = RequestOptions(request_id=request_id, limits=limits)
    errors = []
    for client in (local, http):
        try:
            call(client, options)
            raise AssertionError(f"{code} accepted")
        except ProductError as error:
            errors.append(fields(error))
    assert errors[0] == errors[1]
    assert errors[0]["code"] == code and errors[0]["request_id"] == request_id

expired_errors = []
for client in (local, http):
    expired = RequestOptions(
        request_id=20_110, deadline_micros=time.time_ns() // 1000 + 100
    )
    try:
        client.prove_sql("SELECT label FROM proof_items WHERE id = ?", [7], options=expired)
        raise AssertionError("expired request accepted")
    except ProductError as error:
        expired_errors.append(fields(error))
assert expired_errors[0] == expired_errors[1]
assert expired_errors[0]["code"] == "deadline_exceeded"

cancelled_errors = []
for transport, client in (("local", local), ("http", http)):
    token = CancellationToken()
    if transport == "local":
        threading.Timer(0.0001, token.cancel).start()
    else:
        token.cancel()
    cancelled = RequestOptions(request_id=20_111, cancellation=token)
    try:
        client.prove_sql("SELECT label FROM proof_items WHERE id = ?", [7], options=cancelled)
        raise AssertionError("cancelled request accepted")
    except ProductError as error:
        cancelled_errors.append(fields(error))
assert cancelled_errors[0] == cancelled_errors[1]
assert cancelled_errors[0]["code"] == "cancelled"

authorization_errors = []
for client in (denied_local, denied_http):
    try:
        client.structure_get(b"denied", options=RequestOptions(request_id=20_112))
        raise AssertionError("unauthorized request accepted")
    except ProductError as error:
        authorization_errors.append(fields(error))
assert authorization_errors[0] == authorization_errors[1]
assert authorization_errors[0]["code"] == "authorization_denied"

proven = http.prove_sql(
    "SELECT label FROM proof_items WHERE id = ?", [7], options=RequestOptions(request_id=20_120)
)
assert proven.kind == "proven"
assert proven.value["proof"].startswith(b"HYNPRF02")
assert proven.value["witness"].startswith(b"HYNWIT02")
proof_local = HyphaeClient.local(os.environ["HYPHAE_SOCKET"])
verified = proof_local.verify_proof(
    proven.value["proof"], proven.value["witness"], proven.value["trusted_anchor"],
    options=RequestOptions(request_id=20_121),
)
assert verified.kind == "proof_verification"
assert verified.value["semantic_reexecution_performed"]
proof_local.close()

with open(os.environ["HYPHAE_PYTHON_ARTIFACT"], "wb") as artifact:
    for value in (proven.value["proof"], proven.value["witness"], proven.value["trusted_anchor"]):
        artifact.write(len(value).to_bytes(8, "little"))
        artifact.write(value)

proof_local = HyphaeClient.local(os.environ["HYPHAE_SOCKET"])
proven = proof_local.prove_sql(
    "SELECT label FROM proof_items WHERE id = ?", [7], options=RequestOptions(request_id=20_122)
)
assert proven.value["proof"].startswith(b"HYNPRF02")
assert proven.value["witness"].startswith(b"HYNWIT02")
verified = http.verify_proof(
    proven.value["proof"], proven.value["witness"], proven.value["trusted_anchor"],
    options=RequestOptions(request_id=20_123),
)
assert verified.kind == "proof_verification"
assert verified.value["semantic_reexecution_performed"]
proof_local.close()

for offset, client in enumerate((local, http)):
    row_id = 20_200 + offset
    begun = client.transaction_begin(options=RequestOptions(request_id=20_200 + offset * 10))
    assert begun.kind == "explicit_transaction_status"
    assert begun.value["state"] == "active"
    handle = begun.value["handle"]
    staged = client.transaction_stage_sql(
        handle,
        "INSERT INTO proof_items (id, label) VALUES (?, ?)",
        [row_id, f"python-{offset}"],
        options=RequestOptions(request_id=20_201 + offset * 10),
    )
    assert staged.kind == "transaction_staged"
    assert staged.value["result"]["kind"] == "sql"
    rolled_back = client.transaction_rollback(
        handle, options=RequestOptions(request_id=20_202 + offset * 10)
    )
    assert rolled_back.kind == "transaction_rolled_back"
    selected = client.sql(
        "SELECT label FROM proof_items WHERE id = ?",
        [row_id],
        options=RequestOptions(request_id=20_203 + offset * 10),
    )
    assert selected.value["result"]["rows"] == []

local.close()
denied_local.close()
