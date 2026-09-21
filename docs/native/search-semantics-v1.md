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
The M05 authenticated ANN overlay has bounded point-upsert and point-delete
publication, conflict-only complete-image absence fencing, and bounded
foreground consolidation; it adds no new public search capability or release
claim.

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
| `0x04` | collection `ObjectId` + u32 term length + term + document ID | live `HYPOST01`/`HYPOST02` or v2 `HYPOSTT1` tombstone |
| `0x05` | vector-index `ObjectId` | readable `HYANNM01` through `HYANNM05`; base creation emits M04 and a later physical point mutation emits M05 |
| `0x06` | vector-index `ObjectId` + 32-byte build identity + object `ObjectId` | `HYANNV01` creating CSN and canonical `f32` vector |
| `0x07` | vector-index `ObjectId` + 32-byte build identity + object `ObjectId` + u16 layer | `HYANNG01` stable neighbor IDs |
| `0x08` | vector-index `ObjectId` + object `ObjectId` | current `HYANND01` vector upsert or tombstone |
| `0x09` | vector-index `ObjectId` | fixed `HYANNO01` overlay manifest |
| `0x0a` | vector-index `ObjectId` + object `ObjectId` | `HYANND02` overlay upsert or tombstone |
| `0x0b` | vector-index `ObjectId` + depth + 128-bit high-nibble path | `HYANNN01` sparse-Merkle internal node |

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
base and M04 metadata. A later upsert or deletion of a present vector freezes
`0x08` and updates one bounded authenticated `0x0a` path plus M05 metadata
without rebuilding or repersisting the base graph. Delete writes a D02
tombstone that suppresses the same object in D01 and the base.
Exact query ranks the effective base-plus-delta set. Approximate query merges
base graph candidates with exact live-delta candidates and suppresses every
shadowed base object. Before suppression, its base-hit target is
`min(base_count, ef_search, k + shadowed_base_count)`, where
`shadowed_base_count` counts selected-base objects named by the effective delta.
It does not first truncate the base to `k`. Graph search and reranking never
exceed the caller's `ef_search`, even when the index permits a larger configured
maximum. If that caller ceiling cannot supply enough unshadowed base hits and
live delta upserts, the result may contain fewer than `k` hits. Underfill does
not trigger a complete-corpus exact fallback or silently widen `ef_search`.
The selected base count, 4,096-record delta ceiling, and query candidate ceiling
remain hard bounds. `HYSEABT1`/`2` and `HYANNM01` remain readable.

The M05 reader composes `0x0a` first, frozen `0x08` second, and the base
last. An overlay tombstone suppresses both lower layers. It validates exact
layer counts/bytes/sequences, one manifest, all ordered radix-16 Merkle paths,
the root, and the final view identity before either exact or ANN hydration.
XOR, additive, and other commutative accumulators are explicitly rejected.
The exact byte contract is [ANN delta overlay format
v1](../storage/ann-delta-overlay-format-v1.md). Foreground physical point
upsert and delete emit those records with fixed `HYANNA02` opcode 56 authority.
Historical unmarked `DeleteVector` opcode 19 retains its pre-M05 meaning;
physical M05 delete is opcode 19 plus `HYANNA02`; `FenceVectorAbsence` opcode
57 proves prior absence and emits no ANN physical record. Opcode 50 with
`HYANNC02` publishes bounded M05 consolidation. M05 initial-bulk rewriting
remains fail closed and background scheduling remains outside this slice;
lexical compaction and page-generation vacuum may preserve authenticated M05
bytes unchanged.

### Complete-image vector omission

Integrated document ingest, replacement, and deletion are complete-image
operations over every catalog-bound named-vector target:

- insert or replacement upserts every supplied named vector;
- an omitted target deletes it when the transaction-private view is live, or
  emits opcode 57 when that view is already absent; and
- document deletion applies the same delete-or-fence rule to every bound target,
  regardless of which vectors the prior document image supplied.

The absence branch proves the admitted object absent in D02-first, D01-second,
base-last order, including the authenticated D02 path. It contributes no D02
leaf, Merkle node, manifest, metadata, view-identity, count, byte, sequence, or
delta-slot change. It does contribute one WAL mutation, the object conflict
key, and the index lifecycle-fence key. Therefore a stale same-object upsert
cannot survive an omitted or deleted complete image in either commit order;
disjoint object operations may rebase, while initial-bulk and consolidation
replacement race through the lifecycle fence. At most 4,096 opcode 57 records
are admitted per transaction, independently of physical `HYANNA02` operation
counts.

A transaction containing only absence fences names unchanged root page IDs,
blob generation, and catalog version, but its transaction ID, WAL commit, CSN,
and global visible authority advance normally. Recovery must re-prove each
absence from the prior committed root, require zero target physical changes,
and rebuild the same object and lifecycle conflict keys.

For M04 or M05 input, bounded ANN consolidation captures an effective set,
source-version flag, selected base/view identities, captured `next_sequence`,
and the complete object-ordered effective delta. It constructs a replacement
base and publishes it through an ordinary root commit using append-only WAL
opcode 50 and fixed 384-byte `HYANNC02` authority. The capture digest includes
the index plus each effective record's object, sequence, kind, mutation CSN, and
canonical vector components. The canonical effective-vector-set digest also
hashes the index before the effective count and record accumulators; the fixed
body separately duplicates that same target. No per-record tail
follows the fixed body. C02 permits captured format M04 or M05 and result format
M05 only. Historical fixed 112-byte
`HYANNC01` remains authoritative for retained M01-through-M04 consolidation
recovery and is never emitted for this path.

M01 through M03 retain the historical writable ordinary-vector path. Its first
accepted mutation atomically upgrades the target metadata to M04; reads and
rejected mutations do not rewrite it. A retained C01 consolidation can likewise
authorize its original M01-M03-to-M04 upgrade. Neither compatibility path
creates M05, and M01-M03 are never encoded in a C02 format byte.

At publication, physical D01 and D02 records below captured `next_sequence`
must reconstruct the captured object/sequence map, count, digest, and view
exactly. A current effective record at its captured sequence is consumed; a
greater-sequence record is preserved, and a new object is later only at or
above the boundary. A later D02 may shadow a captured D01 because the D01 still
proves the capture. Overwriting a captured D02, or overwriting a captured D01
in D01, destroys that proof and is stale. A stale base, changed capture
kind/CSN/vector, missing capture record, lower later sequence, or unclassified
record rejects before C02 is encoded or any page is appended.

The result is canonical overlay-only M05: D01 is empty, the replacement base is
the frozen legacy identity, and every surviving later record appears once in
D02 with its sequence, mutation CSN, kind, and vector unchanged. Consolidation
preserves publication-time `next_sequence`; its manifest, ordered sparse-Merkle
tree, and view identity are regenerated exactly. The current root preserves the
ordered retained-generation list, appends the superseded selected nonempty base
at the end only when it differs from the replacement and is not already
retained, and drops only the oldest generations beyond policy. Every surviving
selected or retained child has its complete vector and graph records; surviving
prior generation bytes are
unchanged and only retired generations disappear. Strict crash cuts through
`PageSynchronized` recover the prior root; cuts from `WalAppended` recover only
the complete replacement after the same capture, overlay, sequence, exact
retention transition, graph/generation, and effective-result proof. Recovery
performs that proof against each immediately preceding retained root even when
the result was later superseded. Snapshot pins retain old page-file roots, and
unpin plus page vacuum/collection reclaims them safely. Initial-bulk rewriting
of M05 and background scheduling remain non-claims.

Complete-state validation rebuilds terms, document frequencies, term
frequencies, document count, and total length from stored source text and
requires byte-for-byte equality with the physical metadata and postings.
Orphan documents/postings, noncanonical terms, count divergence, invalid
UTF-8, bad envelopes, and missing/corrupt blobs fail closed.

Lexical complete-state materialization owns only one borrowed B+tree range
visit over `[0x00,0x05)`. It never visits or copies ANN metadata, vectors,
graphs, deltas, manifests, or Merkle nodes. The visit admits at most 131,072
lexical entries and 64 MiB of encoded lexical key/value bytes. Before any live
entry is copied or any document blob is read, retained accounting charges 512
bytes plus four copies of the physical key length and the logical document
bytes, when applicable; the aggregate is capped at the 64 MiB recovery
authority. A separate zero-entry borrowed range over `[0x0c,+inf)` preserves
unknown-prefix corruption authority. Prefixes `0x05` through `0x0b` belong
exclusively to ANN validation.

When an M05 root is loaded as complete product authority, measured lexical
retention is subtracted from the shared 64 MiB recovery allowance before ANN
metadata admission. Search and ANN cannot each consume an independent 64 MiB
allowance. Oversized or malformed ANN values do not allocate through lexical
materialization. Point, initial-bulk, and consolidation publication substitute
the target index's exact candidate metadata charge into that same shared
authority and reject overflow before page creation. Group point members carry
that candidate state and charge forward in accepted commit order.

Roots containing only M01 through M04 are grandfathered onto their historical
bounded streaming load and may open, pin, back up, restore, and recover even
when their aggregate hydration estimate exceeds that M05 shared allowance. An
M04 initial-bulk publication remains M04 under the same rule. The first point
publication or C02 consolidation that would introduce M05 projects the complete
lexical-plus-ANN candidate, activates the shared cap, and rejects without page
or WAL growth if the candidate does not fit.

At the product boundary, `AnnDeltaLimitExceeded` and direct or queued governor
`ParentCapacity` rejection map to `ProductErrorCode::LimitExceeded`, category
`Limit`, with retry `Never`. Direct or queued global/class-capacity rejection,
queue-full, and queue-timeout map to `ProductErrorCode::Unavailable`, category
`Unavailable`, with retry `AfterBackoff`; they are not collapsed into an
internal failure or an untyped generic search error.

## Query operators

V1 target operators are exact term, match, boolean, phrase, range, prefix,
fuzzy, wildcard, exists, stable-ID filter, lexical top-k, facet, metric
aggregation, highlight, vector search and hybrid fusion.

The implemented vertical slice is one analyzer, one text field, `MATCH`,
exact vector ranking, approximate HNSW top-k with optional exact reranking,
and stable-ID tie-breakers. Filtered ANN uses exact object-point evaluation when
the complete caller allowlist contains at most `ef_search` identifiers. Larger
allowlists use bounded filtered graph traversal; partitioned indexes route that
work through bounded child graphs without scanning the complete base to lower
the caller's allowlist cardinality. Either graph shape may honestly underfill
without exact completion. Receipts name the snapshot CSN, build identity,
metric, breadth, truthful strategy/risk, candidate counts, reranking flag and
visited nodes.
Bounded boolean, phrase, prefix and fuzzy execution, stable-ID vector filters,
typed doc-value filters/sort, terms facets, metric aggregations and native RRF
hybrid execution are implemented as embedded G4 surfaces. The integrated
surface additionally accepts a per-request fusion selector: the default is
deterministic weighted reciprocal-rank fusion (`k = 60`), and
`weighted_score` blends each branch's weight with its normalized score — a
lexical candidate contributes `weight × score / branch_top_score` and a
vector candidate contributes `weight × 1 / (1 + distance)`.
`relative_score` min-max normalizes each branch over its admitted
candidates before weighting: a lexical candidate contributes
`weight × (score − branch_min) / (branch_max − branch_min)` and a vector
candidate contributes `weight × (branch_max_distance − distance) /
(branch_max_distance − branch_min_distance)`, so the best admitted
candidate of every branch contributes exactly its weight and the worst
contributes zero regardless of the branch's score scale. A branch whose
admitted candidates all share one score contributes the full weight for
each. Candidates excluded by the eligibility filter never participate in
a branch's normalization bounds. An optional
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
deterministically; it never runs a model.

Integrated vector execution derives one complete stable-ID allowlist from the
collection manifest and request filter and reuses it for every vector branch.
Thus `MatchAll` excludes a foreign vector stored in the same physical ANN index,
and a filtered branch admits only the filtered collection members. ANN and
adaptive-ANN branches call native filter-aware execution directly; the product
layer adds no exact one-hit seed and reports the native candidate count, visited
nodes, exact-rerank flag, approximation status, hits, and bounded underfill
unchanged. A larger filtered graph request may therefore underfill without exact
completion whether its base is single or partitioned.

The lexical branch may declare weighted field boosts: an ordered list of
`(field, weight)` pairs (weights in micros, `1..=1_000_000_000`; at most
64 fields) switches the branch from single-field BM25 to versioned BM25F
over the bounded committed corpus. The reserved field name `body` scores
the canonical indexed source text; any other name scores the exact
string doc value of that field (a missing or non-string value reads as
the empty field). Per-field statistics (document frequency, field
length, average field length) follow the legacy-equivalent BM25F
reference with fixed `k1 = 1.2`, `b = 0.75`, nano-quantized scores and
bytewise document-key tie-breaks. The boosted branch participates in
fusion exactly like the ordinary branch (scores map to
`score_nanos / 1e9`). Field boosts are mutually exclusive with the term
operator and prefix expansion; duplicate field names, unknown non-`body`
names, zero weights, or an empty analyzed query fail closed.

The lexical branch may declare phrase matching: candidates score
ordinary BM25 over the analyzed query terms, then admission requires
the exact consecutive analyzed-position sequence to occur in the
candidate's canonical indexed text (positions from the canonical
analyzer, including gaps left by discarded oversized tokens — a
discarded token therefore breaks adjacency, deterministically).
Verification re-analyzes only the BM25 candidates, never the whole
corpus. A query with fewer than two analyzed terms is an ordinary
match. Phrase matching is pairwise mutually exclusive with the term
operator, prefix expansion, fuzzy expansion, and field boosts.

The lexical branch may declare fuzzy expansion: each analyzed query
term expands to every distinct indexed term within the declared
Levenshtein character-edit distance (`1..=2`), then the branch scores
ordinary BM25 over the union of expanded terms. Expansion walks the
same bounded committed vocabulary as prefix expansion — the durable
live term dictionary of the index (namespace `0x03`), never the
document texts — and admits at
most 64 distinct expanded terms across the whole query
(`MAX_LEXICAL_PREFIX_TERMS`); overflow fails closed as limit-exceeded.
Terms whose expansion is empty contribute nothing. Fuzzy expansion,
prefix expansion, field boosts, and the term operator are pairwise
mutually exclusive in one request.

The lexical branch may declare prefix expansion: the final analyzed
query term is treated as a prefix and expands to every distinct indexed
term starting with it, then the branch scores ordinary BM25 over the
expanded OR of terms (earlier query terms stay exact). Expansion scans
the durable live term dictionary of the index (namespace `0x03`; a
prefix walk bounded by the literal prefix, tombstoned terms skipped)
and admits at most 64 distinct expanded terms
(`MAX_LEXICAL_PREFIX_TERMS`); more distinct matches fail closed as
limit-exceeded rather than answering from a truncated vocabulary. A
prefix with no expansion leaves the branch empty. Prefix expansion and
the term operator are mutually exclusive in one request.

The lexical branch may declare an optional term operator. The default
(absent) keeps pure OR semantics: any analyzed query term admits a
candidate. `And` admits only candidates containing every distinct
analyzed query term; `Or { minimum_match }` admits candidates containing
at least `minimum_match` distinct analyzed terms (`1..=64`; a
`minimum_match` above the distinct-term count admits nothing). BM25
scores are unchanged — the operator filters admission, never scoring.
Membership is verified against complete bounded per-term posting sets:
each distinct term's full match set must fit the branch candidate bound
(10,000), and a term whose set reaches that bound fails closed as
limit-exceeded rather than answering from a truncated set. A query whose
analysis yields no terms leaves the branch empty as before.

A vector branch may declare an optional `max_distance` cutoff: hits at
a canonical metric distance strictly greater than the cutoff are
discarded before fusion, so garbage matches never earn a reciprocal
rank or a normalized score. The cutoff must be a finite nonnegative
canonical float; it never widens a branch (the candidate limit still
applies first) and exact and approximate strategies honor it
identically. Branch receipts keep reporting pre-cutoff candidate
counts, so recall risk stays observable.

An optional nonnegative `offset` skips that many leading hits of the
final ranking before the `limit` window, after every other stage
(fusion, filter, sort, rerank, deduplication, autocut). `offset + limit`
must stay within the bounded ranking ceiling (1,024 hits), so deep
paging cannot silently degrade; facets, aggregations, and counters keep
describing the complete filtered candidate set, not the window.

An optional autocut stage truncates the final score-ordered ranking at
the first steep quality drop instead of a fixed count. With hit scores
`s_0 >= s_1 >= … >= s_{n-1}` mapped to `x_i = i/(n-1)` and
`y_i = (s_i − s_0)/(s_{n-1} − s_0)`, the deviation `d_i = y_i − x_i`
measures how far the score curve sits above uniform linear decay; the
ranking is cut immediately before the `N`-th strict local maximum of
`d` (`N` in `1..=16`). A ranking with one hit, equal extreme scores, or
fewer than `N` such maxima is returned whole. Autocut runs after
reranking and parent deduplication and before the final `limit`; it is
deterministic, needs no score threshold tuning, and composes best with
`relative_score` fusion whose normalized magnitudes it inspects.
Wildcard, persistent multi-field doc-value columns and unrestricted
query language remain non-claims.

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

The durable scorer's cost scales with the live postings of the query terms.
Segment planning reads only each leaf's verified boundary keys and entry
count; posting scans borrow keys and values from the verified buffer-pool
frame and append only the matched document id to a per-segment arena; the
merge sorts, folds, and ranks arena offsets and copies ids only for the
returned `limit` hits. Every borrowed decode performs the same preamble,
count, length-consumption, key-order, and key-size checks as the owned
decode, so a malformed leaf is rejected identically on either path.

## Transactional visibility

The commit coordinator installs the search delta root with the same CSN as
other engine roots. A transaction can read its own indexed changes. The next
transaction observes them without refresh or CDC.

Segment merging and analyzer shadow builds preserve the logical snapshot and
publish a new generation atomically.

## Doc-value types

Doc values are typed scalars: boolean, signed 64-bit integer, canonical
IEEE-754 binary64 float, UTF-8 string, and binary bytes. Floats are
canonicalized on ingest (`NaN` payloads collapse to one canonical `NaN`,
signed zero collapses to `+0`) and order under the deterministic total
order (`-NaN < -inf < … < -0=+0 < … < +inf < +NaN` via canonical-bit
comparison), so filters, sort, `IN`, facets and min/max aggregations
treat them exactly like every other comparable scalar across hosts.
Float doc values bind to `Float32`/`Float64` catalog field types. `Sum`
aggregates integers with checked 128-bit accumulation or floats with
finite-guarded binary64 accumulation; a non-finite float sum fails
closed. `Average` divides the same checked sum by the count of present
values and always yields a canonical float aggregate (absent when no
value is present); a non-finite intermediate fails closed. Comparisons
between different scalar types never match, as before.

Range facets bucket numeric doc values into caller-declared half-open
intervals `[lower, upper)` with independently optional canonical-float
bounds (`NaN` bounds are rejected; an absent bound is unbounded on that
side; `lower < upper` when both are present). Each request admits at
most 8 range facets of at most 64 ranges each. Buckets return exactly
one per declared range in request order — never count-sorted — with the
zero-based range ordinal as an `Integer` bucket value and the count of
filtered candidates whose integer or float value falls inside the
interval (integers convert through deterministic exact-halved binary64
conversion). Missing fields and non-numeric values never count.
Overlapping ranges are legal and count independently. Terms facets are
unchanged and continue to reject non-scalar shapes.

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
strict fixed HYANNC02 shape and ordered effective-record digest, M04/M05
overlay-only consolidation, later-delta and `next_sequence` preservation,
captured-D01 shadow preservation, captured-D02 overwrite rejection,
interruption recovery, complete retained graph/generation cleanup,
grandfathered M01-M04 loading with fail-before-write M05 projection,
collection-manifest allowlisting without product-side exact seeding, and stable
typed resource-error mapping.
