# Python SDK

`hyphae-sdk` is the bounded Python client for APIs v1 and Native v2. It requires
Python 3.11 or newer, uses only the standard library at runtime, and includes
typed generated models plus a `py.typed` marker. Native v2 has source-compatible
synchronous calls and an async adapter with an owned serial worker. The
development source package version is `1.1.0`; this guide does not claim PyPI
publication without a separate registry release and receipt.

The distribution is named `hyphae-sdk` and the import package is
`hyphae_sdk`. The unrelated `hyphae` distribution on PyPI is not this project.

## Test from this repository

```bash
PYTHONPATH=sdks/python/src \
  python -m unittest discover -s sdks/python/tests -v
```

## Use

```python
import os
from hyphae_sdk import HyphaeClient

client = HyphaeClient(
    "http://127.0.0.1:8787",
    bearer_token=os.getenv("HYPHAE_BEARER_TOKEN"),
    timeout_seconds=60.0,
    response_bytes=32 * 1024 * 1024,
    witness_bytes=512 * 1024 * 1024,
)

receipt = client.put({
    "records": [{"key_hex": "616c706861", "value": {"score": 10}}]
})
response = client.get({"key_hex": "616c706861"})
witness = client.download_witness(response.value["proof"])

print(receipt.value["status"], response.request_id, len(witness.value))
```

Methods are `capabilities`, `liveness`, `readiness`, `put`, `delete`, `get`,
`query`, `define_vector_space`, `put_vectors`, `delete_vectors`,
`retrieve_exact`, `define_lexical_index`, `retrieve_lexical`,
`retrieve_hybrid`, `download_witness`, and `download_retrieval_witness`.
Every result is an immutable `ApiResponse` containing `value` and
`request_id`. Python integers preserve Hyphae's full signed 64-bit document
domain; floating-point JSON is rejected. Generated success models are static
types, not runtime shape validators.

## Errors and bounds

- `HyphaeApiError` is a valid server-declared v1 error and exposes `status`,
  stable `code`, `request_id`, and `server_message`.
- `HyphaeClientError` covers local configuration, transport, deadline, size,
  media-type, request-ID, JSON contract, or witness verification failure.

The client accepts only a root HTTP(S) origin and rejects redirects before a
request can be replayed. One monotonic deadline starts before request
serialization. A cancelable watchdog shuts down the active CPython socket at
that absolute deadline, independently of peer progress, across response
headers and success, error, or witness bodies.

Operating-system DNS resolution happens before CPython exposes a socket and
has no portable synchronous cancellation hook. If resolution outlives the
deadline, the client fails closed before continuing after it returns. Alternate
Python implementations require separate transport validation. Witness download
validates the canonical path, BLAKE3 digest header, and exact length from the
proof.

See [public client semantics](../../docs/clients/v1.md),
[data model](../../docs/concepts/data-model.md), and
[error codes](../../docs/api/error-codes-v1.md).

## Native v2

`hyphae_sdk.v2.HyphaeClient` exposes one capabilities, catalog, SQL,
structure, search, administration, telemetry, doctor, backup, transaction
status, and proof-verification API over either `HyphaeClient.local(endpoint)`
or `HyphaeClient.http(origin)`. Local uses exact `HYPHLCL1` bytes over AF_UNIX
or a Windows `\\.\pipe\...` path; HTTP uses canonical product envelopes at
`/v2/execute`. Both reconstruct `ProductError` typed fields and accept
`RequestOptions` deadlines and cancellation.

Managed local sessions negotiate Native 1.2 and authenticate in the bounded
`HELLO` trailer. Security metadata responses contain no credential secret or
verifier, and every security mutation requires a caller-selected nonzero
idempotency token:

```python
import os
from pathlib import Path
from hyphae_sdk.v2 import HyphaeClient, RequestOptions

api_key_path = Path(os.environ["HYPHAE_NATIVE_API_KEY_FILE"])
api_key = api_key_path.read_text(encoding="ascii").removesuffix("\n")

with HyphaeClient.local_authenticated(
    "/var/run/hyphae.sock",
    api_key,
) as client:
    status = client.security_status()
    principal = client.security_principal_create(
        "analytics",
        options=RequestOptions(idempotency_token=1),
    )
```

`HYPHAE_NATIVE_API_KEY_FILE` must name a caller-controlled restricted regular
file (owner-only permissions on Unix, or an equivalent owning-account ACL on
Windows). The SDK receives the credential in memory but does not configure or
audit filesystem permissions. Do not place the credential value in argv,
logs, exceptions, or source control.

For Native v2 HTTP, a bearer credential may use `http://` only with a
canonical loopback host (`127.0.0.0/8`, `[::1]`, or exact `localhost`). Every
other managed origin requires `https://` and is rejected before a request can
carry the key. This rule does not alter the separate `/v1` Python client.

The same typed security methods work through
`HyphaeClient.http(origin, bearer_token=...)`: `security_status`,
`security_principal_list`, `security_role_list`, `security_assignment_list`,
`security_key_list`, `security_audit_read`, `security_principal_create`,
`security_principal_set_enabled`, `security_custom_role_create`,
`security_built_in_assignment_create`, `security_custom_assignment_create`,
and `security_assignment_revoke`.

### Native v2 lifecycle and async use

`HyphaeClient.close()`, `LocalTransport.close()`, and `HttpTransport.close()`
are idempotent and terminal. They overwrite the SDK-owned mutable copy of a
credential; they cannot overwrite the caller's original Python `str` or
temporary immutable header bytes. Prefer a context manager and delete the
caller's credential reference when it is no longer needed.

`AsyncHyphaeClient` owns exactly one worker thread. Cancellation marks the
request token, aborts only the matching active transport generation, and waits
for that worker to stop before propagating `CancelledError`. A queued request
cannot abort the request ahead of it. Page iterators issue the next request only
after the current page has been consumed.

```python
import asyncio
from hyphae_sdk.v2 import AsyncHyphaeClient


async def inspect_security(endpoint: str, api_key: str) -> None:
    async with AsyncHyphaeClient.local_authenticated(endpoint, api_key) as client:
        async for page in client.security_principal_pages(limit=100):
            for principal in page.value["items"]:
                print(principal["display_name"])

        async with await client.begin_transaction() as transaction:
            await transaction.stage_sql("insert into jobs values (1, 'ready')")
            await transaction.stage_structure({
                "kind": "string_set",
                "key": {"keyspace": 1, "key": b"job:1"},
                "value": b"ready",
            })
            await transaction.commit()


asyncio.run(inspect_security("/var/run/hyphae.sock", api_key))
```

An async transaction left active by its context rolls back. A cancelled or
transport-failed commit becomes terminal `outcome_unknown`; inspect
`transaction_id` when present and resolve it through the transaction-status
operation rather than issuing rollback or commit again. A local transport abort
during staging invalidates the session-local handle instead of attempting a
rollback on a replacement connection.

Local abort destroys that local protocol connection and its session-local
prepared statements and explicit transaction handles; a later operation
reconnects. HTTP abort closes only matching client-side sockets while preserving
the local session identity; terminal HTTP close also clears that identity.
Native HTTP does not yet expose a remote session-close request, so server-side
session state remains governed by its configured TTL. CPython does not expose a
portable way to interrupt DNS before a socket exists; after DNS returns, the
client fails closed before continuing an already cancelled or expired request.
Windows named-pipe cancellation uses `CancelSynchronousIo`; release evidence
comes from the hosted `windows-2025` gate over a real Win32 named-pipe peer.
That gate stalls WELCOME and response reads, requires task cancellation,
deadline expiry, and `aclose()` to interrupt within one second, and proves
clean reconnect after cancellation and deadline. Its retained receipt binds
the exact source commit/tree, installed wheel digest, and transcript digest.
