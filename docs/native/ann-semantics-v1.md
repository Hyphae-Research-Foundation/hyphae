<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native ANN semantics v1

Status: normative bounded G6 contract; deterministic HNSW, canonical `f32`
admission, three metrics, exact oracle, catalog definitions, native search
B+tree generations, WAL mutations, all-engine MVCC visibility, object-keyed
base-plus-delta lifecycle, bounded consolidation, historical snapshots,
stable-ID eligibility traversal, adaptive exact filtering, durable lifecycle
policy, maintenance due signaling, retained generations, and fail-closed
recovery are implemented. Page-buffered traversal and a background scheduler
remain production non-claims. The authenticated M05 layered-delta format has a
bounded foreground point-upsert and point-delete writer, conflict-only
complete-image absence fencing, and bounded foreground consolidation. M05
initial-bulk rewriting remains a non-claim and fails closed.

ANN is a Hyphae-owned search-engine capability. Exact vector execution remains
the quality oracle.

## Vector admission

- V1 stored ANN vectors are canonical finite `f32`.
- Dimension is fixed by index definition from 1 through 65,535.
- Supported metrics are cosine distance, negative dot-product distance and
  squared L2 distance.
- Cosine vectors cannot be zero.
- NaN, infinity and dimension mismatch are rejected before commit.
- Object ID is the deterministic final tie-breaker.

## Exact oracle

Every index definition has an exact scorer using the same canonical vector and
metric semantics. Quality evaluation compares ANN top-k against the complete
exact ranking on a pinned snapshot.

## HNSW v1 target

The first approximate index is Hyphae-owned HNSW with versioned:

- `M`;
- `ef_construction`;
- default and maximum `ef_search`;
- level multiplier;
- entry-point selection;
- random seed derivation;
- neighbor pruning rule; and
- distance implementation identity.

Canonical base construction orders insertions by creating CSN then object ID
and derives randomness from index ID plus definition digest. Parallel
optimization may not change logical results without a new physical-build
identity.

The first executable kernel is `hyphae-native-ann`. V1 bounds `M` to 2 through
64, requires `ef_construction >= M`, derives each node level from BLAKE3 over
the index-definition digest and object ID, and retains at most `M` directed
neighbors per layer. Neighbor selection uses the deterministic diversity
heuristic (HNSW paper Algorithm 4 without candidate extension or pruned
backfill): candidates ordered by exact metric distance then object ID are
accepted only while strictly closer to the anchor node than to every
already-accepted neighbor, so retained edges spread across directions
instead of clustering; a node may therefore retain fewer than `M`
neighbors. The same rule selects insertion links from the
`ef_construction` frontier and re-prunes overflowing backlinks. The build
identity hashes a version tag (`2` for this rule; `1` was plain
truncate-to-`M`), so graphs persisted under the previous rule fail closed
as corrupt rather than validating against the wrong canonical form.
Foreground update and delete do not rebuild this graph. A point upsert or point
delete publishes one authenticated overlay path. A conflict-only absence fence
authenticates the admitted path but publishes no ANN physical change.

`IndexSnapshot` exports definition, vectors with creating CSNs, graph nodes,
entry point, maximum level and build identity. Restore reconstructs the graph
from the vector records and rejects any snapshot that differs from that
canonical build.

## Implemented durable generation

The native runtime stores ANN in the same copy-on-write search B+tree as
lexical state:

- `0x05 + index ObjectId` selects readable `HYANNM01` through `HYANNM05`
  generation metadata; base creation emits M04 and a later physical point
  mutation emits M05;
- `0x06 + index ObjectId + build identity + object ObjectId` stores one
  `HYANNV01` vector with its creating CSN; and
- `0x07 + index ObjectId + build identity + object ObjectId + u16 layer`
  stores one `HYANNG01` neighbor list; and
- `0x08 + index ObjectId + object ObjectId` stores one current `HYANND01`
  upsert or tombstone with a monotonic per-index sequence and mutation CSN;
- `0x09 + index ObjectId` stores the one fixed `HYANNO01` overlay
  manifest;
- `0x0a + index ObjectId + object ObjectId` stores one `HYANND02`
  overlay upsert or tombstone; and
- `0x0b + index ObjectId + depth + high-nibble path` stores one
  `HYANNN01` fixed-depth radix-16 sparse-Merkle internal node.

Every identity component is big-endian in the key. The 32-byte build identity
is content-bound by the kernel. Vector components remain canonical
little-endian `f32`; graph neighbors are stable 128-bit object IDs.

M02 through M04 name the selected immutable base identity and a view identity
over that base plus the complete current D01 delta, together with base counts,
delta record/byte counts and next sequence. M03 adds durable lifecycle and
retention policy; M04 adds partitioned child and retained-generation
descriptors. `HYANNM01`, `HYANNV01`, and `HYANNG01` remain readable. The v2
envelope includes and validates `HYANNM01` as its predecessor-lineage tag.
M01 through M03 also retain their historical writable compatibility path: the
first accepted ordinary vector mutation reconstructs the legacy target and
atomically emits M04, while reads, rejected mutations, and failed commits leave
its bytes unchanged. Retained C01 consolidation WAL remains authoritative for
its original M01-through-M04 transition, including an M01-M03 upgrade to M04;
new C02 authority cannot name M01, M02, or M03.

A complete root containing only M01 through M04 metadata retains its historical
load, pin, backup, restore, and recovery acceptance even when the aggregate
lexical-plus-ANN hydration estimate exceeds the M05 64 MiB shared authority.
Metadata-derived per-index and physical namespace bounds still apply. The M04
initial-bulk path likewise remains M04 and does not activate the M05 aggregate
cap. A first physical point mutation or C02 consolidation projects the complete
candidate root with M05 present, substitutes the candidate target charge, and
enforces the shared cap before appending a page or WAL byte; rejection leaves
the grandfathered source root unchanged.
`HYSEABT1` and `HYSEABT2` remain readable; the first base-plus-delta mutation
selects `HYSEABT3`.

Index creation, including vectors staged in the same transaction, constructs
the initial canonical M04 base. A later foreground upsert or deletion of a
present vector freezes D01 and publishes one M05 D02 leaf, its 32
sparse-Merkle ancestors, the manifest, and metadata. Deletion publishes a D02
tombstone that suppresses a base, D01, or prior D02 upsert. The base build
identity and its vector/graph records remain unchanged across foreground
mutation commits. Each index durably selects a
`delta_max_entries` no larger than the 4,096-record format ceiling, a
`consolidate_after_deltas` threshold no larger than that capacity, and one to
64 retained generations. Encoded delta data remains capped at 64 MiB. A
mutation exceeding its per-index or byte bound fails before publication.

M05 is the authenticated layered representation defined by [ANN delta
overlay format v1](../storage/ann-delta-overlay-format-v1.md). It freezes the
complete D01 map and its unchanged legacy view identity, then authenticates an
object-keyed D02 overlay with an ordered sparse-Merkle tree. Lookup takes the
overlay first, frozen legacy second, and immutable base last; an overlay
tombstone suppresses both lower layers. Validation requires one manifest,
exact frozen/overlay/effective counts and bytes, legal nonoverlapping sequence
ranges, complete tree reachability and node count, its root, and the final view
identity. XOR and additive/commutative accumulators are invalid substitutes for
these ordered hashes.

### Point deletion and absence fencing

The append-only WAL meanings are distinct:

- a historical unmarked `DeleteVector` opcode 19 retains its M01-through-M04
  physical and generation-conflict semantics;
- a physical M05 `DeleteVector` opcode 19 requires the object to be present,
  writes one D02 tombstone and its authenticated path, and is covered by one
  trailing fixed `HYANNA02` opcode 56 marker for the index; and
- `FenceVectorAbsence` opcode 57 requires the object already to be absent and
  records conflict authority only. It does not write D02, metadata, manifest,
  or Merkle nodes and does not change an ANN root, view identity, count, byte
  count, `next_sequence`, or base identity.

An admitted absence is proved in overlay-first order. The writer validates the
exact D02 leaf or its authenticated non-membership path against the overlay
root, then the exact D01 record, then the selected base-child vector keys when
neither delta layer names the object. A D02 or D01 tombstone is absence and
suppresses a lower live value. More than one selected child containing the
object is corruption. Writer admission repeats this proof against the admitted
root, and recovery repeats it against the committed prior root; opcode 57
against a present prior object or any physical change attributable to that
fenced object/path fails closed.

Ordinary and physical-delta point upsert and present-object delete synthesize
M05, D02, overlay manifests/nodes, and fixed `HYANNA02` WAL opcode 56. Sequence
and mutation CSN are assigned under writer admission. `HYANNA02.operation_count`
counts only physical upserts and deletes, so a mixed transaction may carry
opcode 57 records without counting them in the marker or consuming M05
sequences or delta slots. A transaction admits at most 4,096 opcode 57 records
in total; they remain ordinary WAL mutations and therefore still count toward
the transaction mutation body. A pure-fence transaction appends its transaction
ID and commit to WAL and advances commit CSN and global visibility while naming
the unchanged four root page IDs, blob generation, and catalog version.

Physical point operations validate an object conflict key and index-generation
authority but publish only the object key, allowing disjoint point writers to
rebase. An absence fence validates and publishes its object key and the index
lifecycle-fence key, not the generation key, so same-object upsert/delete races
conflict in either commit order and stale initial-bulk or consolidation plans
cannot cross the omission. Disjoint fences and physical points compose.
Creation and vectors in the same transaction remain canonical M04. M05
initial-bulk rewriting remains fail closed. Consolidation is the separately
bounded maintenance writer specified below; it adds no protocol, catalog,
backup/proof version, background scheduler, release, or broad performance
claim. Lexical compaction and page-generation vacuum may preserve authenticated
bytes byte-for-byte.

Open and snapshot materialization scan the selected base and delta, validate
every ANN physical key/value, reconstruct the base `IndexSnapshot`, and require
`HnswIndex::restore` to reproduce it exactly. Delta envelopes, dimensions,
sequences, counts, bytes and the selected view identity are also checked.
Unknown indexes, missing metadata, orphan records, malformed vectors/layers,
count divergence, bad neighbors or a noncanonical build fail the complete root.
Queries currently traverse this validated in-memory materialization, not
buffer-pool pages directly.

Recovery discovers changed ANN indexes from a bounded immutable B+tree diff over
all physical prefixes `0x05` through `0x0b`, including an absent prior root. It
does not rely on WAL targets or metadata-only differences. Metadata from both
roots bounds changed-key, changed-index, and node work. Any point-writer-changed
index with M05 on either side requires exact HYANNA02, physical vector-mutation
agreement, and the complete per-index point-transition proof. Opcode 50 instead
requires exact HYANNC02 consolidation authority and the replacement proof
below. Unchanged M05 beside lexical-only work is inherited by shared page
identity. Opcode 57 indexes are also replayed
from the committed prior root even though they are absent from the physical
diff and marker count. Recovery reconstructs their object and lifecycle-fence
conflict keys. A commit containing only opcode 57 must retain every root page
ID, blob generation, and catalog version exactly while its WAL commit and CSN
remain authoritative.

## MVCC and mutation

New/updated vectors and physical tombstones enter the transaction-private object
delta. `upsert_vectors` admits a duplicate-free batch atomically without
rebuilding the base. Single-vector upsert and delete preserve read-your-writes.
An absence fence changes no private or durable ANN value; it preserves the
already-absent read result while retaining commit-time conflict authority. At
commit, physical records receive the assigned CSN and are published with the
all-engine root. Retained root sets preserve historical base-plus-delta views.

The exact oracle ranks the effective set: start with base vectors, replace or
remove every object named by the delta, then apply metric order and object-ID
tie breaking. Approximate execution traverses the base graph, removes base hits
shadowed by any delta, scores every live delta upsert exactly, merges those
candidates and truncates to `k`.

The base query must oversample before that suppression instead of first
truncating the base to `k`. Let `B` be the selected base vector count, `D` the
effective object-keyed delta count, `S <= min(B, D)` the number of selected-base
objects shadowed by that delta, and `Q` the caller's `ef_search`. Its base-hit
target is the checked value `min(B, Q, k + S)`, and both graph search and
optional exact reranking remain at or below `Q`. The implementation must not
replace `Q` with the index's larger configured maximum.

Suppression then runs before final top-k truncation. If the base candidates
admitted by `Q`, together with live exact delta upserts, cannot replace all
shadowed hits, the approximate result is allowed to contain fewer than `k`
hits. Underfill does not authorize a complete-corpus exact scan, an exact
fallback, or an increase in `ef_search`; callers that need a wider attempt must
request a larger legal `Q`. Base count, the 4,096-record delta limit, and `Q`
are independent hard bounds.

The receipt's build identity is the selected view identity; without a delta it
equals the canonical base identity. This is honestly approximate because delta
candidates are exact while base candidates remain bounded by graph traversal.

## Bounded consolidation

For M04 or M05 input, `plan_ann_consolidation` captures one current effective
set and constructs a canonical replacement base outside writer admission. The
capture binds the source-version flag, selected base and view identities,
captured `next_sequence`, and the complete object-ordered effective delta. Its
fixed-body captured-record digest includes the index, each object's sequence,
kind, mutation CSN, and canonical vector bytes. The canonical effective-vector-
set digest also includes the index before its count and commutative record
accumulators. The enclosing fixed body duplicates the same target index, and
publication and recovery require the two authorities to agree. Both formulas
are fixed by the WAL contract. Both effective vectors and captured delta
records have caller-supplied hard bounds; the implementation also caps those
requests at 1,000,000 vectors and 4,096 delta records. An empty delta is not a
consolidation candidate. New C02 capture/result format bytes are exactly
M04-or-M05/M05.
Historical fixed 112-byte `HYANNC01` remains authoritative only for its original
M01-through-M04 transitions; it cannot authorize M05 and is not emitted.

`consolidate_ann` publishes a captured plan through an ordinary root commit and
append-only search maintenance opcode 50 with fixed 384-byte `HYANNC02`
authority. Publication requires the captured base and index definition still
to be selected. Physical D01 and D02 records below captured `next_sequence`
must reconstruct the captured object/sequence map, count, ordered
effective-record digest, and format-specific view exactly. A current effective
record still at its captured sequence is consumed; a greater-sequence record is
preserved, while a new object is later only at or above the boundary.

A later D02 may shadow a captured D01 because the D01 still reconstructs the
capture. Overwriting a captured D02, or overwriting a captured D01 in D01,
removes or changes that proof and makes the plan stale even when the new
sequence is greater. A missing record, changed capture kind/CSN/vector, lower
later sequence, or unclassified record is also stale. The complete C02 body is
encoded and validated before any page is appended.

The result is canonical M05 with the replacement base selected and an
overlay-only delta. It contains no D01 key. Every preserved later upsert or
tombstone is represented once in D02 with its object, sequence, mutation CSN,
kind, and vector bytes unchanged; no captured-equal record remains. Metadata
and `HYANNO01` set frozen D01 count and bytes to zero, set the frozen legacy
view identity to the replacement base, and set frozen `next_sequence` to the
minimum preserved sequence or the final `next_sequence` when the overlay is
empty. The final `next_sequence` equals the publication-time prior value:
consolidation neither allocates nor renumbers a sequence. D02 leaves, ordered
sparse-Merkle nodes, counts, bytes, root, and final view identity are rebuilt
canonically from exactly that preserved set.

Retention starts with the publication-time retained-generation list and removes
the replacement identity from that list while preserving survivor order. It
then appends the publication-time selected nonempty base at the end only when
it differs from the replacement and is not already retained, and drops oldest
descriptors only as needed to satisfy `retain_generations`. This promotes a
retained generation selected again by an A-to-B-to-A cycle without duplicating
it. Every surviving selected or retained child must retain its complete vector
and graph records. Existing
surviving generation bytes are
copied byte-for-byte, while exactly the retired generations are removed. The
B+tree replacement checks the exact current key set under the format key and
all target prefixes `0x05` through `0x0b` before its first append, validates the
complete replacement and all selected/retained graphs in an unpublished tail,
and exposes no candidate page on rejection.

Strict interruption has the ordinary singleton cut: `BlobStaged` through
`PageSynchronized` reopen the prior root; `WalAppended` through `RootPublished`
reopen the complete replacement. Recovery validates every retained transition
against its immediately preceding committed root, including superseded result
roots. It derives the below-bound capture cohort, recomputes the C02 count and
ordered digest, and requires exactly the canonical overlay-only result,
unchanged `next_sequence`, lifecycle policy, retained-generation transition,
complete selected/retained vectors and graphs, and logical effective vectors. A
mixed base, D01/D02 result, omitted later record, consumed later record, extra
record, noncanonical overlay, descriptor without its graph, orphan generation,
or unrelated target change is an invalid committed root. Historical roots and
snapshot pins retain normal page-generation safety; physical reclamation
remains page vacuum rather than an ANN-specific reclaimer. Initial-bulk
rewriting of M05 and automatic background scheduling remain non-claims.

A query traverses the visible graph and may exact-rerank a declared candidate
count. A complete caller allowlist containing at most `ef_search` identifiers
uses exact object-point evaluation. A larger allowlist uses bounded filtered
graph traversal; a partitioned base routes that work through its bounded child
graphs rather than scanning the complete base to count visible eligibility.
Typed predicate construction remains outside this API.

## Filtering

Stable-ID bitmaps and typed doc-value predicates may run before, during, or
after graph traversal according to the physical plan. The explanation records
which strategy ran and whether it can reduce recall.

The current bounded implementation accepts a stable `ObjectId` allowlist. When
the caller supplies more identifiers than `ef_search`, navigation may visit
admitted and non-admitted connector nodes, but disallowed nodes never become
hits. A single base reports `StableIdEligibilityTraversal` and
`FilteredApproximateTraversal`; a partitioned base reports its bounded graph
strategy and approximation risk. Either graph path may honestly underfill when
its bounded candidates cannot supply `k` eligible, unshadowed hits. It never
fills from a complete exact scan or silently increases `ef_search`.

When the complete caller allowlist cardinality is at most `ef_search`, both
single and partitioned bases perform direct point evaluation of exactly those
identifiers and report `StableIdAdaptiveExact` and `ExactFilteredCandidates`.
The decision uses caller allowlist cardinality, including absent identifiers;
it does not scan the base to lower that count. Exact point evaluation may return
fewer than `k` only because fewer than `k` live eligible objects exist.

Integrated collection search materializes one exact allowlist for all vector
branches from the collection manifest and the request filter. `MatchAll` means
the manifest membership, not every object that happens to share the physical
ANN index. Each ANN branch returns the native filtered result and receipt
directly; the product layer does not add an exact seed, merge a post-filter
oracle hit, or rewrite the runtime's candidate, visit, rerank, approximation,
or underfill evidence. Consequently either bounded graph shape can underfill,
while a point-evaluated branch remains labelled exact.

## Result contract

Every ANN result names:

- approximate status;
- index/build identity and snapshot CSN;
- metric, `k`, `ef_search`, filters and candidate count;
- whether exact reranking ran;
- returned distance/score and stable ID; and
- measured quality profile applicable to that build, when available.

Explicit partition-routing receipts also bind execution to the persistent
scheduler. `targeted_single_batches` counts one-item parallel waves accepted by
the stable worker-local route, while `generic_single_fallback_batches` counts
one-item waves admitted through the generic queue because that worker slot was
busy. In one query receipt their sum is bounded by the worker-batch count and
equals the number of one-item waves. Interval aggregation uses checked sums;
an interval maximum is not substituted for those totals. Multi-item waves,
direct serial execution, and a
single-generation fallback report zero for both. G7 warm/control C1 evidence
requires one targeted batch per observation and no generic fallback; C8 and C32
may report bounded fallback honestly.

A proof can attest to execution, inputs, graph identity, candidates and exact
reranking. It cannot claim global nearest-neighbor optimality unless the query
used the exact oracle.

## Scalar quantization primitive

`Sq8Quantizer` is the audited compression primitive for a future
compressed traversal mode. Training folds the exact global minimum
(`b`) and range (`a`) over the training vectors in input order;
training data whose range is zero or non-finite fails closed. Encoding
maps each component to `clamp(floor((x − b) · 255 / a), 0, 255)` and
retains the code sum and squared-code sum, so asymmetric distances
never decode:

- squared L2 ≈ `a2 · Σ(cx − cy)²`;
- dot ≈ `a2 · Σ cx·cy + ab · (Σcx + Σcy) + ib2`; and
- cosine derives both norms from the retained sums via
  `norm² ≈ a2 · Σc² + 2·ab · Σc + ib2`, rejecting zero norms,

with `a2 = a²/255²`, `ab = a·b/255`, `ib2 = b²·dimension`. All
arithmetic is f64 in deterministic input order. The V1 quality gate
requires that compressed top-`3k` candidates rescored with exact
distances recover recall@k ≥ 0.95 against the exact oracle on the
bounded deterministic corpus, per metric. The primitive is not yet
wired into the durable index format; graphs continue to persist exact
`f32` vectors.

## Quality gates

The G4 bounded correctness profile requires deterministic exact-oracle recall
evidence with recall@10 at least 0.95. The 1,000,000-vector, 384-dimension
latency and memory target remains a G7 performance profile under the
[microsecond contract](../performance/microsecond-first.md).

Receipts also report build time, ingest/update/delete cost, graph bytes per
vector, tombstone ratio, rebuild time, p50/p95/p99/p99.9 and recall
distribution across at least ten deterministic query sets.

## Verification

Verification for this contract must additionally cover unchanged base identity
and generation-record counts across foreground mutations, base/D01/D02 point
deletion, prior-absence proof, mixed physical/fence marker counts, the 4,096
fence bound, sequence and slot non-consumption, same-object races in both
orders, disjoint group composition, unchanged-root pure-fence recovery, reopen
and effective exact equivalence, strict HYANNC02 maintenance WAL decoding,
the index-bound captured-record and effective-vector-set digest formulas,
M04/M05 capture with M05 result, retained C01 authority for
legacy transitions, M01-M03 write-to-M04 upgrade and M01-M04 grandfathered
load with candidate M05 rejection,
canonical overlay-only output, preservation of later objects and
`next_sequence`, captured-D01 shadow preservation, captured-D02 overwrite and
stale-base rejection, old-or-new interruption recovery, complete retained graph
generations, configured retention, policy
bounds, due-plan generation, pin-safe old-root retention, unpin plus
page-vacuum collection, and filter-aware recall against the exact oracle.
Background scheduling and page-buffered traversal remain future production
work.
