# Native local ecosystem phase-1 gate

Status: in progress; G0 specifications are drafted but implementation and
dependency evidence remain incomplete

This is the ordered implementation gate for the Hyphae-owned relational,
structure, and search ecosystem. It describes future work, not shipped
`0.2.1` behavior.

A later gate may be prototyped early but cannot be declared complete while an
earlier gate is red.

| Gate | Required outcome | Exit evidence |
|---|---|---|
| G0 | Constitution and native specifications | Accepted architecture; versioned type, row, page, blob, WAL, MVCC, SQL, structure, search, ANN, local-protocol and benchmark contracts; clean-room/dependency inventory |
| G1 | Native substrate | Hyphae page/blob store, WAL, catalog, CSN/MVCC, partitioned memory and scheduler; no Redb on the target path; crash injection at every commit/checkpoint boundary |
| G2 | Relational engine | Native DDL/DML, constraints, transactions, indexes, joins, CTEs, windows, prepared plans and `EXPLAIN`; SQLLogicTest, metamorphic tests, isolation litmus, TPC-H correctness and TPC-C ACID evidence |
| G3 | Structure engine | Native strings, counters, hashes, lists, sets, sorted sets, streams, TTL and atomic batches; model-based randomized tests, expiry under controlled time, restart equivalence and memory-amplification evidence |
| G4 | Search engine | Native analyzers, postings, BM25, phrase/prefix/fuzzy, doc values, facets, aggregations, exact vector, ANN and hybrid; scoring goldens, NDCG/recall, rebuild and corruption evidence |
| G5 | Convergence | One transaction mutates all three engines; concurrent readers never observe a mixed CSN; SQL joins and aggregates native structure/search sources through stable IDs and native operators; one checkpoint, backup and restore preserves the complete state |
| G6 | Local product | Embedded API, native binary local protocol, CLI, SDK, administration, `EXPLAIN`, telemetry, doctor, backup and restore share one catalog and error model |
| G7 | Performance | The microsecond contract passes on stable hardware with warm/cold, concurrency, saturation, background interference, p99.9, allocation and hardware-counter receipts |
| G8 | Release evidence | Soak, crash, corruption, resource exhaustion, migration v2-to-native, multiplatform packaging, SBOM, signatures and independent restore verification are green on one exact commit |

## G0 required decisions

G0 must close these decisions before production implementation:

- stable IDs and canonical type semantics;
- page size, row and variable-length layouts, blob references and checksums;
- WAL block, operation, transaction, LSN/CSN and checkpoint formats;
- snapshot isolation conflict rules and the path to serializable execution;
- SQL grammar, null semantics, casts, collation and error model;
- structure ownership, TTL, eviction and volatile-versus-canonical behavior;
- analyzer, postings, doc-values, lexical score and ANN contracts;
- cross-engine transaction, link and view semantics;
- native local framing, authentication boundary and cancellation;
- strict, group and memory durability classes; and
- the reproducible performance and quality corpus.

No disk format, public API, or crate boundary should be frozen before the
relevant G0 contract is reviewable.

The current reviewable drafts are:

- [canonical types](../native/types-v1.md);
- [page, row, and blob format](../native/page-row-blob-format-v1.md);
- [B+tree format](../native/btree-format-v1.md);
- [root manifest and checkpoint format](../native/root-manifest-checkpoint-v1.md);
- [WAL format](../native/wal-format-v1.md);
- [MVCC and commit semantics](../native/mvcc-commit-v1.md);
- [catalog](../native/catalog-v1.md);
- [Hyphae SQL](../native/sql-semantics-v1.md);
- [structures](../native/structures-semantics-v1.md);
- [search](../native/search-semantics-v1.md);
- [ANN](../native/ann-semantics-v1.md);
- [native local protocol](../native/local-protocol-v1.md); and
- [clean-room/dependency policy](../native/dependency-inventory.md).

These drafts do not close G0 until their golden encodings, dependency audit,
benchmark corpus, and implementation-facing tests exist.

## Current experimental evidence

The [2026-08-01 kernel evidence](evidence/native-phase1-kernel-2026-08-01.md)
implements the first reviewable vertical. Steps 1 through 4 and the
reopen-equivalence portion of step 6 below execute in tests. Step 5 now covers
five in-process commit boundaries and four manifest/checkpoint boundaries, but
not blobs, group commit, filesystem reordering, or sector-level power loss.
Step 7 has an embedded and frame-codec smoke only; no named-pipe/UDS transport
receipt.

The follow-on [row, B+tree, and checkpoint
evidence](evidence/native-row-tree-checkpoint-2026-08-01.md) binds canonical
MVCC rows, native copy-on-write relational storage, immutable root manifests,
the four-boundary checkpoint matrix, and a clean physical point-read latency
observation to one source commit.

The [blob, mutation, and conflict
evidence](evidence/native-blobs-mutations-conflicts-2026-08-01.md) adds the
immutable content-addressed blob store, SQL UPDATE/DELETE with copy-on-write
roots and tombstones, a WAL-rebuilt point conflict table, two blob crash
boundaries, and a clean height-two B+tree latency observation.

The [relational version-chain
evidence](evidence/native-relational-version-chains-2026-08-01.md) binds the V2
row pointer, immutable closed histories, V1 compatibility, fail-closed chain
recovery, and the measured cost of the additional version-page lookup to one
source commit.

The [optimistic-writer
evidence](evidence/native-optimistic-writers-2026-08-01.md) binds concurrent
detached preparation, admitted-root rebase across all three engines,
first-committer-wins, lagging-read-CSN recovery, and the optimistic crash
matrix to one source commit.

The [structure B+tree
evidence](evidence/native-structure-btree-2026-08-01.md) binds the first
multilevel scalar keyspace, canonical TTL/blob envelope, legacy compatibility,
cross-engine blob deduplication, direct buffered reads, and a clean latency
observation to one source commit.

The [scalar structure mutation
evidence](evidence/native-scalar-structure-mutations-2026-08-01.md) binds
canonical physical tombstones, `DELETE`, independent `EXPIRE`, `NX`/`XX`,
checked signed counters, exact-boundary TTL absence, WAL expiry-presence
compatibility, same-key conflict admission, and crash recovery to one source
commit.

This evidence advances G0/G1 but closes neither gate. Relational table/primary
key storage now uses the native B+tree and canonical MVCC rows, and large row
values use native immutable blobs. The current implementation also retains
explicit per-key row-version chains with closed half-open intervals while
keeping V1 inline-row directories compatible. Catalog and search state remain
bounded single-page scaffolding. Detached transactions now prepare concurrently
and rebase disjoint all-engine writes under serialized publication;
simultaneous commit submission and concurrency/saturation evidence remain
pending, along with retention/vacuum, secondary indexes, remaining structure
families, postings, segments, and ANN required by G1–G4. The scalar structure
keyspace has a multilevel native B+tree, direct reads, tombstone/expiry
mutations, conditional writes, and signed counters; it still lacks the
collection families, expiry scheduler, model tests, and amplification evidence
required to close G3.

## G1 substrate exit

G1 requires a minimal vertical proof after the substrate exists:

1. create one relation and perform a primary-key insert/read;
2. create one keyspace value with TTL and perform a point read;
3. create one lexical index and perform a match;
4. commit mutations to all three engines under one CSN;
5. interrupt the process at every WAL, page, root-publication and checkpoint
   boundary;
6. recover and prove that readers see either the complete prior CSN or the
   complete committed CSN; and
7. run the first embedded and local-protocol latency receipt.

This slice proves architecture only. It does not close G2, G3, or G4.

## Phase-1 stop rule

Clustering, replication, multitenancy, control planes, SaaS billing, hosted
operations, embedding generation, HiveMind, and any LLM are outside this
phase. They cannot consume implementation capacity or weaken a phase-1 gate.

Phase 2 begins only after G8 has exact release evidence. Its architecture is
not declared by this document.
