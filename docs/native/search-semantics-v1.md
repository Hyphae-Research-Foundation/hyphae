<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native search-engine semantics v1

Status: normative bounded G6 contract; canonical tokenization, BM25, native
B+tree collection/document/term/posting namespaces, direct physical `MATCH`,
catalogued vector/HNSW generations, exact/approximate vector query, lexical
document replacement/deletion, bounded boolean/phrase/prefix/fuzzy execution,
typed doc values, filters, sort, facets, aggregations, native hybrid fusion,
legacy inline-state compatibility, rebuild, corruption, and bounded quality
evidence are implemented. Automatic segments, page-buffered ANN and
production-scale performance remain non-claims.

The search engine owns documents, lexical indexes, doc values, aggregations,
and transactional search visibility. It is not an OpenSearch REST facade.

## Collections and documents

A search collection declares a stable `ObjectId`, source ownership, fields,
stored/source policy, analyzers, doc-values policy and optional vector indexes.
A document has a stable object ID and MVCC version.

Documents may be search-owned or linked explicitly to another engine object.
A linked document update participates in the originating transaction when the
link is synchronous.

## Analyzer pipeline

An analyzer definition pins:

1. UTF-8 validation;
2. Unicode normalization form;
3. tokenizer and version;
4. optional case folding;
5. stop-word set digest;
6. stemmer or token filters with versions; and
7. position and offset emission rules.

Changing any component creates a new index generation. Source text remains
unchanged.

## Inverted index

- Term dictionaries use a versioned finite-state or prefix-searchable format.
- Postings include document/object ID, term frequency, positions and optional
  offsets.
- Doc values provide typed columnar sort, filter, facet and aggregation input.
- Immutable segments are ordered by generation and creating CSN.
- Tombstones and field updates are versioned.
- A mutable transactional delta is searchable at commit; background merges do
  not define visibility.

The physical implementation uses marker `HYSEABT1`, `HYSEABT2`, or `HYSEABT3`
in one immutable copy-on-write native B+tree. It stores:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact format key | ASCII `HYSEABT1`, `HYSEABT2`, or `HYSEABT3` |
| `0x01` | collection `ObjectId` | `HYIDX001` document count and total analyzed terms |
| `0x02` | collection `ObjectId` + document ID | live `HYDOCS01` or v2 `HYDOCT01` tombstone |
| `0x03` | collection `ObjectId` + canonical UTF-8 term | live `HYTERM01` or v2 `HYTERMT1` tombstone |
| `0x04` | collection `ObjectId` + u32 term length + term + document ID | live `HYPOST01` or v2 `HYPOSTT1` tombstone |
| `0x05` | vector-index `ObjectId` | legacy `HYANNM01` or current `HYANNM02` selected base-plus-delta metadata |
| `0x06` | vector-index `ObjectId` + 32-byte build identity + object `ObjectId` | `HYANNV01` creating CSN and canonical `f32` vector |
| `0x07` | vector-index `ObjectId` + 32-byte build identity + object `ObjectId` + u16 layer | `HYANNG01` stable neighbor IDs |
| `0x08` | vector-index `ObjectId` + object `ObjectId` | current `HYANND01` vector upsert or tombstone |

The fixed 128-bit object ID is big-endian in every key. The posting term
length is big-endian so a prefix scan identifies exactly one term even when
terms share byte prefixes. Terms and composite document identities must fit
the native 4,096-byte key limit. Oversized identities are rejected before a
logical mutation is staged.

`INDEX DOCUMENT` creates one document. `REPLACE DOCUMENT` and `DELETE DOCUMENT`
require one exact live identity and atomically maintain stored source,
collection statistics, term metadata, and postings. The first accepted
lifecycle mutation upgrades the current root to `HYSEABT2`; historical v1
roots remain readable and v1 rejects every tombstone. Insertion may revive
canonical v2 tombstones without overwriting a live identity. Text above 8,192
bytes uses the common content-addressed blob store. Exact lifecycle semantics
and tombstone encodings are fixed by
[Native lexical document lifecycle v1](search-document-lifecycle-v1.md). The
format does not store positions, offsets, field norms, generations, or
immutable merge segments. The bounded G4 phrase/prefix/fuzzy executor derives
canonical positions from stored source under explicit work budgets; it does
not claim a production positional-posting layout.

`compact_search` validates the complete current lexical and ANN projection,
then rebuilds `HYSEABT2` without exact `HYDOCT01`, `HYTERMT1`, or `HYPOSTT1`
tombstones. Every retained lexical and ANN key/value is copied byte-for-byte;
historical roots remain immutable, and a root without tombstones advances no
page, WAL identity, transaction ID, or CSN. Page vacuum and blob collection
remain separate retention operations. The exact maintenance contract is
[Native lexical tombstone compaction
v1](search-tombstone-compaction-v1.md).

`CREATE ANN INDEX`, `UPSERT VECTOR`, and `DELETE VECTOR` use the same search
root and global transaction. Creation produces the initial canonical HNSW
base. Later vector writes update the bounded object-keyed `0x08` delta and
`HYANNM03` view metadata without rebuilding or repersisting the base graph.
Exact query ranks the effective base-plus-delta set. Approximate query merges
base graph candidates with exact live-delta candidates and suppresses every
shadowed base object. `HYSEABT1`/`2` and `HYANNM01` remain readable.

Bounded ANN consolidation captures an effective set, constructs a replacement
base and publishes it through an ordinary root commit using append-only WAL
opcode 50. A stale base rejects publication. Captured delta versions are
consumed only if unchanged, so later object versions survive. The current root
retains the configured number of superseded target generations; snapshot pins
retain old page-file roots, and unpin plus page vacuum/collection reclaims them
safely.

Complete-state validation rebuilds terms, document frequencies, term
frequencies, document count, and total length from stored source text and
requires byte-for-byte equality with the physical metadata and postings.
Orphan documents/postings, noncanonical terms, count divergence, invalid
UTF-8, bad envelopes, and missing/corrupt blobs fail closed.

## Query operators

V1 target operators are exact term, match, boolean, phrase, range, prefix,
fuzzy, wildcard, exists, stable-ID filter, lexical top-k, facet, metric
aggregation, highlight, vector search and hybrid fusion.

The implemented vertical slice is one analyzer, one text field, `MATCH`,
exact vector ranking, approximate HNSW top-k with optional exact reranking,
and stable-ID tie-breakers. Filtered ANN separates connector navigation from
candidate eligibility and adaptively executes exact scoring for restrictive
sets. Receipts name the snapshot CSN, build identity, metric, breadth, truthful
strategy/risk, candidate counts, reranking flag and visited nodes.
Bounded boolean, phrase, prefix and fuzzy execution, stable-ID vector filters,
typed doc-value filters/sort, terms facets, metric aggregations and native RRF
hybrid execution are implemented as embedded G4 surfaces. The integrated
surface additionally accepts a per-request fusion selector: the default is
deterministic weighted reciprocal-rank fusion (`k = 60`), and
`weighted_score` blends each branch's weight with its normalized score — a
lexical candidate contributes `weight × score / branch_top_score` and a
vector candidate contributes `weight × 1 / (1 + distance)`. An optional
first-k-per-parent deduplication runs over the complete bounded ranking
before the final limit: hits group by the exact typed value of one
doc-value field, at most `k` (1..=100) survive per group in rank order,
and hits missing the field are never deduplicated. An optional attested
rerank stage reorders the complete bounded ranking before deduplication and
the final limit: externally computed scores — from the attested local tool
or a declared provider, always accompanied by their canonical attestation
envelope — sort their hits by score descending with stable-identity ties,
unscored hits follow in their existing order, and the whole stage
(envelope included) is bound into sealed proofs. The engine reorders
deterministically; it never runs a model. Wildcard,
highlighting, persistent multi-field doc-value columns and unrestricted query
language remain non-claims.

## Lexical scoring

V1 default ranking is versioned BM25F:

- field weights and `k1`/`b` are index-definition values;
- document lengths and corpus statistics bind to the snapshot/index generation;
- filter context contributes no score;
- score ordering is descending, followed by stable object ID ascending;
- explanations name every term, field statistic, parameter and contribution.

The catalog analyzer types are real for integrated collections: the
configurable pipeline runs as a deterministic text-to-text transform at the
product boundary, at ingest and at query, in front of the canonical analyzer
(NFKC, Unicode case fold, alphanumeric tokenization). `UnicodeWord` with
exactly the `Lowercase` filter — or no analyzer — is the identity and keeps
existing collections byte-identical. Frozen version-one stages compose in
ascending filter order: Latin diacritic folding over an explicit table,
English stop-word removal (the classic 33-word list), and English Porter
stemming. Shapes the transform cannot honor exactly — non-word tokenizers on
lexical fields, pipelines without `Lowercase`, out-of-order filters — fail
closed at ingest and query. Recovery replays the transformed text through
the canonical analyzer and lands on identical postings.

The current scorer uses BM25 with per-collection `k1`/`b` taken from the
index definition (defaults `k1=1.2`, `b=0.75`; micro-unit integers in the
catalog so identical definitions score identically on every host), query-term
deduplication, descending score, then bytewise document-ID ascending. The
materialized reference scorer remains the oracle: tests require physical
posting traversal to return exactly the same scores and order. A dedicated
quality/golden corpus remains pending.

## Transactional visibility

The commit coordinator installs the search delta root with the same CSN as
other engine roots. A transaction can read its own indexed changes. The next
transaction observes them without refresh or CDC.

Segment merging and analyzer shadow builds preserve the logical snapshot and
publish a new generation atomically.

## Aggregations and memory

Aggregations operate on doc values or bounded typed field data. Every query
declares or inherits candidate, bucket, memory, CPU and deadline limits.
Partial results are returned only under an explicitly requested partial mode
and are labelled with the skipped/error state.

## Verification

Current tests cover reference/physical BM25 equivalence, multilevel postings,
historical lexical and vector snapshots, lexical replace/delete/reinsert
visibility, exact v1-to-v2 tombstone upgrade, optimistic disjoint
document/vector rebase, vector batch atomicity, restart, single-page legacy
compatibility, large-text blob reuse, key bounds, lexical/ANN metadata
corruption, canonical graph restore, and all-engine crash recovery. G4 evidence
also covers analyzer/token/position goldens, bounded query-operator properties,
filtered ANN strategy receipts, facet/aggregation equivalence, NDCG/recall,
rebuild and structured corruption matrices. Buffered ANN traversal, automatic
background merge policy, cross-engine SQL joins and production-scale
performance remain G7 work. G6 ANN evidence additionally covers foreground
base-identity stability, effective exact equivalence, reopen, hard delta bounds,
strict maintenance WAL shape, bounded consolidation, later-delta preservation,
stale plans, interruption recovery and current-root generation cleanup.
