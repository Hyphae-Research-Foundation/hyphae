# TypeScript SDK

`@celiums/hyphae` is the bounded ESM client for APIs v1 and Native v2. It requires Node.js 20
or newer, uses the runtime `fetch`, and has no runtime package dependencies.
The release source package version is `1.0.1`. It is maintained in this repository;
this guide does not claim npm publication without a separate registry release
and receipt.

## Build from this repository

```bash
cd sdks/typescript
npm ci --ignore-scripts
npm test
```

## Use

```typescript
import { HyphaeClient } from "@celiums/hyphae";

const bearerToken = process.env.HYPHAE_BEARER_TOKEN;
const client = new HyphaeClient("http://127.0.0.1:8787", {
  ...(bearerToken === undefined ? {} : { bearerToken }),
  timeoutMs: 60_000,
  responseBytes: 32 * 1024 * 1024,
  witnessBytes: 512 * 1024 * 1024,
});

const receipt = await client.put({
  records: [{ key_hex: "616c706861", value: { score: 10 } }],
});
const response = await client.get({ key_hex: "616c706861" });
const witness = await client.downloadWitness(response.value.proof);

console.log(receipt.value.status, response.requestId, witness.value.byteLength);
```

Methods are `capabilities`, `liveness`, `readiness`, `put`, `delete`, `get`,
`query`, `defineVectorSpace`, `putVectors`, `deleteVectors`, `retrieveExact`,
`defineLexicalIndex`, `retrieveLexical`, `retrieveHybrid`, `downloadWitness`,
and `downloadRetrievalWitness`. Every result is `{ value, requestId }`.
Generated success models provide static typing; the client does not validate
each successful response shape at runtime.

## Exact integers

Hyphae documents use signed 64-bit integers. The SDK parser returns safe values
as `number` and larger values as `bigint`; serialization rejects an unsafe
`number` rather than losing precision.

```typescript
await client.put({
  records: [{ key_hex: "6d6178", value: 9223372036854775807n }],
});
```

Generated models use `HyphaeJsonInteger` for this dual representation.

## Errors and bounds

- `HyphaeApiError` is a valid server-declared v1 error and exposes HTTP
  `status`, stable `code`, and `requestId`.
- `HyphaeClientError` is local configuration, transport, timeout, size,
  media-type, request-ID, JSON contract, or witness verification failure.

The client accepts only a root HTTP(S) origin without embedded credentials,
path, query, or fragment. Witness download requires the canonical proof path,
`Digest: blake3=...`, and exact declared length. A custom `fetch` can be
injected for an audited runtime or tests.

One monotonic deadline starts before request serialization and governs
`fetch`, response headers, success/error bodies, and witness bodies. Redirect
following is disabled. The client races an injected `fetch` and every body read
against its own deadline, so a transport that ignores `AbortSignal` cannot make
the client accept a late result. Such a transport can still continue external
work after rejection if its own implementation ignores cancellation; that work
is outside the SDK's control.

See [public client semantics](../../docs/clients/v1.md),
[data model](../../docs/concepts/data-model.md), and
[error codes](../../docs/api/error-codes-v1.md).

## Native v2

The `@celiums/hyphae/v2` export provides one high-level API over
`HyphaeClient.local(endpoint)` and `HyphaeClient.http(origin)`. The Node local
connector uses AF_UNIX paths on Unix and `\\.\pipe\...` named-pipe paths on
Windows and carries exact `HYPHLCL1` frames without a wrapper protocol. HTTP
uses canonical product envelopes at `/v2/execute`. Both expose typed product
errors, request deadlines, `AbortSignal` cancellation, and transaction state.
