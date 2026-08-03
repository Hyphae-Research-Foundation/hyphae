# Native lexical document lifecycle evidence

Date: 2026-08-03

Status: direct-Linux lifecycle gates passed; hosted stack and phase gates remain
open

Source branch: `codex/native-search-document-lifecycle`

Stack base: `codex/native-delta-all-engine-transaction`

Contract commit: `0200485600b8eea98f4290d371e80588881ae7d0`

Implementation commit: `02888c099c354f1109efeec1eef2f01dbcabfa92`

Measured harness commit: `e5bac1a3c56f00edbffb2b3486f56936906e139f`

Blob-reclamation gate commit:
`b095629a478d8b77e1c27c44a0cf9ce2821a1784`

The checked
[machine-readable receipt](native-search-document-lifecycle-linux.json)
contains all three exact release runs, their derived median statistics,
environment and artifact identities, directed-gate counts, and the separate
allocation observation. Its SHA-256 before repository insertion is
`346458e8891fb91c98a6d9dd88807aa99208ed176ab829f73f65dd7469dadd0d`.

## Implemented mechanism

Hyphae now owns lexical document creation, exact replacement, deletion, and
identity reuse without a sidecar or complete-state rewrite.

- `REPLACE DOCUMENT=37` and `DELETE DOCUMENT=38` are canonical search WAL
  mutations. Derived statistics and postings do not create a second mutation
  stream.
- Replacement and deletion point-load one named collection and document into
  the detached physical delta. Subsequent operations resolve against that
  private overlay.
- The write-conflict identity is collection plus exact binary document ID.
  Same-document writers conflict; disjoint documents may rebase.
- The first lifecycle mutation atomically upgrades the current search marker
  from `HYSEABT1` to `HYSEABT2`.
- V2 admits only exact `HYDOCT01`, `HYTERMT1`, and `HYPOSTT1` tombstones.
  V1, malformed, extended, and cross-namespace tombstones fail closed.
- One sorted copy-on-write batch maintains the document source, collection
  length/count, term document frequency, posting membership, and term
  frequency.
- The local transaction protocol adds search opcodes `3` for replacement and
  `4` for deletion. A missing target preserves the active batch and its next
  operation ordinal.
- Text above 8,192 bytes retains the shared immutable-blob envelope. Page
  vacuum, checkpoint, WAL retention, and blob collection reclaim replaced and
  deleted source blobs without resurrection.

## Red and deterministic gates

The valid compiler-reaching red log has SHA-256
`a6bb5e89bb7442b04b6cfccca483897a9586f7979a5cf5c17223307371cbe8ba`.
It reached Rust compilation and failed only because the four public
replacement/deletion APIs did not yet exist.

An earlier SSH attempt did not source `/home/mario/.cargo/env`; it failed
before compilation because `cargo` was absent from the non-login shell `PATH`.
That setup failure was discarded and is not evidence.

The directed suites prove:

- transaction-private index/replace/delete/reinsert sequences;
- exact historical and reopened BM25 result/score equivalence;
- same-document conflict and disjoint-document optimistic rebase;
- exact V1-to-V2 upgrade, tombstone revival, and fail-closed malformed
  tombstones;
- stable WAL bytes and target/value/expiry/engine shape rejection for opcodes
  `37` and `38`;
- stable local bytes, every truncation boundary, reserved bytes, frame bounds,
  and ordinal preservation after a missing-document failure;
- seven replacement plus seven deletion commit interruption boundaries, each
  reopening the prior or complete projection and never a mixed projection;
- a large inline-to-blob replacement, deletion, reopen, page vacuum, retention,
  and removal of both unreachable blob files; and
- thread-local guards that reject any lifecycle hot-path complete engine-state
  or catalog materialization.

The direct Linux package run passed 343 native-runtime unit tests, 5 delta
integration tests, 23 local all-engine transaction tests, 6 local search tests,
9 local SQL tests, 5 local GET tests, 6 local SET/TTL tests, 4 UDS transport
tests, and 6 lifecycle integration tests. Clippy accepts zero warnings.

## Direct-Linux environment

- canonical host `mario@10.77.10.10`;
- canonical repository `/home/mario/celiumsai/hyphae`;
- AWS EC2 `m6i.2xlarge`, Ubuntu 24.04.4 LTS;
- kernel `6.17.0-1019-aws`, KVM;
- Intel Xeon Platinum 8375C @ 2.90 GHz, 8 logical CPUs;
- 30 GiB RAM, no swap;
- ext4 on `/dev/nvme0n1p1`;
- Rust and Cargo 1.96.0, `x86_64-unknown-linux-gnu`;
- `perf` 6.17.13 and `heaptrack` 1.5.0; and
- release observations pinned to logical CPU 0 at concurrency one.

The environment receipt SHA-256 is
`d5f27e20bfcc7f2a2d4e4eef452a66dfb6e429f62a34a3ebc147f050b4bb5f46`.
WSL is not in the edit, build, test, benchmark, Git, or evidence path.

## Lifecycle latency observations

Each configuration uses a separately copied closed baseline, 32 distinct live
target documents, and 0, 256, or 4,096 unrelated documents. No observation is
discarded as warmup. Stage, commit, and total are independent distributions;
the table reports the median p50 statistic across three exact runs and never
subtracts percentiles.

| Unrelated docs | Operation | Durability | Stage p50 | Commit p50 | Total p50 |
|---:|---|---|---:|---:|---:|
| 0 | replace | memory | 50.748 us | 664.189 us | 717.336 us |
| 0 | replace | strict | 99.300 us | 7.564222 ms | 7.653823 ms |
| 0 | delete | memory | 22.469 us | 293.642 us | 319.104 us |
| 0 | delete | strict | 54.199 us | 6.754565 ms | 6.809480 ms |
| 256 | replace | memory | 42.702 us | 973.559 us | 1.018248 ms |
| 256 | replace | strict | 101.265 us | 9.166215 ms | 9.263235 ms |
| 256 | delete | memory | 43.176 us | 805.384 us | 851.762 us |
| 256 | delete | strict | 99.617 us | 8.678214 ms | 8.787797 ms |
| 4,096 | replace | memory | 71.515 us | 1.295983 ms | 1.369099 ms |
| 4,096 | replace | strict | 141.813 us | 9.670471 ms | 9.817983 ms |
| 4,096 | delete | memory | 70.715 us | 1.033210 ms | 1.109708 ms |
| 4,096 | delete | strict | 141.175 us | 8.966219 ms | 9.109864 ms |

The staging surface remains in the microsecond domain at every observed
population. Memory commit crosses one millisecond at 4,096 unrelated
documents; strict commit remains dominated by physical publication and
synchronization. No universal sub-millisecond claim follows from these
observations.

Release harness source SHA-256:
`c4ea2bbe6660adb1894dc0fbffdb43fea371f3e7b47bc305e5ab5a7f93d898c0`.

Release binary SHA-256:
`6a0c7805c102de5e06e4a992ce5695978efe40501e059882975566c782ee9c6c`.

Raw run SHA-256:

- run 1:
  `6349289f2381ca8979d20fd0c87b17d0cb674de95b06e9380926bd5f5dc1f61b`;
- run 2:
  `2113d2b87f004a594cee48a27da364369f85fe4e6ab33d5dc7f2a4d42d986f72`;
  and
- run 3:
  `ac76647e5cf9435a3d4de9093a6a44f2bfb730977dea19aad0d160ad7c662212`.

## Physical work

Physical counters are median p50 across the same three runs. Memory and strict
durability produce the same logical page work for a given
population/operation, so they are shown once.

| Unrelated docs | Operation | Stage reads | Commit reads | Page appends | WAL bytes |
|---:|---|---:|---:|---:|---:|
| 0 | replace | 4 | 41 | 4 | 65,536 |
| 0 | delete | 2 | 17 | 1 | 65,536 |
| 256 | replace | 4 | 51 | 13 | 65,536 |
| 256 | delete | 4 | 41 | 11 | 65,536 |
| 4,096 | replace | 6 | 71 | 15 | 65,536 |
| 4,096 | delete | 6 | 56 | 12 | 65,536 |

Every observation reports zero complete engine-state loads and zero complete
catalog loads. Population growth increases B+tree height, reached pages, and
copy-on-write appends; it does not introduce an all-document or all-catalog
scan.

## Allocation observation

Unsafe Rust remains forbidden, so the evidence does not install an in-process
counting allocator. `heaptrack` 1.5.0 instruments the unchanged release binary
in a separate CPU-0 run:

- allocation-function calls: 14,991,574;
- temporary allocations: 720,506;
- peak heap: 7.35 MiB;
- peak RSS including profiler overhead: 16.18 MiB; and
- leaked at process exit: 544 bytes.

This is a whole-process counter including corpus construction, baseline copies,
and every configuration. It is not a per-operation allocation claim, and the
profiler run's latencies are excluded from the latency table.

Capture SHA-256:
`080dbcd0c5ced99001eb33a28e164e821e931c3d8d30e96568cb866cc8c5c315`.

Summary SHA-256:
`c204a3e92e6dff75ebea4e61358053d82d6448fd037183769790f0cdc56b1e40`.

## Discarded benchmark path

The first benchmark wrapper was invalid. Its shell expanded the loop variable
before the inner process, so all three runs targeted one filename. That
harness also reseeded the same 4,096-document baseline four times per run.
The process was stopped, no output was admitted, and commit `e5bac1a` instead
creates one closed baseline per population and copies it for each operation
and durability. The three admitted run hashes above are distinct.

## Verification funnel and open gates

The final direct-Linux funnel runs:

- `cargo fmt --all -- --check`;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
  and
- `python3 tools/check_documentation.py`.

The branch still requires the hosted Linux stable/MSRV, macOS, Windows,
quality, conformance, release-readiness, fuzz, dependency/license/secret,
package, release-assembly, optional-integration, and soak checks after its
stacked draft PR is opened.

This slice advances G0/G1 and removes mutable lexical documents from the known
G1 delta gap. It closes no complete phase gate. Search tombstone compaction,
bulk APIs, positions/phrases/filters/facets/highlights, immutable segments and
merge scheduling, cross-engine SQL operators, concurrent-reader mixing,
backup/restore, replication, clustering, multitenancy, TLS, encryption at
rest, SaaS roles/billing, embeddings, and LLM integration remain open.
