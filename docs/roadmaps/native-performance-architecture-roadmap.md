<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->

# Native hardware-aware performance roadmap

Status: active engineering program; no performance closure claimed

This roadmap turns `microsecond-first` from a collection of isolated targets
into one hardware-aware execution architecture for Native SQL, data
structures, lexical search, vector search, durability, and cross-engine work.
It does not weaken correctness, deterministic results, durability, or the G7
measurement contract.

## Implementation status

| Program | Current evidence | State |
|---|---|---|
| P0 evidence | Versioned receipt, progress, and suite contracts; external required-cell authority; embedded structure baseline; ANN kernel-to-publication progress | implemented, no G7 claim |
| P1 discovery | Read-only embedded API and `hyphae hardware discover`; Linux per-processor core/socket/NUMA/SMT placement plus macOS and Windows fingerprint adapters | in progress |
| P1 calibration | Versioned CPU/memory/engine/storage/WAL receipt, Native B+tree, posting, filesystem sync, block-framed WAL, Linux hard-affinity scaling, and controlled I/O-depth cells; multi-NUMA residency is explicitly unsupported until an exact safe VMA provider exists | in progress |
| P2 governor | Versioned hardware-derived policy plus rollback-safe global/class CPU, I/O, and memory admission; RAII cancellation and parent-only nested subdivision | persistent physical-core-first NUMA queues and governed exact-ANN batches now complement routed reads/mutations/hybrid/maintenance/recovery/backup/WAL retention; exact ANN has target-query workspace accounting and component receipts, and worker completion is published before caller notification; index-scoped/all-engine hydration accounting, pool connections beyond exact ANN, calibrated cross-node stealing, broader receipts, proof routing, and measured interference pending |
| P3 segmented substrate | Immutable B+tree leaf planning with root-bound range/cardinality summaries, snapshot-frozen readers, and governed parallel relational, hash, set, stream, sorted-set score/rank, list, BM25, and contiguous exact-vector batches; directional/tie order is deterministic; multi-root recovery and deterministic maintained/compaction amplification bounds are proven | in progress; richer SQL/structure/lexical/vector pruning summaries plus dedicated-hardware pruning and foreground-interference receipts pending |
| P4 vector execution | Deterministic recursively projected balanced plans with one projection evaluation per vector/split, centroid/radius/projection summaries, explicit selected-partition routing, pre-plan `Bulk` admission, cancelable planning/child ingestion, governed persistent-pool child builds without a second corpus copy, canonical exact/approximate merge, aggregate identities, an ownership-transferred HYANNM04 partitioned base, append-only WAL publication, exact stale-plan checks, prior-or-complete recovery, retained-child ownership, online delta/consolidation lifecycle, and process-local plus durable receipts are implemented | in progress; bounded cross-links, accepted pruning policy, competing algorithms, resumable build checkpoints, full lifecycle/interference matrix, allocation/RSS proof, and bare-metal quality/build evidence pending |
| P5 SQL execution | Direct small-query path, catalog-version-bound 256-entry prepared plan/expression cache, exact ordered multi-index intersection with root-bound leaf-summary ordering for 3+ streams, governed 1,024-row decode/filter/project batches, indexed nested-loop join/probe receipts, independent fail-closed 1,048,576-candidate ceilings for scans and joins, a 64-byte-aligned selection bitmap, and a CSN/root-bound execution receipt | in progress; allocation proof, SIMD/columnar kernels, persisted fine-grained statistics and stale controls, aggregation/sort, hash/merge join alternatives and adaptive choice, spill, reusable cross-engine masks, adversarial budget matrices, and dedicated-hardware evidence pending |
| P6 structure execution | Direct governed points; immutable segmented hash/set/list/stream/sorted-set ranges; bounded hash/set batches, set algebra, and expiry sweeps; per-field/member optimistic conflicts; componentized commit/group durability receipts; HYSTRBT3 incarnation/key/typed-family metadata/retirement codecs; explicit lossless V2-to-V3 WAL migration and V3 backup/restore; public constant-cardinality deletion for all five collection families; public V3 scalar set/conditional/increment/expire/delete plus five-family due-key reuse; public incarnation-aware create/recreate plus hash set/batch/increment/delete, set add/batch/remove, list head/tail push and pop, stream append, sorted-set add/rescore/remove, collection/Hash-field TTL mutation, and ordered bounded active-expiry paths; all 30 current-root logical structure commands (52 public Rust methods), including Hash scans/patterns, Set algebra/scans, List/Stream ranges, and Sorted Set rank/rank-range/score-range paths, now execute directly or through governed immutable V3 segments without complete structure-state materialization; scalar all-engine delta commits resolve only point metadata and both HYSTRBT2/HYSTRBT3 scalar replacement avoid the old durable payload; exact-field V3 Hash delta HSET/HDEL/HINCRBY and exact value/field-TTL read-your-writes hydrate only the addressed field, while whole-Hash TTL resolves exact typed V3 point metadata and remains unchanged by field writes; one conservative batch-wide retained-memory ledger bounds point-resolved SQL/scalar/lexical/Hash deltas beneath the 32 MiB mutation allocation while preserving an 8 MiB Hash sub-budget; SQL delta requires HYCAT006 and rejects relations with inbound or outbound foreign keys; batches are linear and bound to their exact live database handle; governor-admitted, shared-buffer-pool reclamation in at most 1,024-entry steps with progress/no-op receipts; V3 compaction that preserves active retirements; and delete, recreate, scalar/member/list-boundary mutation, direct-read/range, TTL, expiry-sweep, partial-cleanup, terminal-cleanup, migration, backup/restore, corruption, and reopen matrices | in progress; the delta ledger is conservative rather than allocation-exact; aggregate/scanning delta reads, foreign-key validation, remaining collection-valued delta mutations, plan-sized legacy Product materialized candidates, and other legacy public transactions remain unsupported or retain complete-state work; page-generation and broader backup interruption matrices; million-member and allocation/RSS proof; SIMD kernels; hot-key and concurrent lifecycle scaling; and dedicated-hardware evidence pending |
| P7 cross-engine fusion | Common-snapshot lexical/vector retrieval and canonical reciprocal-rank fusion already execute without an internal protocol hop | in progress; the 3.0.0 durable posting scorer now merges scores in one linear pass and borrows leaf entries from the verified buffer pool (8,192 frames, up from 1,024) rather than cloning them — CHANGELOG §3.0.0, measured 2.3× on the scorer stage from pool residency alone and page verification (BLAKE3/CRC32C) dropping out of the scorer's top-40 profile entirely ([`hyphae-3.0-metal-a443c52-2026-09-03.md`](../gates/evidence/hyphae-3.0-metal-a443c52-2026-09-03.md) §6); shared SQL/structure masks, concurrent branch scheduling, one arena/deadline/cancellation budget, bounded streaming merge across engines, and exact same-snapshot G7 evidence still pending |
| P8 storage/background | Group-commit cohort/outcome receipts, bounded WAL replay, crash matrices, governed compaction/vacuum/backup/expiry/ANN consolidation, and interruptible ANN publication are implemented | in progress; full optimized-publication crash coverage, device-calibrated cohort policy, asynchronous portable I/O boundary, comprehensive background progress, and paired interference/recovery evidence pending |

P1 discovery still requires Windows affinity/NUMA/cache and storage-queue
enrichment. Linux calibration now has active, fail-closed aligned `O_DIRECT`,
hard-affinity worker scaling. First-touch affinity alone is not residency proof,
so multi-node memory calibration and cross-node stealing remain explicitly
`unsupported`/`disabled`. Multi-node bare-metal qualification, a safe exact-VMA
residency adapter, safe SIMD, platform async I/O,
and equivalent affinity/NUMA adapters elsewhere remain pending. The portable
fallback and current receipt report unknown or unsupported properties
explicitly.

## Outcome

Hyphae should discover the machine it is running on, calibrate the primitives
that materially affect execution, segment native data by access pattern, and
assign bounded CPU, memory, and I/O budgets through one scheduler. The engine
must optimize for two different objectives without conflating them:

- bounded foreground operations target predictable microsecond latency; and
- corpus construction, scans, consolidation, backup, and maintenance target
  controlled parallel throughput and progress.

Using every hardware thread is not itself a success condition. A successful
configuration minimizes cycles, cache misses, queueing, and tail latency for
foreground work while using the machine efficiently for work that is actually
parallel.

## Non-negotiable invariants

1. SQL, structures, lexical search, and vector search retain one catalog, one
   MVCC snapshot, one commit sequence, one WAL authority, and direct typed
   engine-to-engine calls.
2. No internal TCP, HTTP, JSON, RESP, PostgreSQL wire, or search compatibility
   protocol is introduced.
3. Result ordering, tie-breaking, durable state, and generation identities are
   independent of thread scheduling.
4. Approximate search remains explicitly labelled and must meet its declared
   recall floor against the exact native oracle.
5. Background work never has an unbounded CPU, memory, I/O, or queue budget.
6. Physical synchronization, cold I/O, unbounded work, and corpus construction
   are reported separately from bounded hot-path latency.
7. CPU is the complete portable implementation. Optional SIMD, accelerator,
   and operating-system paths require a tested fallback and cannot change
   logical results or product availability.

## Workload classes

The scheduler must classify work before it allocates resources.

| Class | Examples | Primary objective |
|---|---|---|
| Foreground point | structure get, prepared SQL primary-key read | p50/p99 latency, zero queueing |
| Foreground bounded | indexed SQL, bounded join, BM25/ANN/hybrid top-k | latency under an explicit work bound |
| Mutation | set, row mutation, document/vector update | admission, publication, and durability clocks |
| Bulk | initial load, ANN build, index creation, large import | throughput, bounded memory, progress |
| Maintenance | compaction, expiry, statistics, ANN consolidation | bounded interference and guaranteed progress |
| Recovery | WAL replay, manifest validation, index reopen | correctness, bounded recovery time |
| Administrative | backup, proof, vacuum, verification | isolation, cancellation, and auditable progress |

## Program 0: freeze the evidence contract

Before optimizing an engine, add a component timing and resource receipt that
records:

- admission and queueing;
- parse, bind, planning, and prepared-plan lookup;
- engine execution;
- cross-engine fusion without serialization;
- WAL append and physical synchronization;
- result/proof encoding;
- CPU time, cycles, instructions, cache misses, context switches, page faults,
  allocations, RSS, and bytes read/written; and
- exact source, dataset, hardware fingerprint, topology, affinity, profile,
  compiler, operating system, and configuration.

Every performance change starts with a failing or deficient baseline receipt
and ends with a comparable receipt. A benchmark without correctness checks is
diagnostic only.

### Gate P0

- One versioned receipt schema covers every workload class.
- Unsupported counters are reported as unsupported, never as zero.
- Clock decomposition sums consistently to the enclosing observation.
- Receipts bind the exact source tree and immutable dataset identity.
- The checker rejects missing cells, reused seeds, mismatched hardware, and
  claims made from virtualized or shared hosts.

## Program 1: hardware discovery and calibration

Add a read-only `HardwareProfile` and a calibration command exposed through
the embedded facade and CLI. Discovery must not silently modify the host.

### Static discovery

- physical cores, SMT threads, sockets, NUMA nodes, affinity, and CPU quota;
- cache hierarchy, cache-line size, supported SIMD instructions, and frequency
  governor;
- total and available memory, page size, huge-page availability, and NUMA
  memory placement;
- storage devices, filesystem, mount options, queue depth, discard, direct-I/O
  support, and virtualization status; and
- operating-system and local transport capabilities.

### Active calibration

- scalar and SIMD dot product, cosine, L2, hashing, CRC, and comparison kernels
  by representative vector/key width;
- sequential and random memory bandwidth and latency, including NUMA-local and
  remote access when applicable;
- B+tree page lookup, posting decode, bitmap intersection, arena allocation,
  channel handoff, and atomic-operation cost;
- buffered append, WAL append, group flush, fsync/fdatasync, random page read,
  and controlled queue-depth sweeps; and
- one-to-many thread scaling over physical cores first, then SMT.

Calibration has three modes:

- discovery: sub-second and safe at every start;
- quick: bounded first-install calibration, approximately 5–15 seconds; and
- thorough: opt-in qualification, approximately 3–10 minutes.

Results are cached under a fingerprint containing hardware, kernel, filesystem,
compiler, and Hyphae build identity. Passive telemetry may refine scheduling
inside declared bounds but may not rewrite performance claims.

### Gate P1

- The same machine produces a stable topology and capability fingerprint.
- Calibration reports variance and rejects unstable samples.
- CPU kernels are selected only when feature detection and differential tests
  pass.
- NUMA-local and remote behavior are distinguishable where NUMA exists.
- The generated profile is sufficient to reproduce scheduler decisions.

## Program 2: one resource governor for all engines

Replace independent or implicit thread decisions with a shared governor. It
owns bounded pools and tokens for:

- foreground execution;
- bulk computation;
- commit/WAL work;
- storage I/O;
- maintenance;
- request arenas and build scratch memory; and
- per-tenant or per-database admission.

The governor must prevent nested parallelism from oversubscribing the host.
For example, 32 concurrent SQL queries cannot each create 96 workers, and an
ANN build cannot consume every core needed by group commit or local transport.

Worker pools are NUMA-aware. Work stealing stays inside a NUMA node first and
crosses nodes only after a calibrated threshold. Foreground pools reserve
capacity; bulk pools consume idle capacity and yield when measured queueing or
tail latency exceeds policy.

### Initial policy for the current i7i.metal-24xl

AWS reports 48 physical cores, 96 hardware threads, 768 GiB RAM, six 3.75 TB
NVMe SSDs, and 22.5 TB total local NVMe. Exact socket and NUMA placement must
come from the machine receipt rather than be inferred from the instance name.

Initial policies are calibration candidates, not hard-coded product defaults:

| Mode | Physical-core budget | SMT policy | Intended use |
|---|---:|---|---|
| Latency qualification | 4 system/WAL, up to 40 foreground, 4 reserve | off first; compare separately | clean p50/p99 and saturation curves |
| Bulk construction | 4 system/I/O, up to 44 bulk | enable only with measured throughput gain | ANN/index build and import |
| Mixed service | 4 system, 8 commit/I/O/maintenance, 16 foreground, up to 20 background | adaptive within caps | interference qualification |

The memory governor initially retains at least 15% host headroom. It does not
allocate RAM merely because it exists. Corpus pages, indexes, posting blocks,
build scratch, arenas, WAL buffers, and OS cache receive explicit high-water
marks. Local NVMe striping is eligible for rebuildable scratch and benchmark
datasets; durable authority requires a separately accepted failure model.

### Gate P2

- No nested-pool oversubscription at concurrency 1, 8, 32, or saturation.
- Bulk work demonstrates a scaling curve across physical cores and SMT.
- Foreground p99 degradation under background work remains inside a frozen
  interference budget.
- Cancellation returns CPU, memory, queue, and I/O tokens.
- Starvation tests prove both foreground priority and maintenance progress.

## Program 3: segmented native data substrate

All engines use immutable or append-friendly segments plus bounded mutable
deltas. Segmentation is physical; it does not divide catalog or transaction
authority.

Each segment carries engine-appropriate summaries:

- key and row ranges, min/max values, null counts, and compact membership
  filters for SQL;
- key-family ranges, cardinality, expiry bounds, and collection metadata for
  structures;
- term ranges, document frequencies, block maxima, and filter bitmaps for
  lexical search; and
- vector count, centroid/radius summaries, dimensions, metric, and ANN
  generation information for vector search.

The planner prunes segments before scheduling work. Selected segments execute
in parallel under one snapshot and merge canonically. Small bounded operations
stay on the direct single-segment path to avoid scheduler overhead.

### Gate P3

- Segment pruning is proven against an unsegmented exact oracle.
- Cross-segment results and ties are identical for every worker count.
- Publication is atomic at one CSN and recovery never exposes a partial set.
- Compaction amplification and foreground interference are bounded and
  measured.

## Program 4: vector execution

### Separate build and mutation algorithms

The current initial build behaves like a long serial chain of online HNSW
insertions. Replace it with an explicit bulk-build contract while retaining
small mutable deltas for online writes.

Evaluate, without prematurely selecting one implementation:

1. deterministic epoch-based HNSW construction;
2. deterministically partitioned HNSW with canonical cross-partition links;
3. Vamana/DiskANN-style construction for large durable corpora;
4. exact flat SIMD for small or highly filtered candidate sets; and
5. IVF and optional quantization where memory or corpus size justifies the
   additional recall contract.

Each candidate uses the same datasets, exact oracle, memory limit, recovery
tests, and quality matrix. The selected planner may keep more than one physical
strategy and choose by corpus size, dimension, filter selectivity, update rate,
available memory, and calibrated hardware.

### Parallel bulk path

- ingest into aligned contiguous vector blocks;
- precompute norms and reusable metric metadata;
- partition deterministically using vector geometry and balance constraints;
- calculate candidates concurrently from immutable epoch snapshots;
- resolve conflicts and publish edges in canonical order;
- build independent partitions in parallel and add bounded cross-partition
  navigation links;
- persist checkpoints and progress so cancellation or failure does not restart
  all accepted work; and
- atomically publish the completed generation while online deltas remain
  queryable.

The first experimental slice is specified in
[`native-vector-bulk-build-v1.md`](../performance/native-vector-bulk-build-v1.md).
It implements deterministic balanced geometric partitions, shared-governor
`Bulk` admission, ownership transfer to persistent workers, independent child
builds, canonical aggregate validation, and exact fanout merge. It remains
process-local and intentionally does not claim cross-partition links, selective
fanout, checkpoints, or durable publication.

The durable companion vertical is specified in
[`native-ann-initial-bulk-publication-v1.md`](../performance/native-ann-initial-bulk-publication-v1.md).
It freezes the empty-base precondition, off-lock governed build, exact stale-plan
checks, partition-aware durable metadata, WAL anchoring, cooperative
cancellation, and prior-or-complete recovery. The runtime implementation, local
correctness matrix, and fail-closed G7 wiring are present; bare-metal quality,
capacity, and latency evidence remains a separate gate and cannot be inferred
from those local tests.

Durable default search currently performs full partition fanout and canonical
top-k merge. The experimental routed path uses certified metric lower bounds,
canonical merge, and explicit full-fanout fallback when its preferred budget
cannot prove an omission safe. It cannot become the default until its pruning
and reranking policy satisfies the accepted recall gate. Its checked request,
receipt, fallback, and lifecycle boundary are specified in
[`native-ann-durable-routing-v1.md`](../performance/native-ann-durable-routing-v1.md).
The next hot-loop vertical is frozen by
[`native-ann-read-view-v1.md`](../performance/native-ann-read-view-v1.md): one
governed index-scoped hydration produces an owned immutable view whose retained
memory stays admitted, while each query admits CPU and scratch independently.
Opening and hydration remain outside the query histogram. This is a planned
contract and does not close P4 or G7.

### Gate P4

- Recall at 10 is at least 0.95 on every accepted corpus, not only on average.
- Snapshots and generation identities match across 1, 8, 32, and maximum
  calibrated workers.
- Build throughput, peak RSS, write amplification, and recovery time are
  reported alongside query latency.
- Update, delete, consolidation, interruption, and reopen preserve visibility.
- The million-vector, 384-dimensional corpus completes within a frozen build
  budget before it may enter a release G7 run.

The `1,000,000 x 384` corpus remains the frozen G7 qualification shape for the
1.0 release line. A later release must add independent `1,000,000 x 768` and
`1,000,000 x 1,024` capacity lanes without changing or retroactively weakening
that receipt. Those lanes require their own calibrated build/query memory
budgets, NUMA and partition curves, SIMD kernel selection, persistence and
recovery measurements, recall floors, index-scoped multi-ANN loading or
full-root admission, and exact-SHA receipts. They are future architecture work,
not additional closure requirements for 1.0.

## Program 5: SQL execution

Keep a direct row path for point and tiny indexed queries. Add a vectorized,
segment-oriented execution path for bounded scans and joins.

Work includes:

- cache-aligned row/column decode kernels selected by access pattern;
- prepared-plan and prepared-expression caches bound to catalog versions;
- segment statistics, membership filters, and index intersection;
- vectorized predicate, projection, aggregation, and sorting kernels;
- adaptive selection among index nested-loop, hash, and merge joins;
- NUMA-local parallel scan/join partitions with deterministic merge;
- bounded spill with explicit I/O and memory tokens; and
- adaptive thresholds so small queries never pay parallel setup costs.

SQL filters must be reusable directly by lexical/vector execution as native
bitmaps or candidate iterators. They must not materialize through a transport
or a second database representation.

The first experimental slice is specified in
[`native-sql-vector-execution-v1.md`](../performance/native-sql-vector-execution-v1.md).
It retains the direct path at limits through 256 and adds governed,
batch-at-a-time current-root scans above that threshold. The compact selection
bitmap is native and transport-free, but cross-engine consumption remains P7
work.

### Gate P5

- Differential results match the scalar native SQL oracle.
- Prepared point reads allocate nothing after admission.
- Indexed queries never regress to unbounded scans silently.
- Parallel and spilled plans preserve ordering, isolation, cancellation, and
  recovery.
- Point, bounded range, aggregation, and join matrices include skew, empty
  results, hot keys, selectivity changes, and stale-statistics controls.

## Program 6: native structures execution

Strengthen scalar, hash, set, list, sorted-set, and stream operations around
key-range ownership and collection-local segments.

Work includes:

- direct allocation-free point paths;
- cache-conscious key encoding and prefix/range traversal;
- independent segment ownership for unrelated keys and collections;
- bounded multi-key coordination under the shared MVCC/commit authority;
- batched and SIMD-assisted comparisons, membership, and score filtering;
- timing-wheel or equivalent segmented expiry scheduling;
- incremental collection deletion and cleanup with progress, instead of large
  foreground cardinality walks; and
- group publication that separates private mutation, WAL append, and physical
  synchronization clocks.

The interface may remain semantically familiar to Valkey users, but the
execution remains fully Hyphae-owned and shares pages, WAL, MVCC, scheduling,
backup, and proofs with SQL and search.

### Gate P6

- Command equivalence, TTL, atomic multi-family, and crash matrices stay green.
- Unrelated keys scale across partitions without a global reader mutex.
- Whole-collection lifecycle work is incremental and interference-bounded.
- Hot point paths meet allocation and microsecond targets before local
  transport is added.
- Skewed hot-key tests expose serialization rather than hiding it in aggregate
  throughput.

## Program 7: cross-engine fusion

The largest advantage of the unified engine should come from avoiding copied
intermediate results.

- SQL predicates produce segment masks consumed directly by BM25 and ANN.
- Lexical and vector branches share stable document IDs and filter state.
- Structure keys can resolve catalog-bound row/document identities without a
  protocol hop.
- Hybrid execution schedules branches concurrently when profitable and shares
  cancellation, deadline, arena, and result budget.
- The planner uses segment summaries across engines while every read remains
  bound to one immutable root set and CSN.
- Fusion operators stream bounded candidates and merge canonically instead of
  materializing unbounded intermediate collections.

### Gate P7

- Cross-engine results match independently executed exact branches.
- One receipt proves a common CSN, dataset identity, filter population, and
  execution budget.
- Fused execution reduces cycles, bytes moved, or latency against the composed
  native baseline without changing results.
- Failure, cancellation, and deadline tests never expose partial mutations or
  provisional reads as complete.

## Program 8: WAL, storage, and background work

- Calibrate group-commit cohort size and delay against the actual device.
- Keep one logical WAL order while preparing independent page/segment work in
  parallel.
- Coalesce writes without merging durability classes or deadlines incorrectly.
- Use asynchronous or platform-specific I/O only behind a portable native
  boundary with equivalent recovery behavior.
- Schedule checkpoint, vacuum, ANN consolidation, expiry, and backup through
  the same resource governor.
- Publish progress and safe cancellation points for every long operation.

Strict durability remains hardware-dependent. Fast hardware does not permit a
weaker synchronization promise, and a slow fsync is never reported as engine
execution latency.

### Gate P8

- Power-loss and process-crash matrices cover every optimized publication path.
- Group commit proves cohort membership, durability class, and final outcome.
- Background work stays inside CPU/I/O/memory budgets under paired control and
  interference runs.
- Recovery and backup verification use independently checked durable state.

## Benchmark hierarchy

### Level A: kernel

Distance, decode, hash, compare, bitmap, posting, B+tree page, arena, WAL, and
transport kernels. Use these for dispatch calibration, not product claims.

### Level B: engine

One SQL operator, structure command, lexical branch, vector branch, commit, or
maintenance operation with correctness assertions.

### Level C: converged product

Embedded and local-protocol operations over the same durable corpus and CSN,
including hybrid and all-engine transactions.

### Level D: qualification

Dedicated-hardware control/interference matrices, concurrency 1/8/32,
saturation sweeps, one million hot observations, crash/recovery, and exact-SHA
receipts. Only this level can close G7.

Every scaling receipt reports useful work per physical core, cycles per
operation, scaling efficiency, memory bandwidth, LLC misses, queueing, and
tail latency. CPU percentage alone is insufficient: a memory-bound kernel can
be saturated while showing idle execution units.

## Delivery sequence

| Milestone | Deliverable | Depends on | Indicative focused effort |
|---|---|---|---:|
| M0 | Receipt schema, baseline, and reproducible profiler | none | 3–5 days |
| M1 | Hardware profile and quick/thorough calibration | M0 | 1–2 weeks |
| M2 | Shared NUMA-aware resource governor | M1 | 2–3 weeks |
| M3 | Segmented substrate and atomic publication | M2 | 2–4 weeks |
| M4 | Vector algorithm bake-off and bulk builder | M1–M3 | 3–6 weeks |
| M5 | SQL point/vectorized/parallel paths | M1–M3 | 3–5 weeks |
| M6 | Structure point/segmented lifecycle paths | M1–M3 | 2–4 weeks |
| M7 | Cross-engine fusion and shared planning | M4–M6 | 2–3 weeks |
| M8 | Storage, recovery, interference, and G7 qualification | M2–M7 | 2–3 weeks |

M4, M5, and M6 can proceed in parallel after M3. The ranges are engineering
planning estimates, not release promises. A single sequential stream is
approximately 17–30 focused engineering weeks; parallel ownership can reduce
elapsed time, but not the evidence required at each gate.

## Immediate next actions

1. Retain the composite exact-source scheduler authority preflight that binds
   profile, calibration, governor policy, execution topology, clean source
   tree, and calibrated executable before any dedicated benchmark.
2. Separate durable ANN logical partition count from execution worker count;
   prove one aggregate identity with 1, 8, 32, and maximum calibrated workers.
3. Qualify selected-partition durable ANN routing against the exact oracle with
   a per-query recall floor, bounded oversampling, and an explicit full-fanout
   fallback before sending the million-vector corpus to AWS again.
4. Implement the contract-first
   [`NativeAnnReadView`](../performance/native-ann-read-view-v1.md): retain one
   owned index-scoped hydration per captured root, charge its memory through
   the last handle, and prove that query observations perform no restore or
   physical read.
5. Freeze representative SQL, structure, lexical, vector, and mixed corpora;
   retain the conservative 32 MiB batch-wide SQL/scalar/lexical/Hash delta
   ledger and its 8 MiB Hash sub-budget, then extend checked physical accounting
   to the remaining collection families and the nominal legacy Product
   materialized candidate. Do not call the current upper bound
   allocation-exact or close P6 from the point-resolved slice alone.
6. Keep cross-NUMA stealing disabled while page residency is unsupported;
   accept it only from a complete exact-residency directed matrix, then prove
   foreground priority and maintenance progress under the derived policy.
   Multi-worker G7 evidence must not claim page-home or NUMA locality meanwhile.
7. Continue the epoch-HNSW and Vamana/DiskANN bake-off behind the same durable
   build/quality/recovery contract; do not select from build throughput alone.
8. Run physical-core/SMT/interference curves and the canonical G7 matrix on
   dedicated AWS bare metal only after the exact tested SHA passes every local
   functional and authority gate.

No provisional benchmark or optimized vertical closes G7 by itself. Closure
requires all required product cells and independent receipt validation on the
same exact source.
