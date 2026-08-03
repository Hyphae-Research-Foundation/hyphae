# Native lexical tombstone compaction evidence

Status: direct-Linux implementation, recovery, corruption, scaling, and
allocation gates pass. Hosted cross-platform and release gates are pending.

Date: 2026-08-03

Canonical host: `mario@10.77.10.10`

Canonical repository: `/home/mario/celiumsai/hyphae`

Branch: `codex/native-search-tombstone-compaction`

Stack base: `95f70fe9cdbc5d8423a776e13cbbc3e3d3abee9b`
(`codex/native-search-document-lifecycle`, PR #85)

Contract commit: `d9e3090`

Implementation commit: `7a8884a`

Scaling-harness commit:
`fea5925dc37bb1b8c9dbecaca4bc407e5199d3b2`

Corruption-gate commit: `c0e6512`

Machine-readable summary:
[native-search-tombstone-compaction-linux.json](native-search-tombstone-compaction-linux.json)

## Scope proved

This slice adds one embedded `compact_search(durability)` maintenance
operation for native `HYSEABT1`/`HYSEABT2` roots. Applied work uses search WAL
opcode `COMPACT SEARCH=39`, rebuilds only the current lexical/search B+tree,
and publishes the replacement search root through the existing global
commit/WAL/MVCC authority.

The implementation:

- validates the complete lexical projection and every exact V2 tombstone;
- validates the catalog-bound ANN generations sharing the search root;
- drops only exact `HYDOCT01`, `HYTERMT1`, and `HYPOSTT1` tombstones;
- retains every live lexical, metadata, and ANN key/value byte;
- returns a true no-op for an empty pre-genesis slot, V1 root, or V2 root
  without tombstones;
- leaves historical roots and immutable blobs untouched;
- keeps page vacuum, WAL/manifest retention, and blob collection as separate
  authorities; and
- exposes no local-protocol opcode in this slice.

This is explicit maintenance work, not a request hot path. The observations
below do not claim universal microsecond latency.

## Specification-first red gate

The integration contract test was compiled before the receipt, method, WAL
opcode, or physical batch mode existed:

```text
error[E0432]: unresolved import
`hyphae_native_runtime::SearchCompactionReceipt`

error[E0599]: no method named `compact_search` found for struct
`NativeDatabase`
```

The 22-line compiler-reaching log has SHA-256
`a3b6021e98dcf1a09138515da005743637a192c4b58de3b5618177084302a0cf`.
The failure reached the new integration target and named only the absent
public contract.

## Deterministic implementation gates

The focused gate:

```text
cargo test -p hyphae-native-runtime \
  search_compaction_equivalence --all-features --locked
```

passes six internal equivalence/recovery tests. The public integration target
adds two API/lifecycle tests. Together they prove:

- exact mixed replace/delete tombstone counts and receipt arithmetic;
- byte-for-byte retained physical entries;
- identical BM25 hits and scores before/after compaction and reopen;
- a retained historical snapshot with its original documents and scores;
- identical ANN physical bytes, build identity, approximate hits, and exact
  ranking;
- successful ordinary revalidation of a document writer captured before
  compaction;
- V1 and legacy-inline no-write behavior with unchanged page and WAL sizes;
- malformed/extended tombstones, unknown prefixes, orphan postings, malformed
  ANN metadata, and missing source blobs rejected before compaction append;
- all seven commit interruption boundaries reopening either the complete
  prior tree or the complete replacement tree;
- a second compaction as an exact no-op; and
- compaction followed by page vacuum, checkpoint, WAL retention, and blob
  collection without document resurrection.

The WAL codec includes stable opcode-39 bytes and rejects invalid
engine/target/key/value/expiry shapes. `cargo clippy` passes for every runtime
target and feature under `-D warnings`.

Before the final documentation seal, the package-wide command

```text
cargo test -p hyphae-native-runtime \
  --all-targets --all-features --locked
```

passed 350 library tests plus every runtime integration and example target.
The final workspace-wide funnel is recorded separately below when sealed.

## Direct-Linux scaling method

Environment:

- Ubuntu AWS kernel `6.17.0-1019-aws`, x86-64;
- Intel Xeon Platinum 8375C, 8 logical CPUs;
- `rustc 1.96.0`, `cargo 1.96.0`;
- release profile, concurrency 1, pinned to CPU 0;
- three independent process runs;
- five observations per population/ratio/durability in each run.

The harness creates deterministic lexical corpora at populations 256 and
4,096. Each document contributes one unique term and one shared term.
Deleting 25% or 75% therefore produces four exact tombstones per deleted
document. Every observation starts from an independently copied data
directory whose files are synchronized before the timer starts.

`validated_v1_plan` measures complete validation and scanning on a same-size
V1 live corpus and proves zero page/WAL movement. `memory_compaction` and
`strict_compaction` measure applied V2 rebuild/publication. Reported values are
the median of the three process-level p50 values:

| Documents | Tombstones | Scanned / dropped | Plan p50 | Memory p50 | Strict p50 | New pages |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 25% | 1,027 / 256 | 9.925 ms | 15.801 ms | 21.746 ms | 5 |
| 256 | 75% | 1,027 / 768 | 9.952 ms | 14.821 ms | 20.683 ms | 3 |
| 4,096 | 25% | 16,387 / 4,096 | 29.429 ms | 56.681 ms | 64.283 ms | 52 |
| 4,096 | 75% | 16,387 / 12,288 | 29.915 ms | 39.242 ms | 46.609 ms | 18 |

Planning scales with scanned physical population. Rebuild time also depends on
retained population: the 75% tombstone corpora retain fewer entries and append
fewer pages than the 25% corpora. Every applied observation writes one
65,536-byte WAL block. No observation materializes complete engine state;
one complete catalog load is intentional because ANN definitions must be
validated before publication.

The three raw JSON outputs have SHA-256:

- `cc1e893b91b323830f516d23707f8d72f7c0c2b2aeb1184da1debcbfd3cc7e09`;
- `8579eed9a48aff4d63231e6a0cd2d0a281a1238e40d4bf1b2711630abcf798ac`;
  and
- `5d7ad4f7bb972c31c5ede98c0f9c9d093ad09cf077499c429d11ff06b877cc42`.

### Discarded measurement

The first benchmark design copied each fixture and immediately timed strict
compaction. Linux still held the copied baseline pages dirty, so the strict
`sync_data` paid for fixture-copy writeback and produced false 7–10 second
p50 values at population 4,096. That output is intentionally excluded. Its
SHA-256 is
`fc6d5501f0942f6ad9de567e5968e39e4cb241805ce7aaf0e013d75dc819a9bc`.

The corrected harness synchronizes every copied file before timing and removes
each observation directory after closing it. A one-observation probe reduced
the large-corpus strict measurements to 45–65 ms before the three canonical
runs were admitted.

## Whole-process allocation observation

`heaptrack 1.5.0` ran the release harness pinned to CPU 0 with one observation
per configuration:

- 33,317,237 allocation calls;
- 1,059,214 temporary allocations (3.18%);
- 7.48 MiB reported peak heap;
- 16.33 MiB peak RSS including profiler overhead; and
- 544 bytes reported leaked at process exit.

The compressed trace SHA-256 is
`bfc443f7ff699091dea33a1723189c7c539ea76b98d07d4c8e0b6bef58bd9351`;
captured JSON stdout SHA-256 is
`6f0ed571264a3d4678e77d693b2409a4168f0c95462cd0bf262825095654f1cf`.
This is deliberately labelled whole-process evidence: it includes corpus
construction, synchronized copies, opens, validation, compaction, and cleanup.
It is not an operation-only allocation budget.

## Full funnel and hosted status

Pending at this evidence draft:

- workspace formatting, all-target/all-feature tests, Clippy, rustdoc, and
  documentation-link checks at the final seal commit;
- hosted Linux stable/MSRV, macOS, Windows, bounded fuzz, dependency/license,
  packaging, release assembly, public-conformance, optional-integration, soak,
  and release-readiness checks; and
- a draft stacked PR targeting
  `codex/native-search-document-lifecycle`.

No merge is authorized by this evidence.

## Residual boundary

This closes explicit current-root lexical tombstone compaction. It does not
add automatic policy, background workers, immutable segment merging, broad
query operators, cross-engine SQL, replication, clustering, multitenancy,
TLS, encryption at rest, SaaS roles/billing, embeddings, HiveMind, or an LLM.
It closes no complete phase gate by itself.
