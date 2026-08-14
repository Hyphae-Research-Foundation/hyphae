Most data engines can answer a query. I wanted one that could also explain, offline and byte for byte, why that answer belongs to a specific durable state.

That question led me to build **Hyphae**, an open-source data engine in Rust with a deliberately small operational footprint:

- one native binary;
- one data directory;
- no required database, cache, cloud service, embedding provider, or LLM;
- deterministic KV and structured queries;
- portable result proofs that can be verified offline.

If you read my [earlier post about provenance over prediction](https://dev.to/terrizoaguimor/why-im-building-hyphae-provenance-over-prediction-and-the-3-line-baseline-that-tied-it-2e32), this is the next and deliberately narrower step. That work began as a cognitive substrate and clarified the provenance thesis. I have since rebuilt Hyphae as a standalone data engine. The research lineage remains, but the current product does not need an AI stack to be useful.

Hyphae `0.1.0` is now available on [GitHub](https://github.com/celiumsai/hyphae), through crates.io, and as signed multiplatform archives in the [first public release](https://github.com/celiumsai/hyphae/releases/tag/v0.1.0).

## The problem was not another query syntax

The usual path for an application data feature grows surprisingly quickly. A local library becomes a database service. Search adds another service. Caching adds another. Semantic retrieval adds a model provider. Soon, a feature that should be optional controls whether the application can start at all.

There is a second problem hiding underneath that operational stack: a successful response is usually just an assertion from the system that produced it.

If I receive a filtered, sorted, limited result, how do I check that:

1. the underlying durable state was not corrupted;
2. the query was executed with the declared semantics;
3. matching rows were not silently omitted;
4. the returned result is tied to the state I actually intended to trust?

Checksums help with corruption. Signatures can identify a producer. Neither one, by itself, proves that an arbitrary query result is complete and was reexecuted correctly.

Hyphae is my attempt to make those concerns part of the engine instead of application glue.

## What Hyphae is

Hyphae is an autonomous, embeddable, and verifiable Rust data engine. Its durable authority is an append-only, checksummed, digest-chained log. Embedded indexes are rebuildable accelerators, not the source of truth.

The engine currently provides:

- atomic structured KV mutations;
- durable idempotency through caller-visible UUID transaction IDs;
- recovery that fails closed on complete corruption and truncates only an incomplete final frame;
- deterministic filters, global sorting, logical cursors, and grouped or global aggregations;
- canonical snapshots, anchored compaction, backup, restore, and diagnostics;
- exact provider-neutral vector retrieval in the Rust API, with explicit abstention;
- proof-bearing reads and complete offline reexecution;
- an optional, loopback-first OpenAPI `/v1` server;
- Rust, TypeScript, Python, CLI, and MCP clients over public versioned contracts.

The base product works with AI completely absent. A host may supply vectors to the Rust retrieval API, but Hyphae does not create embeddings and does not make a provider part of durable authority.

## Try the single-binary flow

Install the public CLI from crates.io:

```bash
cargo install hyphae-cli --version 0.1.0 --locked
hyphae version --json
```

Choose a data directory and write two structured records:

```bash
export HYPHAE_DATA_DIR="$PWD/hyphae-data"

hyphae put --key alpha --json '{"group":"edge","score":10}'
hyphae put --key beta  --json '{"group":"edge","score":20}'
hyphae get --key alpha
```

Then run a deterministic query without a server or an AI dependency:

```bash
hyphae query \
  --field group --equals '"edge"' \
  --sort score --descending --limit 2
```

Every command reopens and verifies the same durable directory. A mutation returns its transaction ID and commit digests. An exact retry of the same transaction returns the original receipt; reusing that UUID for different operations is rejected.

## From a result to a verifiable result

Add `--proof-out` to create a portable result proof:

```bash
hyphae query \
  --field group --equals '"edge"' \
  --sort score --descending --limit 2 \
  --proof-out result.hyproof
```

The JSON response identifies three things:

- the canonical `.hyproof` file;
- the exact snapshot witness;
- the anchor digest for the snapshot and verified log checkpoint.

After pinning that anchor through a channel the verifier trusts, the result can be checked without opening the live data directory or contacting a network:

```bash
hyphae verify \
  --proof result.hyproof \
  --snapshot '<proof.snapshot_path>' \
  --anchor '<proof.anchor_digest>'
```

Verification checks the proof framing, CRC32C and BLAKE3 digests, the caller-supplied anchor, and the complete snapshot. It then reexecutes the embedded operation and requires an exact result match.

That last step matters. Hyphae does not only prove that some bytes were preserved. It checks that the declared query over the complete witness produces the declared answer.

There is also an important limit to state plainly: **a self-consistent proof is not an external trust anchor**. If an attacker controls the proof, snapshot, and the expected anchor, cryptographic consistency alone cannot establish which historical state should be trusted. The expected anchor must be pinned independently by the verifier.

## The architecture in one picture

```text
application
  |-- embedded Rust facade --------------------------|
  |-- local CLI                                      |
  `-- /v1 clients (Rust / TypeScript / Python / MCP) |
                         |                           |
                   optional HTTP server              |
                         `-------------+-------------'
                                       |
                          engine: KV / query / proof
                                       |
                         append-only durable log
                            |                  |
                     canonical snapshots  rebuildable index
```

The write path acknowledges a mutation only after its canonical frames and commit frame are appended and synchronized. The embedded index is then updated atomically. If that second stage fails, the log still contains the valid commit, the live handle refuses potentially stale reads, and the next open verifies and replays the missing work.

Snapshots serialize logical KV state rather than copying database internals. Compaction first commits the snapshot and next log generation through a new immutable manifest, then retires the previous segment. An interrupted transition therefore has one unambiguous winner.

## Four design choices that shaped 0.1.0

### 1. No partial success disguised as a result

Structured query has hard budgets for scanned records, matches, returned rows, groups, query shape, time, proof size, and response size. If a budget or timeout is exceeded, the operation returns an error and no partial logical result.

For a proof-bearing engine, a plausible-looking prefix is worse than a loud failure.

### 2. The log is authority; indexes are disposable

Treating the embedded index as rebuildable simplified the recovery contract. A checkpoint is accepted only when its sequence and digest identify the same verified log commit. If the index is missing, Hyphae can reconstruct it. If durable history is corrupt, it fails closed.

### 3. AI is an optional input, never a prerequisite

KV, structured query, aggregation, recovery, snapshots, backup, and proof verification all work without embeddings or an LLM. Exact cosine retrieval accepts vectors supplied by a Rust host and can abstain when there are no candidates, the score is insufficient, or the top margin is ambiguous.

This keeps semantic providers replaceable and outside the storage trust boundary.

### 4. Integrations depend on Hyphae, not the reverse

The wire contract is OpenAPI 3.1 plus JSON Schema 2020-12 under a versioned `/v1` surface. Framework adapters are optional consumers. Astro, Next, Vite, PliegoRS, or any other host must continue to build and run when Hyphae is absent.

That boundary is less exciting than a feature list, but it is what lets the engine remain autonomous.

## What 0.1.0 deliberately does not do

Hyphae is not SQL, a distributed database, a hosted SaaS, or a cognitive runtime. It does not currently ship joins, replication, clustering, built-in TLS, encryption at rest, access-control roles, multitenancy, an embedding model, or an LLM.

The optional HTTP server binds to loopback by default. Applications own remote TLS termination, process supervision, filesystem permissions, and backup-media policy.

Those are product boundaries, not omissions hidden behind a roadmap. The complete current capability matrix and limits are documented in the repository.

## What ships in the first release

Hyphae `0.1.0` includes:

- the `hyphae` binary for local operations, server, remote client, verifier, and MCP;
- ten published Rust crates, including the embeddable `hyphae-engine` facade;
- dependency-free TypeScript and Python HTTP clients;
- canonical OpenAPI and JSON Schema contracts;
- executable examples and a cross-client conformance suite;
- Linux, macOS, and Windows archives with checksums, SBOMs, signatures, and provenance;
- documented disk formats, recovery behavior, threat models, ADRs, and release gates.

At the time of the `0.1.0` publication described here, the source was licensed
under Apache-2.0. The current repository is separate from historical research
prototypes and the Hyphae Network project; historical sources remain frozen
inputs, and only audited pieces with documented provenance may cross that
boundary. Later repository versions may use the licensing policy current at
their own release.

## Where I would value feedback

This is a first public release, and I am especially interested in three questions:

1. Where would a proof-bearing local query result materially simplify your system?
2. Is a complete snapshot witness the right 0.1 tradeoff, or would your workload require authenticated indexes or smaller multiproofs?
3. Which embedded Rust integration should become the next executable example?

You can explore the project here:

- [Hyphae website](https://hyphae.dev/)
- [Interactive playground](https://hyphae.dev/playground/)
- [Source and technical documentation](https://github.com/celiumsai/hyphae)
- [Hyphae 0.1.0 release](https://github.com/celiumsai/hyphae/releases/tag/v0.1.0)
- [`hyphae-cli` 0.1.0 package documentation](https://docs.rs/crate/hyphae-cli/0.1.0)

If durable local data and independently checkable results belong in the same engine, I would like to hear what you would build with it.
