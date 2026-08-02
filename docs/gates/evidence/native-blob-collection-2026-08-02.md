# Native immutable-blob collection evidence — 2026-08-02

Status: implementation, deterministic process-interruption recovery, matched
Windows/NTFS and WSL2/tmpfs observations, and strict workspace validation are
green; native-ext4, sector-level power-loss, automatic scheduling, and
multi-root pinning remain open

## Identity and scope

The measured implementation is:

- commit
  `53396099256e54b01a5ba81a83150e72f733179e`;
- tree `dccbd4aa3e7eebab76a40fb6a69aafb73015f252`;
- Rust `1.96.0 (ac68faa20 2026-05-25)`;
- benchmark
  `crates/hyphae-native-runtime/examples/blob_collection_benchmark.rs`; and
- contract [Native immutable-blob collection
  v1](../../native/blob-collection-v1.md).

This evidence covers explicit collection only after current-root page vacuum,
checkpoint, and WAL/manifest retention have reduced restart authority to one
exact root. It does not establish safe collection across retained historical
snapshots, replicas, backups, archives, change feeds, or a nonempty WAL
suffix.

## Implemented behavior

`hyphae-native-blobs` now separates verified physical inventory from the
committed generation floor:

- existing format-1 blob and root bytes are unchanged;
- isolated legacy open still derives its generation from file count;
- runtime recovery verifies every complete blob before WAL/root recovery and
  then applies the latest retained root's committed generation as a floor;
- collection never decreases the in-memory generation;
- a later unique publication increments the retained generation even when
  physical file count was reduced; and
- recovery reports physical files/bytes, committed floor, effective
  generation, and complete blob-verification time separately.

The store also owns a scoped exact-reference trace. Every successful
`BlobStore::read` records the complete verified `BlobReference` only while the
trace is active. Nested traces fail closed. Candidate enumeration rejects a
forged live set, orders candidates by digest, supports a deterministic partial
prefix, deletes exact canonical files, and synchronizes `blobs/` where the
platform implementation supports it.

`NativeDatabase::collect_blobs` accepts only:

- one stable `HYWAR001` anchor and no pending anchor;
- an empty retained WAL file;
- one retained manifest equal to the anchor generation/digest;
- a coordinator root exactly equal to that manifest;
- `visible_csn == retention_floor_csn == anchor.base_visible_csn`;
- the active page generation; and
- a root blob generation no newer than the verified store generation.

It then executes complete `validate_roots` under the trace. That traversal
loads catalog definitions, relational version chains, scalar and collection
structures, lexical source documents, and ANN state. Only verified physical
references absent from the resulting complete live set become candidates.

## Red-to-green gates

The blob-store acceptance tests first failed with 11 `E0599` errors because
reference tracing, generation-floor open, pruning, and namespace
synchronization did not exist. The runtime acceptance tests then failed with
21 `E0433`/`E0599`/`E0609` errors because collection boundaries, receipts,
recovery fields, and database methods did not exist.

After implementation:

- 7 `hyphae-native-blobs` tests pass;
- 162 `hyphae-native-runtime` tests pass;
- strict clippy passes for both crates, all targets, and all features;
- no new `allow`, unsafe Rust, `unwrap`, `expect`, `panic`, or `unreachable`
  was introduced; and
- the existing complete cross-engine recovery suite remains green.

The final evidence branch passed the following on both Windows and WSL2:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py
```

The documentation checker reported 179 Markdown files and 12 JSON examples.
Both benchmark JSON files also parsed successfully before this evidence was
recorded.

## Deterministic interruption matrix

The runtime test corpus creates:

- one large catalog definition blob;
- five successive large values, each deduplicated between the relational row
  and scalar keyspace;
- the final value reused as one lexical source document, producing one
  reference shared by three engines; and
- retired values made unreachable through page vacuum and WAL/manifest
  retention.

Each boundary returns an injected error, requires dropping the handle, and
then reopens to the exact retained catalog, relational, structure, lexical,
and generation state:

| Boundary | Durable state on reopen |
|---|---|
| references traced | all dead candidates may remain |
| first candidate removed | one digest-ordered dead file is absent |
| all candidates removed | all dead files are absent; directory sync may be pending |
| directory synchronized | complete collection is durable where supported |

Every reopened state retains two live files, preserves generation floor 6,
and accepts an idempotent retry. A commit after collection advances generation
to 7. Collection before vacuum/checkpoint/WAL retention or after a later WAL
suffix fails with `BlobCollectionIneligible`.

Existing complete-corruption tests still fail closed while opening the
physical namespace. Collection cannot be used to bypass a corrupt dead file
because all complete files verify before liveness authority is selected.

## Matched benchmark corpus

Both corpora execute the same operations:

1. create relational, scalar, lexical, ANN, and wide typed-catalog state;
2. publish 129 distinct large values, with the final value shared by
   relational, scalar, and lexical paths;
3. update the ANN vector in the final transaction;
4. vacuum the current page root;
5. checkpoint and retire the WAL/manifest prefix;
6. leave one corpus uncollected and collect the other;
7. reopen each corpus 25 times; and
8. verify exact relational bytes, scalar bytes, lexical hit, ANN hit, and wide
   catalog object on every open.

The matched physical result on both hosts is:

| Metric | Uncollected | Collected | Change |
|---|---:|---:|---:|
| blob files | 130 | 2 | -128 |
| encoded blob bytes | 1,200,106 | 35,928 | -97.0063% |
| committed generation floor | 130 | 130 | unchanged |
| effective generation | 130 | 130 | unchanged |

The two live files are the large catalog definition and the final
cross-engine value. The 128 removed files occupy 1,164,178 bytes.

## Windows/NTFS observation

The Windows benchmark data directories were created below
`C:\Users\Mario\AppData\Local\Temp`. `Get-Volume` identified drive `C:` as
healthy NTFS. The source checkout and build output location are not used as
the benchmark data directory.

Exact receipt:

- liveness trace: 1,840,900 ns;
- candidate enumeration/deletion: 15,103,400 ns;
- namespace synchronization call: 78,300 ns;
- total collection: 17,050,700 ns;
- directory-sync guarantee: unsupported and reported `false`;
- warm blob verification p50: 6,434,200 ns → 243,100 ns,
  `26.467297x`; and
- warm external reopen p50: 8,582,500 ns → 1,883,500 ns,
  `4.556676x`.

The complete receipt is
[`native-blob-collection-windows.json`](native-blob-collection-windows.json).
The Windows result proves process restart on NTFS. It does not prove
directory-entry persistence after power loss because the Windows
implementation explicitly reports directory synchronization unsupported.

## WSL2/tmpfs observation

`findmnt` identified:

- benchmark data under `/tmp` as `tmpfs`; and
- the source checkout under `/mnt/e` as `9p`.

`CARGO_TARGET_DIR` was also placed under `/tmp`. Therefore this is a Linux
runtime/process-restart and synchronization-path observation on memory-backed
tmpfs, not native-ext4 persistence evidence.

Exact receipt:

- liveness trace: 931,094 ns;
- candidate enumeration/deletion: 521,465 ns;
- namespace synchronization call: 3,593 ns;
- total collection: 1,466,568 ns;
- directory-sync implementation: reported `true`;
- warm blob verification p50: 819,195 ns → 20,523 ns,
  `39.915948x`; and
- warm external reopen p50: 1,659,461 ns → 800,895 ns,
  `2.072008x`.

The complete receipt is
[`native-blob-collection-wsl2.json`](native-blob-collection-wsl2.json).
The `true` synchronization field proves that the Unix directory-sync code path
ran successfully; tmpfs still cannot establish physical-media power-loss
durability.

## Reproduction

Windows:

```powershell
$commit = git rev-parse HEAD
$tree = git show -s --format=%T HEAD
$rustc = rustc -V
cargo run --release -p hyphae-native-runtime `
  --example blob_collection_benchmark --locked -- `
  $commit $tree "$rustc" "NTFS (Windows TEMP on C drive)"
```

WSL2:

```bash
cd /mnt/e/Codex-Projects/MyBook/Documents/celiumsai/hyphae
CARGO_TARGET_DIR=/tmp/hyphae-blob-collection-target \
  cargo run --release -p hyphae-native-runtime \
  --example blob_collection_benchmark --locked -- \
  53396099256e54b01a5ba81a83150e72f733179e \
  dccbd4aa3e7eebab76a40fb6a69aafb73015f252 \
  "rustc 1.96.0 (ac68faa20 2026-05-25)" \
  "tmpfs (/tmp), source on 9p (/mnt/e)"
```

## Residual risk and gate status

This closes the immediate immutable-blob leak for the deliberately narrow
single-retained-root policy. It does not close G1 or G7.

Still required:

- registered pins for snapshots, replicas, backups, archives, and change
  feeds before multi-root collection;
- native Linux persistent-filesystem and physical power-loss evidence;
- Windows directory durability or a documented stronger publication
  primitive;
- large-blob streaming/chunk trees and bounded-memory tracing;
- automatic maintenance admission, cancellation, and scheduling;
- saturation, storage-wear, and collection-amplification evidence; and
- the broader crash/reordering matrix outside process termination.
