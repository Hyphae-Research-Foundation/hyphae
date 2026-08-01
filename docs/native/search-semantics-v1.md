# Native search-engine semantics v1

Status: normative target contract; deterministic tokenization, BM25, native
B+tree collection/document/term/posting namespaces, direct physical `MATCH`,
legacy inline-state compatibility, and multilevel recovery evidence are
implemented experimentally; positions, segments, phrases, facets, and broad
scale/quality evidence remain pending

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

The first physical implementation uses marker `HYSEABT1` in one immutable
copy-on-write native B+tree. It stores:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact format key | ASCII `HYSEABT1` |
| `0x01` | collection `ObjectId` | `HYIDX001` document count and total analyzed terms |
| `0x02` | collection `ObjectId` + document ID | `HYDOCS01` token count and inline/blob source text |
| `0x03` | collection `ObjectId` + canonical UTF-8 term | `HYTERM01` document frequency |
| `0x04` | collection `ObjectId` + u32 term length + term + document ID | `HYPOST01` term frequency |

The fixed 128-bit object ID is big-endian in every key. The posting term
length is big-endian so a prefix scan identifies exactly one term even when
terms share byte prefixes. Terms and composite document identities must fit
the native 4,096-byte key limit. Oversized identities are rejected before a
logical mutation is staged.

`INDEX DOCUMENT` is immutable in this slice. It analyzes source text once,
stores per-document length, increments exact collection/term statistics, and
inserts one posting per distinct term. Text above 8,192 bytes uses the common
content-addressed blob store. The format does not yet store positions,
offsets, field norms, tombstones, generations, or immutable merge segments.

Complete-state validation rebuilds terms, document frequencies, term
frequencies, document count, and total length from stored source text and
requires byte-for-byte equality with the physical metadata and postings.
Orphan documents/postings, noncanonical terms, count divergence, invalid
UTF-8, bad envelopes, and missing/corrupt blobs fail closed.

## Query operators

V1 target operators are exact term, match, boolean, phrase, range, prefix,
fuzzy, wildcard, exists, stable-ID filter, lexical top-k, facet, metric
aggregation, highlight, vector search and hybrid fusion.

The implemented vertical slice is one analyzer, one text field, `MATCH`, top-k
and a stable-ID tie-breaker. Phrase, boolean, range, prefix, fuzzy, wildcard,
facets, highlighting, doc values, and hybrid operators remain target work.

## Lexical scoring

V1 default ranking is versioned BM25F:

- field weights and `k1`/`b` are index-definition values;
- document lengths and corpus statistics bind to the snapshot/index generation;
- filter context contributes no score;
- score ordering is descending, followed by stable object ID ascending;
- explanations name every term, field statistic, parameter and contribution.

The current scorer uses BM25 with `k1=1.2`, `b=0.75`, query-term
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
historical snapshots, optimistic disjoint-document rebase, restart,
single-page legacy compatibility, large-text blob reuse, key bounds, metadata
corruption, and all-engine crash recovery. Required remaining evidence
includes analyzer/token/position goldens, BM25F score/explanation fixtures,
query-operator properties, delete/update visibility, rebuild equivalence,
merge interruption, facet/aggregation correctness, bounded cancellation,
quality metrics, and cross-engine stable-ID joins.
