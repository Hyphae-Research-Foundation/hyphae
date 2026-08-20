# Native acceleration and verification-asymmetry roadmap

Status: adopted 2026-08-20 as the forward plan after the `1.2.2` registry
publications. Base: `main` after `v1.2.2`, Apache-2.0, 24 crates. Method:
every claim in this document was verified against the source tree, not the
documentation. Guiding principle: verification asymmetry — acceleration may
touch anything whose result is verifiable or non-authoritative, and can be
the last word on nothing.

## 1. Current state, read from the code

### What `hyphae-native-ann` already implements

One crate, 4,910 lines, three dependencies (`blake3`, `hyphae-native-types`,
`thiserror`). No `rayon`, no `roaring`, no SIMD crates. The engine-wide hash
is already BLAKE3 (16 workspace crates depend on it; no `sha2` or `ed25519`
anywhere), so no hash migration exists on this roadmap.

```rust
pub enum AnnSearchStrategy {
    GraphTraversal,                 // bounded unfiltered traversal
    StableIdEligibilityTraversal,   // layer-zero eligibility allowlist
    StableIdAdaptiveExact,          // admitted set small enough for full exact scoring
}

pub enum AnnRecallRisk {
    ApproximateTraversal,
    FilteredApproximateTraversal,   // navigation still visits non-admitted nodes
    ExactFilteredCandidates,
}
```

Three observations that shape the plan:

1. **Layer-zero filtering already exists.** It is neither pre- nor
   post-filtering: eligibility participates in the traversal.
2. **The adaptive-to-exact strategy already exists.** A sufficiently small
   admitted set is scored completely and declared `ExactFilteredCandidates`.
3. **`AnnRecallRisk` is honest per-query qualification.** The engine tells
   the caller what guarantee that specific result carries. No competitor
   does this.

`SearchOptions` already carries `exact_rerank: Option<usize>`, `k`, and
`ef_search`: over-fetch with exact re-scoring — the pattern the entire
acceleration design leans on — is already the shape of the code.

### What does not exist

- A bitmap representation of the filter mask (today a stable-ID set).
- SIMD kernels; distance computation is scalar.
- Any parallelism or acceleration.
- Quantization.
- A measured latency baseline for filtered ANN.

### Open conditions at adoption

- **Registry publication is complete.** crates.io, npm (as the `@hyphae_`
  scope), and PyPI all serve `1.2.2` under Apache-2.0; the PyPI publication
  carries PEP 740 attestations. The two trusted-gate defects the first live
  runs exposed (the Cargo VCS dirty-marker expectation, and the JSON accept
  header sent to the crates.io download endpoint) are fixed on `main` with
  regression tests; the next release tag inherits both fixes.
- **G7 scope is declared.** The
  [native gate status](../gates/native-gate-status.md) now scopes the closed
  G7 profile to the pre-access-control engine and records the per-release
  1.2.x exact-SHA G8 closures.

## 2. Guiding principle

> **Acceleration may touch anything whose result is verifiable or
> non-authoritative. It cannot be the last word on anything.**

The boundary is not CPU versus GPU. It is authority versus acceleration.

### Authority map

| Stage | Acceleratable | Why |
|---|---|---|
| Index construction | **Yes** | The graph is an acceleration structure; its quality is measured, its identity is hashed |
| Candidate generation | **Yes** | Produces a superset; the CPU decides |
| Bulk ingest, compaction, rebuild | **Yes** | Batch, offline, outside the commit path |
| Embedding generation | **Yes, outside the engine** | Attested companion component |
| Exact rescoring and tie-breaking | **No** | Defines the result; must be bit-identical |
| WAL, MVCC, commit, CSN | **No** | Transactional authority |
| Proofs, witnesses, checkpoints | **No** | A proof over an irreproducible computation is not a proof |

### The pattern, already in the code

`SearchOptions::exact_rerank` implements today: fetch a wide candidate set,
score it exactly, return the top-k. `StableIdAdaptiveExact` implements the
variant where the admitted set is scored completely. **The whole
acceleration strategy changes who produces candidates without touching who
scores them.** An accelerated backend plugs into the candidate stage; exact
rescoring, stable-ID tie-breaking, and `AnnRecallRisk` qualification remain
on the CPU, unchanged.

## Phase 0 — Embedded-path contention (blocking)

G7 evidence, C1 to C32 degradation:

| Surface | C1 | C32 | Factor |
|---|---|---|---|
| Structure point get, embedded | 2.634 µs | 7,026.185 µs | ×2,667 |
| Structure point get, local protocol | 123.323 µs | 303.213 µs | ×2.5 |
| SQL prepared PK, embedded | 3.710 µs | 7,224.293 µs | ×1,947 |
| SQL prepared PK, local protocol | 128.761 µs | 287.156 µs | ×2.2 |

The signature is a global serialization point in the embedded facade; the
daemon, which queues and amortizes, does not exhibit it.

Work: profile contention at 8/16/32 threads **on `1.2.2`** (not against the
pre-access-control G7 numbers); identify the serialization point (page
cache, MVCC root set, catalog, scheduler, or the 1.2 authorization check);
measure access control in isolation (same benchmark with and without
authorization) to separate pre-existing contention from new work; target
embedded degradation within the same order of magnitude as the local
protocol (≤ ×10 from C1 to C32); measure and publish real binary size per
target.

This goes first because accelerating while the primary product surface
collapses at 32 threads optimizes the wrong layer, and any later gain
measurement is contaminated by a contended baseline.

## Phase 1 — Complete single-stage filtering

Three concrete gaps over an existing implementation:

1. **Navigation gap.** `FilteredApproximateTraversal` honestly documents
   that navigation still visits non-admitted connector nodes. The real work
   is making eligibility participate in navigation, raising the share of
   queries that qualify as `ExactFilteredCandidates` without widening
   `ef_search`.
2. **Measurement gap.** Filtered ANN is closed for correctness in G4, but
   no `Filtered ANN` row exists among the 11 G7 surfaces. **First
   deliverable: a `Filtered ANN top 10` baseline at C1/C8/C32 under the G7
   protocol, before optimizing anything.**
3. **Representation gap.** A `RoaringBitmap` mask (`roaring` is MIT,
   admitted by `deny.toml`) gives sublinear intersection and cardinality —
   exactly what makes the choice between `StableIdEligibilityTraversal`
   and `StableIdAdaptiveExact` cheap.

**Correctness requirement:** the mask must derive from the same MVCC
snapshot as the traversal, enforced by the type system — the mask travels
with its CSN and the engine rejects any search whose filter does not match
the traversal snapshot. The compiler prevents it, not code review.

## Phase 2 — SIMD and index placement

- **SIMD kernels** for dot product and Euclidean distance: AVX-512
  (x86_64) and NEON (ARM64), scalar fallback, runtime detection. With a
  fixed reduction order this is deterministic and reproducible, so it is
  legitimate acceleration *inside* the authority path — the highest
  benefit-to-risk item on this roadmap, because distance computation is
  scalar today.
- **The index lives in `hyphae-native-pages`.** Graph persistence uses the
  existing page cache, never a parallel mmap regime: two durability and
  crash-consistency stories would break `doctor`'s ability to speak for the
  whole directory and would split backup/restore.
- **Quantization is opt-in, never default.** The measured, signed
  `recall@10 = 1.0` across 33/33 cells is a demonstrable property; it is
  not traded for an unmeasured memory gain. Any quantization experiment
  pre-registers its recall-loss threshold per dataset before results are
  observed.

## Phase 3 — Optional heterogeneous acceleration

**A user with a GPU must be able to use it. A user without one must not
notice it exists.**

1. **Accelerated index construction** — the biggest win and safest case.
   The build is not deterministic; the artifact is identifiable (BLAKE3,
   anchored to the manifest — `HnswGenerationDescriptor` and
   `IndexSnapshot` already carry canonical hashes), and search over it is
   reproducible. Validated precedent: cuVS/CAGRA, Milvus GPU indexes.
2. **Accelerated exact search — the competitive argument.** Brute force is
   embarrassingly parallel; with a GPU, exact search holds to scales where
   the sector is forced to approximate. The pitch stops being "our ANN is
   faster" and becomes: **"At your scale, Hyphae does not approximate. And
   when it does, it tells you."** The second half is `AnnRecallRisk`,
   already implemented. The GPU feeds the candidate stage; `exact_rerank`
   does what it already does; results are bit-identical to pure CPU.
3. **Bulk ingest, compaction, rebuild** — batch, offline, zero risk to the
   commit path.

| Profile | Backend | Status |
|---|---|---|
| CPU (x86_64, ARM64) | Native SIMD | Authoritative, default |
| Consumer GPU (Apple Silicon, RTX, Radeon) | WGPU (Metal/Vulkan/DX12) | Optional, non-authoritative |
| Datacenter GPU (B300, H100, MI300X) | CUDA / ROCm-HIP | Optional, non-authoritative |

WGPU first: it covers "I have a GPU and want to use it", and is
MIT/Apache-2.0, already admitted by `deny.toml`. CUDA carries a proprietary
EULA — review before distributing any CUDA-linked binary.

## Phase 4 — Attested embeddings and Proof of Retrieval

- **`hyphae-embed`**: an optional crate and binary, outside the default
  dependency graph, delivering vectors through the door the product
  boundary already declares ("semantic providers can supply vectors to the
  Rust APIs but never become a core dependency or source of authority").
  Separation is what makes attestation meaningful: inside the engine, the
  signature would be the system certifying itself; outside, it is a chain
  of custody with two distinguishable parties. This is also where a GPU
  genuinely earns its keep — inference.
- **Attestation**: per produced vector, the embedder signs the model and
  weight hashes, inference version and configuration, input-text hash,
  output-tensor hash, device, and driver — all BLAKE3.
- **Proof of Retrieval**: a retrieval proof that certifies not only *what*
  was returned over *which* snapshot, but *with which model, version, and
  weights* each involved vector was generated — and, through
  `AnnRecallRisk`, what recall guarantee the query carried. End-to-end
  forensic audit for agents in regulated industries: the one piece of this
  roadmap nobody else is building.

## Phase 5 — Distribution

- Operator console as the demonstration asset: documented, with captures,
  in the README and on `hyphae.dev`.
- The agent plugin is a distribution channel, not only a feature: a
  developer who installs it in Claude Code or Codex tries Hyphae without
  writing Rust.
- RRF as a scoring option over the existing hybrid search.
- Evaluate an in-process PyO3 binding as an addition to `hyphae-sdk`.
- A public reproducible benchmark against the comparison set below.

## Accelerated-backend contract

```rust
/// Backend identity, recorded in the receipt of every operation that used it.
pub struct DeviceIdentity {
    pub name: String,
    pub driver_version: String,
    pub kernel_version: String,
    pub authoritative: bool,
}

/// Candidate producer. Does NOT define results. Exact rescoring, stable-ID
/// tie-breaking, and AnnRecallRisk qualification remain in the engine.
pub trait CandidateSource: Send + Sync {
    fn identity(&self) -> &DeviceIdentity;

    /// Returns a superset of size k * overfetch. May be non-deterministic.
    fn candidates(
        &self,
        query: &[f32],
        k: usize,
        overfetch: u32,
        metric: Metric,
        eligible: Option<&SnapshotMask<'_>>,
    ) -> Result<Vec<u64>, AccelError>;
}
```

Deliberate decisions: no `async_trait` (boxing on every call is visible
against a 1.595 µs BM25; asynchrony lives in the daemon); no `&mut self`
(mutable exclusivity on a concurrent read server conflicts with MVCC
snapshot semantics; ingest goes through transactional authority); it
returns IDs, not results (the type makes evident it is not an answer); no
embedding generation (that is `hyphae-embed`).

## G9 — the accelerated-subsystem gate

> **Every accelerated result must be bit-identical to its CPU equivalent
> over the same snapshot. A backend that cannot satisfy this is marked
> non-authoritative, stays outside the proof path, and can never be a
> default.**

| Control | Verification |
|---|---|
| G9-C1 | Bit-for-bit CPU/accelerator equivalence over the complete conformance corpus |
| G9-C2 | The candidate superset contains the exact top-k in 100% of measured cases, for every supported over-fetch factor |
| G9-C3 | Accelerator-built index artifacts are BLAKE3-hashed and manifest-anchored; search over them is reproducible |
| G9-C4 | `doctor` reports the active backend, device, driver, and kernel version |
| G9-C5 | Every receipt of an accelerated operation records `DeviceIdentity` |
| G9-C6 | With the backend absent or failed, the system degrades to CPU with no result change |
| G9-C7 | The default binary of the four signed targets links no accelerator dependency |
| G9-C8 | `AnnRecallRisk` stays exact under acceleration: a result qualified `ExactFilteredCandidates` truly is |
| G9-C9 | Every gate path — including the live publication and verification paths the gate itself depends on — has executed at least once against real external systems before closure |

G9-C9 exists because the first live runs of the registry-publication gates
exposed three defects that unit tests could not catch (the dirty-marker
expectation, the download accept header, and the build-directory marker):
paths that had never run against the real registry. A gate whose own
enforcement path is unexercised certifies nothing.

Like G7, the G9 closure must declare its non-claims: portable latency
across devices, behavior under VRAM contention with other processes, and
equivalence across untested driver versions.

## Packaging and licensing

- Separate crates `hyphae-accel-wgpu`, `hyphae-accel-cuda`,
  `hyphae-accel-rocm`, never in the default graph (G9-C7 by construction).
  New crates enter the publication layers of
  `config/crates-io-release.json`, and every checker pin they touch is
  tag-bound control code — budget one release iteration for gate changes.
- The four signed targets stay identical with reproducibility intact;
  accelerator builds are additional artifacts with their own receipts.
- `deny.toml` verified: the allow list admits Apache-2.0, MIT, BSD-2/3,
  ISC, MPL-2.0, 0BSD, Zlib, Unicode-3.0, CDLA-Permissive-2.0, NCSA — no
  strong copyleft in the graph. WGPU and `roaring` enter without touching
  policy. CUDA's proprietary EULA is reviewed before distributing any
  linked binary.

## Competitive positioning

Primary set — where Hyphae actually competes: SQLite + sqlite-vec,
DuckDB + VSS, LanceDB, redb, sled, libSQL. Secondary set — Weaviate,
Milvus, Qdrant: a category argument, not a benchmark one; they are
distributed, multi-tenant, managed systems that the product boundary
deliberately excludes.

| Axis | Claim |
|---|---|
| Exactness at scale | We do not approximate at your operating scale — `recall@10 = 1.0` measured in 33/33 cells |
| Recall honesty | `AnnRecallRisk` qualifies every query; nobody else does |
| Complex filtering | Eligibility inside the traversal over one snapshot, not decoupled indexes |
| Provenance | Proof of Retrieval with model attestation |
| Operational footprint | One binary, no orchestration, no GC |
| Release verifiability | 24 signed crates, SBOMs, provenance, attestations, exact-SHA receipts |

## Risks and non-claims

- **"Sub-millisecond" is forbidden.** The published G7 evidence says
  otherwise on several surfaces; contradicting the project's own signed
  evidence destroys the only irreproducible asset it has.
- Determinism of the authority path is the hard constraint. If G9 cannot
  close under its criterion, accelerated backends stay non-authoritative
  and remain useful. The criterion is not relaxed.
- Quantization trades a measured, published property for an unmeasured
  gain: opt-in with pre-registration only.
- Phase 0 may reveal that embedded contention is structural rather than a
  single lock; scope then grows and everything shifts. Better discovered
  now than after building three phases on top.
- This roadmap does not include replication, clustering, hosting, or
  multitenancy.

## Decision sequence

| # | Decision | Blocks |
|---|---|---|
| 1 | Profile embedded contention on `1.2.2` (Phase 0) | Everything technical |
| 2 | `Filtered ANN` baseline at C1/C8/C32 | Phase 1 |
| 3 | Snapshot-bound mask contract | Phase 1 |
| 4 | SIMD before any accelerator | Phase 2, and the Phase 3 baseline |
| 5 | Write the G9 criterion before the first kernel | Phase 3 |
| 6 | Embedding attestation format | Phase 4 |
| 7 | CUDA license review | Phase 3 |
