<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native lexical tombstone compaction v1

Status: implemented at `fea5925dc37bb1b8c9dbecaca4bc407e5199d3b2`;
direct-Linux implementation, recovery, and scaling gates pass. Hosted
cross-platform gates remain release evidence rather than local evidence.

This contract adds explicit current-root reachability compaction to Hyphae's
native lexical/search B+tree. It removes no logical document, rewrites no
historical root, and does not compact through OpenSearch, another database, or
a serialized engine-to-engine path.

## Operation

`compact_search(durability)` captures one complete current root set and
examines the current search root. The first slice is an embedded maintenance
operation. It has no local-protocol opcode.

The operation supports native inverted B+tree search roots:

- `HYSEABT1` contains no legal tombstones and is therefore a validated no-op;
- `HYSEABT2` may be rebuilt when at least one canonical lexical tombstone is
  reachable; and
- legacy page-kind-10 inline `SearchState` roots are unsupported and return a
  typed error without WAL, page, blob, manifest, or CSN side effects.

Before appending a replacement page, planning:

1. validates the complete B+tree shape and visible creating CSNs;
2. validates the complete live lexical projection from stored source text;
3. validates every canonical V2 tombstone;
4. loads the captured catalog and validates the complete ANN generations
   sharing the search root;
5. scans every current physical search entry in canonical key order; and
6. classifies every entry as retained live/metadata/ANN state or one exact
   compactable lexical tombstone.

This complete scan is deliberate maintenance work. It is not permitted inside
document mutation, `MATCH`, vector query, or another latency-sensitive
operation.

## Compactable entries

Only these exact V2 key/value combinations are compactable:

| Prefix | Namespace | Exact value |
|---:|---|---|
| `0x02` | stored document | ASCII `HYDOCT01` |
| `0x03` | term metadata | ASCII `HYTERMT1` |
| `0x04` | posting | ASCII `HYPOSTT1` |

The planner retains byte-for-byte:

- the exact `HYSEABT2` format marker;
- collection metadata;
- every live `HYDOCS01`, `HYTERM01`, and `HYPOST01` value;
- ANN generation metadata, vectors, and graph neighbors under prefixes
  `0x05` through `0x07`; and
- every future entry only after a later format contract explicitly classifies
  it.

V1 tombstones, unknown prefixes, malformed keys, extended tombstone values,
tombstones in another namespace, orphan live postings, count divergence,
invalid UTF-8, corrupt/missing blobs, and invalid ANN generations fail before
the replacement tree is built. The planner never guesses that an unknown
value is dead.

## No-op behavior

If the validated root contains no compactable tombstone, the operation returns
a receipt with `commit=None`. It appends no page, writes no WAL record, changes
no conflict state, advances no transaction identity, and advances no global
CSN.

An empty search slot is a no-op only for a physically empty pre-genesis
database. Once a committed root set exists, a missing search root is invalid
committed authority rather than an implicit empty tree.

## WAL and publication

When work exists, the additive maintenance opcode is:

| Opcode | Name | Engine | Target | Key | Value | Expiry |
|---:|---|---|---|---|---|---|
| `39` | `COMPACT SEARCH` | search | absent | empty | empty | absent |

`COMPACT SEARCH=39` is physical rewrite authority, not a logical document
mutation. Recovery must never infer a delete set from the empty body. It
revalidates the admitted prior root and deterministically rebuilds the
replacement from that root.

The operation:

- begins writer admission only after the complete plan exists;
- requires the captured four-root set to remain current at admission;
- fails for retry if any engine root changed, instead of compacting stale
  authority;
- builds one fresh balanced B+tree through the existing ordered batch
  primitive;
- preserves every retained key/value byte exactly;
- publishes only the replacement search root under one new global CSN;
- preserves catalog, relational, and structure root page IDs exactly; and
- satisfies memory or strict durability through the existing WAL/root
  coordinator.

The maintenance mutation has its own non-user conflict domain. It cannot be
combined with document, vector, catalog, relational, or structure mutations.

## Snapshots and blobs

The pre-compaction root remains immutable and readable. Every historical
snapshot retains the same lexical BM25 results, stored source text, and ANN
generation as before compaction. The new root reconstructs byte-for-byte equal
live lexical and ANN logical states.

Compaction removes tombstone entries only from the current reachable search
tree. It does not:

- shrink the append-only page file;
- delete superseded pages or manifests;
- delete an immutable source blob;
- lower the retention floor; or
- change a pinned historical snapshot.

Page-file reclamation remains the authority of page-generation vacuum.
Immutable source blobs become candidates only after page/WAL/manifest
retention proves that no retained root references them; blob collection
remains a separate explicit operation.

## Receipt

`SearchCompactionReceipt` reports:

- complete physical entries scanned;
- entries retained byte-for-byte;
- canonical tombstones dropped;
- reachable B+tree pages before and after;
- pages appended for the replacement tree; and
- the optional native commit receipt.

For applied work:

`scanned_entries = retained_entries + dropped_tombstones`.

For a no-op:

- `dropped_tombstones = 0`;
- page counts before and after are equal;
- `pages_appended = 0`; and
- `commit = None`.

The receipt does not claim physical bytes reclaimed. Benchmark evidence reports
planning, rebuild/commit, page work, and durability separately.

## Recovery and concurrency

All seven native commit interruption boundaries must reopen either:

- the complete prior search root with all admitted tombstones; or
- the complete compacted root with none of those tombstones.

No boundary may expose a partially rebuilt tree, lose a live document/posting,
select a different ANN generation, or publish a mixed all-engine root set.

A detached document/vector writer or another maintenance attempt prepared
against the prior root cannot publish through the replacement root without
ordinary revalidation. A writer admitted before compaction changes the
captured root set and makes compaction retry. A writer admitted after
compaction resolves against the replacement root.

## Verification gates

The slice requires:

- a compiler-reaching red test before `SearchCompactionReceipt`,
  `compact_search`, WAL opcode `39`, and the physical batch mode exist;
- stable opcode bytes plus engine/target/key/value/expiry rejection;
- V1 and V2 no-op proofs with no transaction-ID/CSN/page/WAL movement;
- exact document, term, and posting tombstone counts across mixed
  replace/delete/reinsert histories;
- live lexical source/statistic/posting bytes preserved exactly;
- BM25 result and score equality before/after compaction, after reopen, and
  across a retained historical snapshot;
- ANN generation identity, exact/approximate results, vectors, and graph bytes
  preserved exactly;
- malformed V1/V2, unknown-prefix, orphan, count, source-blob, and ANN
  corruption rejection before page append;
- writer/compactor and compactor/compactor conflict/retry behavior;
- all seven deterministic commit interruption boundaries;
- page vacuum and blob-collection sequencing without historical resurrection;
- hosted Linux stable/MSRV, macOS, Windows, fuzz, dependency, packaging,
  release-assembly, integration, and soak gates; and
- direct-Linux tombstone-ratio and population scaling with planning,
  memory/strict commit, page/WAL, and whole-process allocation observations.

## Boundary

This slice does not add automatic compaction policy, background workers,
segment merging, bulk APIs, positions, phrases, filters, facets, highlights,
doc values, analyzer generations, cross-engine SQL operators, replication,
clustering, multitenancy, TLS, encryption at rest, SaaS roles/billing,
embeddings, or an LLM.
