# Python SDK

`hyphae-sdk` is the synchronous, bounded Python client for APIs v1 and Native
v2. It requires Python 3.11 or newer, uses only the standard library at runtime,
and includes typed generated models plus a `py.typed` marker. The development
source package version is `1.1.0`; this guide does not claim PyPI publication
without a separate registry release and receipt.

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
