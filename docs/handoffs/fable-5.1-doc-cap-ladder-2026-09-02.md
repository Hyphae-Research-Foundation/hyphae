# Fable 5.1 handoff — Hyphae engine, doc-cap ladder, and Studio

Status: current handoff as of 2026-09-02. The previous handoff
([`fable-5-native-ecosystem-2026-08-02.md`](fable-5-native-ecosystem-2026-08-02.md))
is superseded historical context; do not use its gate table.

Audience: Fable 5.1 (the next Claude session on this machine) and any
maintainer picking up the engine or Studio.

This document has four parts: what Hyphae is, the exact state of the work,
how to operate on it, and what is still open. Read all four before writing
code.

---

## Part 1 — What Hyphae is

### One sentence

Hyphae is a local-first data engine in Rust: one binary, one data directory,
three first-class native engines (relational/SQL, keyspace/data structures,
lexical + vector search) over one shared transactional substrate (catalog,
types, page/blob allocator, WAL, MVCC/CSN, scheduling, memory budget,
recovery, backup, proofs). Tagline: *Data that can prove itself.*

### What it is not

- Not a wrapper. It does not embed PostgreSQL, Valkey, OpenSearch, or any
  other database or search engine. General-purpose audited primitives are
  allowed; another engine as an internal runtime or sidecar is not.
- Not cloud-dependent. Runs offline. No embedding provider, reranker, or LLM
  is required for ordinary operation; optional model adapters consume public
  contracts and never become storage authority.
- Not a hosted product. PliegoRS, Mycelium, Hyphae Network, Celiums Network,
  billing, SaaS, and cloud operations are out of this repo.

### Why it exists (product thesis)

Applications combine a relational DB, a structure/cache server, and a search
service, then pay for it with CDC pipelines, dual writes, cache invalidation,
async search refresh, three schemas, and three backup/auth/memory policies.
Hyphae removes those boundaries inside one process: a committed all-engine
write has one visible CSN on every surface, readers never mix root
generations, and every result can carry a proof bound to the exact snapshot.
See [`docs/architecture/native-local-ecosystem.md`](../architecture/native-local-ecosystem.md).

### Surfaces

- **Embedded Rust** (`hyphae-native-product::NativeProduct`) — the primary
  performance surface.
- **Native local protocol** over UDS (`hyphae-native-protocol`,
  `hyphae-native-daemon`) — typed binary frames, versioned minor levels
  (currently minor 6). No internal engine-to-engine path may use TCP, HTTP,
  JSON, or a serialized compatibility protocol.
- **CLI** (`hyphae-cli`, the only executable) — `init`, `serve`, `sql`,
  `structure`, `search`, `catalog`, `doctor`, `hardware`, `proof`, `mcp`.
- **HTTP `/v2`** served by `hyphae serve`, consumed by SDKs.
- **TypeScript SDK** at `sdks/typescript` (v2 protocol codec mirrors the
  Rust codec; 49 tests).
- **MCP** (`hyphae mcp`) including the five-verb Agent Memory.

### Crate graph (24 crates, `crates/`)

Native core: `hyphae-native-types`, `-catalog`, `-records`, `-pages`
(page store + verified buffer pool), `-btree` (copy-on-write B+tree),
`-blobs`, `-wal`, `-mvcc` (root sets, CSN, coordinator), `-manifest`
(checkpoints/root manifests), `-ann` (HNSW + SQ8), `-runtime` (the engine:
`NativeDatabase`, transactions, delta batches, recovery, scorer, SQL
executor; ~63k lines in `src/lib.rs`), `-product` (`NativeProduct`: the
curated public model — search collections, doc values, proofs, access
control, admin), `-protocol`, `-daemon`, `hyphae-cli`.
Legacy/format-2 line (frozen compatibility): `hyphae-core`, `-engine`,
`-query`, `-retrieval`, `-server`, `-storage`, `-client`, `-contracts`.

### Engineering rules (from `AGENTS.md`, non-negotiable)

- English for code, contracts, commits, docs. `unsafe` forbidden.
- **Contract-first**: public behavior changes start in `docs/native/*.md`.
- **Fail-closed, everything bounded**: every limit is a named constant with
  a stable `ProductErrorCode`.
- Failure-path tests for durable behavior. No `expect()` even in tests
  (use `?`). Clippy `-D warnings` with pedantic lints; frequent ones:
  `too_many_lines` (100), `too_many_arguments` (7), `option_option`,
  `cast_precision_loss`, `large_enum_variant`, `items_after_statements`,
  `unchecked_time_subtraction`, `map_unwrap_or`.
- Never claim a roadmap phase complete without exit evidence. Never add an
  automation attribution trailer to a commit.
- Historical repositories are frozen inputs; ports need an accepted entry in
  `docs/porting/ledger.md`.
- "Microsecond-first" is a measured hot-path objective; report transport,
  execution, queueing, and physical durability separately.

### Evidence culture

Every performance number lives in a receipt with host, commit, protocol, raw
observations, and boundaries: `docs/gates/evidence/`. The claims ledger is
[`docs/retrieval/claims-protocol.md`](../retrieval/claims-protocol.md); the
wording authority is [`docs/product/claims.md`](../product/claims.md); the
gate status authority is [`docs/gates/native-gate-status.md`](../gates/native-gate-status.md)
(G0–G8 closed for bounded profiles). The research foundation's publication
policy (record states Hypothesis → Prototype → Measured → Reproduced →
Stable) is in `~/Documents/hyphae-research/docs/RESEARCH-PUBLICATION-POLICY.md`.

---

## Part 2 — Exact state of the work

### Source control

| Field | Value |
|---|---|
| Repository | `Hyphae-Research-Foundation/hyphae` (local: `~/Documents/hyphae`) |
| Branch | `feat/sql-slice-2-and-evidence` |
| HEAD | `1b2cc70` at hand-off; `bda7e28` (this document), then the three 2026-09-03 commits in the addendum below |
| Base | `17a841d` (41 commits on top at hand-off; 45 after the addendum) |
| Pushed | **No.** Nothing on this branch is on the remote. |
| Worktree | clean |
| Suite | 1,753 tests green on the devbox; `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` clean |
| Latest published release | Native `2.2.0` on crates.io |

### The session's directive

The user's order: "vitaminar el engine" for a Thursday meeting — SQL
PostgreSQL-affinity → KV Valkey-parity → lexical/ANN with reverse
engineering of Weaviate and OpenSearch → then the collection document-cap
ladder. "No medias tintas." Studio is paused until the engine work closes.

### What the 41 commits contain (oldest → newest, grouped)

**SQL / KV (earlier in the branch):** SQL slice 2 (HAVING, grouped ORDER BY,
DISTINCT, OFFSET, BETWEEN), KV-2/KV-2b (key scan across families,
ZINCRBY/ZPOP, SETNX/SET-IF-PRESENT, APPEND, SETRANGE, GETRANGE, HSETNX,
seeded SPOP/SRANDMEMBER via `splitmix64`). Wire minor 6 tags are listed in
the session summary at the top of this file's git history if needed; the
authority is `docs/native/local-protocol-v1.md`.

**ANN:** diversity heuristic (HNSW Algorithm 4 + keepPruned backfill, build
identity v2), SQ8 quantizer (`crates/hyphae-native-ann/src/sq8.rs`, not yet
in the durable format).

**Search features (product + wire + proofs + SDK + CLI + MCP):**
relative-score fusion, autocut, float doc values, offset, Average
aggregation, range facets, vector `max_distance`, lexical AND / OR with
minimum-match, prefix, BM25F field boosts, fuzzy, highlights, phrase.

**Doc-cap ladder (this session, the last 9 commits):**

| Commit | Change | Receipt (DigitalOcean c-16, release) |
|---|---|---|
| `93dc3d3` | Point-resolved batch ingest: idempotency/binding/manifest by durable point reads; vector-less batches stage through the physical delta batch; `NativeDatabase::snapshot_identity` / `next_transaction_id`; runs of persistent scalar `SET`s coalesce into one `upsert_sorted_batch`; root construction probes through the buffer pool | ingest 48 → 1,261 docs/s at 100k; per-batch commit 780 → 97 ms |
| `5d42cd4` | `collection_scale_evidence` runs `vacuum_pages` + `checkpoint` + `retain_wal` after load (`HYPHAE_SCALE_SKIP_MAINTENANCE=1` to skip); `HYPHAE_SCALE_APPEND=<n>` | directory 1.9 GB → 385 MB at 100k |
| `0c5040b` | Open decodes complete state **once** (the root that becomes current); every retained superseded root is verified structurally (`validate_root_structure`) | unmaintained 100k reopen ~17 min → 2 min; maintained 12.8 s |
| `a0e73ea` | Durable scorer: `BorrowedLeaf` in `hyphae-native-btree` (full leaf verification, borrowed entries), `visit_planned_segment_cached`, planning on boundary keys, `LexicalPostingBatch` id arena, ranking on offsets | scorer 47 → 7.9 ms (100k), 112 → 13 ms (250k) |
| `c139b2d` | **`MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` 100_000 → 250_000**; evidence doc; bound unit test; roadmap R5; claims ledger; `scale_stage_diagnostic` fails on model/durable divergence | model/durable hits **bit-identical** at 100k (model 335 s) and 250k (model 2,246 s) |
| `c783e2c` | Eligibility visits posting keys in place (`visit_structure_keys_in_range`), sorted bulk set builds; manifest decode verifies ascending order | range+facet 121 → 46 ms at 250k |
| `ed3873b`, `9fd0965` | Ladder receipts recorded in harness header and evidence doc | |
| `1b2cc70` | Prefix/fuzzy dictionary walk via `visit_prefix_cached` with borrowed leaves; reusable `BoundedLevenshtein` workspace; byte-length pre-filter | fuzzy(1) 80 → 19 ms (100k), 211 → 61 ms (250k) |

**Final ladder (p50, shipped bound, reopened corpora):**

| rung | ingest docs/s | reopen | bm25 | filtered+facet | phrase | fuzzy |
|------|---------------|--------|------|----------------|--------|-------|
| 100k | 1,261 | 12.8 s | 16 ms | 20 ms | 22 ms | 19 ms |
| 250k | 949 | 36 s | 39 ms | 46 ms | 42 ms | 61 ms |

Session start baseline at 100k was 48 docs/s, bm25 73 ms, filtered 108 ms,
phrase 97 ms, fuzzy 194 ms.

Evidence document: [`docs/gates/evidence/collection-cap-250k-2026-09-02.md`](../gates/evidence/collection-cap-250k-2026-09-02.md).

### Contracts touched this session

- `docs/native/search-document-lifecycle-v1.md` — "Product batch ingest"
  subsection (cost model, delta vs materialized path, coalesced scalar runs,
  manifest note) and two new verification gates.
- `docs/native/root-manifest-checkpoint-v1.md` — two root-validation depths
  at open.
- `docs/native/search-semantics-v1.md` — borrowed-scan cost model for the
  durable scorer.
- `docs/roadmaps/rag-competitive-roadmap.md` R5, `docs/retrieval/claims-protocol.md`.

### Key code locations

- `crates/hyphae-native-product/src/search.rs`: `ingest_search_batch`,
  `ingest_search_batch_delta`, `ingest_search_batch_materialized`,
  `ingest_manifest`, `IngestPlan`, `structure_point_read`,
  `snapshot_identity_bounded`, `posting_scan`, `posting_filter_ids`,
  `decode_manifest`, `expand_fuzzy_query`, `#[cfg(test)] mod tests`
  (bound test), `MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` (line ~55).
- `crates/hyphae-native-runtime/src/lib.rs`: `snapshot_identity`,
  `next_transaction_id`, `recover_committed_roots`, `validate_roots`,
  `validate_root_structure`, `structure_tree_after_mutations`,
  `coalescable_scalar_set`, `upsert_scalar_set_run`,
  `ensure_scalar_key_has_no_live_collection`,
  `index_documents_batch_in_search_tree`, `decode_lexical_segment`,
  `LexicalPostingBatch`, `finalize_lexical_batches`,
  `search_expand_term_{prefix,fuzzy}_at_snapshot`, `BoundedLevenshtein`,
  `visit_structure_keys_in_range`, `THREAD_FULL_STATE_LOADS` (test).
- `crates/hyphae-native-btree/src/lib.rs`: `BorrowedLeaf`,
  `visit_planned_segment_cached`, `plan_range_segments_node_cached`,
  `visit_prefix_range_node_cached`.
- `crates/hyphae-native-runtime/src/model.rs`: `visit_visible_keys_in_range`.
- Harnesses: `crates/hyphae-native-product/examples/collection_scale_evidence.rs`
  and `scale_stage_diagnostic.rs` (env: `HYPHAE_DIAG_SKIP_MODEL`,
  `HYPHAE_DIAG_SCORER_ROUNDS`, `HYPHAE_DIAG_RANGE_ROUNDS`,
  `HYPHAE_DIAG_FUZZY_ROUNDS`).
- Tests added: `crates/hyphae-native-product/tests/search_ingest_delta.rs`
  (own binary: process-wide counter), runtime lib tests
  `coalesced_scalar_set_run_matches_sequential_v2_semantics`,
  `open_decodes_complete_state_once_and_still_verifies_superseded_roots`,
  batch-merge arena ranking in
  `lexical_batch_merge_preserves_order_and_fails_closed_on_cardinality`.

---

## Part 3 — How to operate

### Build/test only on the devbox

Local `/tmp` is a 16 GB tmpfs; the full suite fails locally with
`Disk quota exceeded`. Write code locally, run `cargo fmt --all` locally,
then compile/test remotely.

- Devbox: DigitalOcean droplet `hyphae-devbox`, c-16 (16 vCPU Xeon 8168,
  32 GB, 200 GB), `198.199.77.236`, id `596919933`, **$0.50/h, still
  running**. SSH key `~/.ssh/celiums-workers`.
- `tools/devbox.sh [cmd]` rsyncs the tree (excluding `target`, `.git`,
  node_modules, dist) to `/workspace/hyphae/` and runs `cmd` with
  `~/.cargo/env` sourced. **Raw `ssh` calls must `source ~/.cargo/env`**
  or `cargo` is not found (this bit us once: a "rebuilt" binary was stale).
- Full gate (~18 min):
  ```
  tools/devbox.sh "cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings 2>&1 | grep -cE '^error'; cargo test --workspace --all-features --locked 2>&1 | grep -E 'test result: (ok|FAILED)' | awk '{p+=\$4; f+=\$6} END {print \"passed:\", p, \"failed:\", f}'"
  ```
- Evidence corpora on the devbox: `/root/ladder-100000` (385 MB),
  `/root/ladder-250000` (2.1 GB) — maintained, reopen in 13 s / 36 s.
  Older: `/root/scale-10k`, `/root/scale-100k-v5`. Disk is 83% full;
  delete `scale-*` dirs before creating new corpora.
- `perf`: `/usr/lib/linux-tools/6.8.0-124-generic/perf`. Release strips
  symbols; build with `CARGO_PROFILE_RELEASE_STRIP=none
  CARGO_PROFILE_RELEASE_DEBUG=1`. Use `--call-graph dwarf` — frame pointers
  lose callers through libc. Attach after `stage=open` finishes; the
  scripts `/root/perfq4.sh`, `/root/perff.sh`, `/root/perfz.sh` do this.
- Zombie diagnostics leave `DataDirectoryLocked`: `pkill -f
  scale_stage_diagnostic`.
- Delete when done: `doctl compute droplet delete hyphae-devbox`.

### Process per slice

Contract (`docs/native/*.md`) → implementation → happy + failure tests →
`cargo fmt --all` → clippy `-D warnings` → full suite → one thematic commit
with receipts in the message. Commit messages in the repo's style: one-line
imperative subject with area prefix (`Search:`, `Runtime:`), body with the
why, the what, and the measured numbers.

### Operational warnings learned the hard way

- Python `str.replace` edits can fail silently if `rustfmt` reformatted the
  target; always `assert old in s` and verify with `rg` afterwards.
- Never `git checkout -- <file>` over uncommitted work (it deleted a hook
  once). Never delete `crates/hyphae-native-runtime/examples/`.
- Regex "add a field to every struct literal" leaked into nested enums; use
  a brace-balanced parser per literal.
- Process-wide counters (`FULL_STATE_LOADS`) are inflated by parallel tests
  in the same binary; use a thread-local (`THREAD_FULL_STATE_LOADS`) or a
  single-test binary.
- A `Result<ControlFlow>` returned from a visitor must be bound
  (`let _flow = …`) or clippy rejects it.
- There are user `hyphae serve` processes on this machine (Studio demo); do
  not kill them.

### Running the engine locally

```
cargo build --locked -p hyphae-cli            # → target/debug/hyphae
hyphae init --data-dir <new-dir>              # dir must not exist
hyphae sql|structure|search|doctor --data-dir <dir> …
hyphae serve --data-dir <dir>                 # only then does a listener start
```
See `docs/quickstart-native.md`.

---

## Addendum — 2026-09-03 session (Fable 5.1)

Three commits on top of `bda7e28`, all gate-verified on the devbox (fmt,
clippy `-D warnings`, 1,772 tests):

| Commit | Change | Receipt (c-16, release) |
|---|---|---|
| `92f3c7a` | **Chunked manifest**: `HYPSMAN2` header + `HYPSCHK1` 16 KB chunks (`crates/hyphae-native-product/src/search_manifest.rs`); inserts stage only `SET`s (sentinel floor 0, midpoint split), deletes merge/drop; one `ManifestState` for delta, materialized, and operation-batch writers; `HYPSMAN1` readable forever, upgraded on the first accepted mutation; contract section in `search-document-lifecycle-v1.md`; 19 tests | 250k: ingest 949 → 1,099 docs/s, directory after maintenance 2.1 GB → 232 MB, reopen 35.6 → 24.8 s; legacy 100k upgrades in 0.8 s |
| `51d8fb0` | **Lazy eligibility**: `Eligibility::Universe(&ManifestView)` probes the owning chunk instead of cloning the manifest; `All` seeds from its first narrowing child; vector branches materialize once | 250k reopened: bm25 39 → 24 ms, phrase 44 → 29, filtered+facet 44 → 39, fuzzy 63 → 46; sparse query 16 → 0.4 ms |
| (this commit) | 1M ladder receipt, `HYPHAE_SCALE_MAINTENANCE_EVERY`, `HYPHAE_DIAG_MODEL_ROUNDS`, roadmap/claims/handoff updates | 1M: 1,014 docs/s, reopen 107 s, bm25 172 ms, filtered+facet 233, phrase 175, fuzzy 308; manifest 1,952 chunks / 39 KB header / 17 µs probe |

| `6de0d7d` | `run-metal.sh` never formats a partitioned or pre-mounted disk (found orchestrating the bare-metal run) | — |
| `b53348e` | **B+tree fix**: `upsert_sorted_batch` split an overflowing rewritten leaf full-plus-remainder, degenerating to one leaf per key under random single-key upserts (every scalar SET since `93dc3d3`); now splits evenly; occupancy test (904 → ≤ 21 leaves for 4,000 keys) | devbox ablation, materialized single SET: 86.6 / 81.7 ms → 13.9 / 10.5 ms (Strict / Memory), back to the `8aeb6ea` level |

Evidence: [`collection-manifest-chunked-1m-ladder-2026-09-03.md`](../gates/evidence/collection-manifest-chunked-1m-ladder-2026-09-03.md) (c-16) and
[`baseline-i7i-metal-2026-09-03.md`](../gates/evidence/baseline-i7i-metal-2026-09-03.md) — the **class-3 bare-metal re-measurement of `2ff8a4b`** on an
`i7i.metal-24xl` (04:15–08:27 UTC, self-terminating runbook, raw outputs mirrored to
`s3://hyphae-metal-receipts-598621/2ff8a4b-20260903T0415Z/`): TLC reproduced with spec
digest; lexical query 4.09 ms → 255 µs and ingest 16.6 → 2.19 s per 1,000 docs vs
2026-08-30; delta sweeps flat across version depth (194–197 µs); ladder 250k bm25 6.2 ms /
reopen 7.7 s, 1M bm25 51.6 ms / reopen 34.5 s / ingest 3,755 docs/s; **1M scorer equivalence
`bit_identical=true`** (model 3.5 h vs durable 57 ms); scorer `perf`: 44 % of the 1M scorer is
page verification (BLAKE3 + CRC32C) — the 1,024-frame buffer pool does not keep the 1,562
posting segments resident. The receipt also publishes the materialized-path regression
(durability ablation 4.98 → 35.5 ms) that the bisect traced to `93dc3d3` and `b53348e` fixes;
the metal numbers above were taken **before** the fix, so reopen and materialized-path rows
are pessimistic and the ladder should be re-measured at `b53348e` or later.

**Second bare-metal run at `a443c52`** (3.0.0 tree: B+tree fix + 8,192-frame pool),
[`hyphae-3.0-metal-a443c52-2026-09-03.md`](../gates/evidence/hyphae-3.0-metal-a443c52-2026-09-03.md):
materialized single-SET commit 4.39 / 3.86 ms (regression closed, under 2.2.0); SQL point
read 20 µs; keyspace GET 2.2 µs; BM25 top-10 111 µs / ingest 1.12 s; 1M ladder linear
(bm25 23 ms, 3.7× the 250k stage); same-directory pool comparison 1,024 → 8,192 frames:
scorer 51 → 22 ms; page verification gone from the scorer profile. Reopen unchanged on
metal (34.6 s at 1M): open time is the next open-path item, not a layout effect.

**The bound stays at 250,000.** R5 gates the 1M rung on the manifest (now
met) *and* on ANN consolidation cost and RSS at 1M×768-dim, which cannot be
measured until vector-carrying batches leave the materialized path (item 2
below). The 1M lexical ladder is also ~6–7× the 250k ladder for 4× the
documents, and the manifest is no longer why: the durable scorer spends
~430 ns per posting entry at 1M against ~235 ns at 250k (1,562 segments,
372k entries for "database engine"). Decision for the user: raise to an
intermediate rung on this receipt, wait for the vector receipt, or amend R5.

Devbox state after the session: corpora `/root/ladder2-250000` (chunked,
232 MB), `/root/ladder-1000000` (chunked, 995 MB), `/root/ladder-100000-upgrade`
(legacy copy upgraded); the originals `/root/ladder-100000` and
`/root/ladder-250000` are untouched (still `HYPSMAN1`). The 1M model/durable
scorer equivalence run was started detached (`/root/equiv-1000000.log`);
append its result to the receipt. Deleting `/root/scale-*` (41 GB) needs the
user's explicit approval; the auto-mode classifier blocks it.

### Commit identities after the DCO sign-off rewrite (2026-09-03)

Before the merge the whole branch was rewritten with `git rebase --signoff`
onto `origin/main` (`8aeb6ea`). Every tree is byte-identical; only the
commit ids changed. Receipts and this document cite the ids as measured;
the rewritten ids are:

| cited | rewritten | tree |
|---|---|---|
| `17a841d` | `ddc1668` | `c6cd73b` |
| `eec0784` | `786083b` | `7fbac6e` |
| `93dc3d3` | `f36f7f1` | `64ee8bd` |
| `0c5040b` | `347d919` | `62455b6` |
| `c139b2d` | `9eccfbd` | `3c3941e` |
| `1b2cc70` | `650e0ab` | `ee05374` |
| `bda7e28` | `089c030` | `2d32220` |
| `92f3c7a` | `d9b1f76` | `e1e28e8` |
| `51d8fb0` | `6bbcf2e` | `45b2f13` |
| `2ff8a4b` | `a18d3f2` | `ccaa229` |
| `6de0d7d` | `10a2a11` | `5af52de` |
| `b53348e` | `6df81f2` | `b0297b8` |
| `f07ee6a` | `3208706` | `b059bbc` |
| `d1e6309` | `f4a2f48` | `517aa29` |
| `2e7c64d` | `645a359` | `7369e4c` |
| `a443c52` | `0295656` | `0eadc82` |
| `4590eb9` | `3cfc74f` | `dc77bd3` |

The backup ref `backup/feat-sql-slice-2-before-signoff` holds the pre-rewrite history locally.

## Part 4 — Open items and next moves

### Engine, in priority order

1. ~~Manifest for the 1M rung~~ — done (`92f3c7a`, `51d8fb0`). Next
   manifest-adjacent item: sequential inserts fill chunks to 50–100 % via
   midpoint splits (1,952 chunks at 1M instead of ~977); an append-aware
   split would halve the header, at the cost of a different invariant proof.
1b. **Durable scorer superlinearity at 1M** — profiled on bare metal:
   44 % of scorer time is page verification (`_blake3_hash_many_avx512`
   17 %, `_blake3_compress_in_place_avx512` 11 %, CRC32C 16 %) plus kernel
   file reads: the shipped 1,024-frame verified buffer pool (16 MiB) cannot
   hold the 1,562 posting segments of a two-term query at 1M, so every query
   re-reads and re-verifies them. **Done in part** (the buffer-pool commit): the default
   pool bound is now 8,192 frames (128 MiB ceiling, lazy), measured on the
   devbox at 1M: scorer 135–146 → 61–69 ms, fuzzy 266 → 159 ms; 65,536
   frames buys nothing more. Still open: deriving the bound from the memory
   governor instead of a constant, and item 3 (the parallel scorer never
   activates).
   Also cheap: `drop_in_place<NativeRuntimeError>` at 2.8 % is the fail-open
   probe constructing errors on the hot path.
1c. **Re-measure at `b53348e`**: the B+tree split fix changes the physical
   layout of every scalar-SET tree (manifest chunks, doc-value postings,
   keyspace); reopen, open-time and materialized-path numbers in both
   2026-09-03 receipts predate it and are pessimistic.
2. **Vector-carrying batches** still take the materialized ingest
   transaction (`ingest_search_batch_materialized`) because the ANN store has
   no delta stage. The ladder corpus has no vectors; that cost is unmeasured.
3. **Parallel scorer path never activates** in product/CLI
   (`scan_lexical_segments_parallel`; receipts show `workers=1 batches=0`)
   because nobody installs governor + execution pool
   (`hardware calibrate` → `NativeGovernorPolicy::derive` →
   `set_resource_governor_with_execution_pool`).
4. **SQ8 rescore limit 20** (Weaviate default) not yet applied; SQ8 not in
   the durable format.
5. **Fuzzy scales with dictionary size** (one unique term per doc in the
   synthetic corpus). A natural-language corpus (FiQA) at 250k would be the
   honest re-measurement.
6. **Candidate issues not yet filed:** clap flag ordering in `security
   bootstrap`; `proof_generate` reports "corruption" over served
   transports; `create-search-collection` unmanaged-only after bootstrap;
   `hyphae-bitnet` file_type 40 vs 41 and stop sequences.
7. **Push and PR** `feat/sql-slice-2-and-evidence` when the user says so.
   Review all 41 commits, not just the last.

### Weaviate/OpenSearch extraction (for future slices)

HNSW defaults M=32, M0=64, ef_construction=128, dynamic ef 100/500/8,
flatSearchCutoff 40k, ACORN ratio 0.4, tombstone cleanup 300 s;
BlockMax-WAND blocks of 128 docs; SQ formula (a2/ab/ib2) already ported to
`sq8.rs`. The Weaviate clone was deleted from `/tmp/opencode`.

---

## Part 5 — Hyphae Studio (the frontend)

### Where

`~/Documents/hyphae-studio` — GitHub
`Hyphae-Research-Foundation/hyphae-studio` (**private** until the user
decides otherwise). `main` at `2d76747` — *Ask Hyphae: teach HAVING, grouped
ORDER BY, DISTINCT, OFFSET, BETWEEN*.

### What it is for

Studio is the official graphical instrument for Hyphae: SQL, keyspace,
search, agent memory, proofs, and natural language over one verifiable
engine, with the engine's own evidence (CSNs, commit receipts, digests)
rendered as first-class UI. It is a Next.js app that talks to a locally
served `hyphae` sidecar through the official TypeScript SDK
(`../hyphae/sdks/typescript`, linked as a local dependency). It adds
multi-user sign-in on top of Hyphae's native access control: every Studio
account is bound to a Hyphae principal and its own `hyp1_…` API key, so the
engine — not the GUI — is the authority on what each user may do.

Sections (`src/app/(studio)/`): `ask` (natural language → engine
operations; taught the SQL slice-2 syntax), `catalog`, `composer`,
`keyspace`, `memory`, `operations`, `proofs`, `search`, `security`,
`settings`, `sql`, plus `bootstrap` and `login`. Phases 1–6 are done;
**phase 7 (Composer / Proofs / Operations) is pending**, as are UI i18n
and publication. Studio work is paused until the engine slice closes.

Studio consumes only public versioned contracts (SDK v2 codec, HTTP `/v2`,
`ProductErrorCode` registry). When the engine adds a wire feature, the SDK
encoder/decoder in `sdks/typescript/src/v2/protocol.ts` and then Studio's
`ask` grammar follow; never the reverse.

### Demo environment

`~/hyphae-studio-demo/`: `rebuild.sh` recreates the data dir in the required
order (init → collections + provision, unmanaged → bootstrap →
principals/keys). Keys: `owner.key`, `service.key`, `terrizo.key`. Ports:
sidecar `:8790`, Studio `:3123`, BitNet `:8793`, Ollama `:11434`. Do not
commit data dirs.

---

## Part 6 — Research foundation and a possible paper

`~/Documents/hyphae-research` is the Hyphae Research Foundation repository
(governance, publication policy, adoption docs, asset-transfer register,
launch readiness). Its `RESEARCH-PUBLICATION-POLICY.md` defines the record
states and the minimum benchmark record; every number in this handoff is at
best *Measured* (one host, recorded protocol), not *Reproduced*.

**A paper is plausible and the material for it exists.** The distinctive,
defensible claims are:

1. Three native engines over one CSN with one WAL and one recovery, where a
   cross-engine transaction is a single commit, not a saga.
2. Verifiable results: proofs bound to an exact immutable root set and
   snapshot logical time, verifiable offline (`docs/native/*proof*`,
   `hyphae-native-product/src/proof/`), with a formal commit model in
   `docs/formal/HyphaeCommit.tla`.
3. Bit-identical durable-vs-reference scoring: the physical posting scorer
   reproduces the materialized reference model's ranked hits exactly at
   100k and 250k documents (this session's receipt), which is the kind of
   equivalence claim most search engines cannot make.
4. Cross-host byte-identical committed state
   (`docs/gates/evidence/rag-cross-host-determinism-2026-08-22.md`).
5. Cost-model discipline: point-resolved transactions whose cost scales with
   touched keys, not corpus size (`docs/native/delta-all-engine-transaction-v1.md`
   plus this session's ingest/open/scorer work), each step with a receipt.

If a paper is written, follow the publication policy literally: exact
commits, hardware declaration, raw observations, exclusions with reasons,
non-claims. The head-to-head evidence against Weaviate already in
`docs/gates/evidence/` (`rag-weaviate-head-to-head-2026-08-23.md`,
`weaviate-139-*-rerun-2026-08-3*.md`) and the FiQA/NFCorpus relevance
receipts are the comparative material. The synthetic 250k ladder is not
publishable as a relevance result; it is a cost-model result and must be
labelled so.

---

## Quick start for Fable 5.1

1. `cd ~/Documents/hyphae && git status && git log --oneline -12` — confirm
   HEAD `1b2cc70`, clean tree, branch unpushed.
2. Check whether the devbox still exists (`doctl compute droplet list`). If
   it does, `tools/devbox.sh "cargo test -p hyphae-native-product --locked
   2>&1 | tail -3"` proves the toolchain. If it does not, recreate a c-16
   with Ubuntu 24.04, install rustup (toolchain pins to 1.96.0 via
   `rust-toolchain.toml`), and update `HYPHAE_DEVBOX_IP`.
3. Ask the user whether the branch should be pushed / PR'd before new work.
4. Then take open item 1 (manifest) unless redirected.
