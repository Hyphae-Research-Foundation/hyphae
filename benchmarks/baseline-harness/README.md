# SPDX-License-Identifier: CC-BY-SA-4.0
# Baseline harness

Standalone benchmark workspace comparing the Hyphae native engines against
widely deployed single-purpose baselines on dedicated hardware. It is
deliberately **not** a member of the root workspace: SQLite, DuckDB, Valkey,
and Tantivy exist here only as measurement subjects and never enter the
product dependency graph.

The current search baseline locks Tantivy 0.26.2 and `lru` 0.16.4 to include
the cache iterator safety fix. New measurements report those engine versions;
existing receipts remain bound to their original source commits and versions.

## Suites

| Suite | Hyphae surface | Baseline | Variable isolated |
|---|---|---|---|
| `sql` | native SQL (prepared PK reads, strict/batched writes) | SQLite (WAL, `synchronous=FULL`), DuckDB (default WAL) | point-indexed OLTP shape |
| `keyspace` | native structures (embedded) | Valkey 9.1.2 over UDS (`no`, `always`, and `everysec`) | transport + persistence acknowledgement |
| `lexical` | native BM25 (`match_latest_text`) | Tantivy (default BM25) | inverted-index ingest + top-10 query |
| `ablation` | Hyphae only | — | fsync policy, batch materialization vs delta, per-engine commit composition |

Workloads are deterministic (seeded xorshift64*), byte-identical across
engines, and every receipt embeds the host fingerprint, source commit, and
per-phase p50/p95/p99/p99.9 exclusive latencies plus throughput.

## Fairness rules

- Identical row/document/key contents and identical operation sequences.
- Durability lanes stay separate: Hyphae `Strict` vs Valkey `always`, Hyphae
  `Memory` vs Valkey `no`, and Valkey `everysec` without a Hyphae equivalence
  claim. Each paired lane iterates the same immutable write-key vector and
  publishes its SHA-256 in both results. All engines also consume one shared
  immutable read-key vector and publish its independently checked SHA-256.
- Version-2 external receipts bind the pinned source archive, compiler and
  explicit build flags, SHA-256 of the live `/proc/<pid>/exe`, checked-in config
  SHA-256, complete effective `CONFIG GET *`, and validated `INFO server`
  runtime identity. Missing or inconsistent identity aborts before measurement.
- Every paired Hyphae lane creates a separate fresh database. The runner removes
  and recreates each Valkey lane directory, writes a per-run setup marker, and
  requires an empty database before loading the shared dataset identity.
- Authoritative keyspace receipts require exactly 1,000,000 keys, 500,000 reads,
  10,000 Strict/always writes, 200,000 Memory/no writes, and the sealed
  production seed. Reduced workloads are diagnostic only.
- Prepared statements everywhere; each engine uses its fastest documented
  local read path.
- Baselines run their defaults where a default is the documented production
  posture; deviations are stated in the receipt.
- DuckDB is included as a familiar reference point, not as an OLTP victim:
  it is a columnar OLAP engine and the receipt says so.

## Running on dedicated hardware

```bash
# on the metal host (Ubuntu 24.04, root), with the repo at /root/hyphae:
bash benchmarks/baseline-harness/scripts/run-metal.sh
```

The script formats the spare local NVMe instance-store disk, pins the
performance governor, downloads (or accepts through `VALKEY_ARCHIVE`) the
pinned source archive, verifies its SHA-256, builds it in a temporary external
directory with explicit compiler and build settings, and starts three UDS-only
Valkey servers from the checked-in configs. It builds the harness in release
mode and writes one `hyphae-baseline-harness-v2` JSON receipt per suite to
`/root/bench-results/`. The keyspace harness rejects a server unless its source,
binary, config, effective settings, fresh setup, and live runtime identity all
match. The runner refuses pre-existing Valkey daemons, sockets, and PID files,
and retains the exact executed Valkey binary beside the receipt for independent
rehashing. It also requires an exact clean Hyphae commit/tree and verifies the
SHA-256 of the currently executed harness containing the embedded product code.
The source commit/tree and clean state are checked both before and after the
sanitized release build. Only a validated i7i.metal-24xl run is authoritative;
all other receipts must identify themselves as non-authoritative diagnostics.
Qualification requires no hypervisor CPU flag, complete CPU/kernel/memory and
topology evidence, 48 physical cores with SMT2, complete `0-95` affinity, no CPU
quota, 768 GiB memory within five percent, performance governors on every CPU,
and the exact Amazon EC2 NVMe Instance Storage model/device/filesystem/queue
topology rather than an EBS NVMe device. The executed clean
tree must contain the exact profile plus its sealed authority, producer,
validator, and test bundle.

## Valkey Application Core foundation

`valkey/application-core-v1.json` pins Valkey tag `9.1.2`, source commit and
tree, source-archive SHA-256, the three config SHA-256 values, and the first
machine-readable operation inventory. `exact`, `equivalent`, `stronger`,
`different`, and `excluded` classify scoped data semantics only; none claims a
RESP surface or Valkey/Redis compatibility. Its full claim seal freezes those
definitions, every operation's mapping/scope/reason or unsupported note, and
all source, evidence, configuration, and claim authorities.

The inventory limits exact `SET` to missing or live scalar keys and excludes
live typed collections. It conservatively classifies `GETRANGE`, `SETRANGE`,
`ZPOPMIN`, and `ZPOPMAX` as different where missing/expired or empty-patch
lifecycle behavior diverges. `ZRANGE` has separate sealed rank and score cases:
rank excludes `LIMIT`/offset shapes, while score requires an explicit bounded
`LIMIT offset count`. `EXPIREAT` and `PEXPIREAT` equivalence is limited to
the signed seconds/milliseconds domains whose multiplication by 1,000,000 or
1,000 respectively fits signed i64 microseconds without overflow. `TTL` and
`PTTL` equivalence covers only strictly future expiries that produce positive
seconds/milliseconds; persistent, missing, expired, and equality-at-now
boundaries are excluded. Set algebra equivalence admits at most 64 input key
positions, 4,096 output members, and 1,000,000 visits. Both set algebra and
`ZRANGE` must satisfy the product's actual `StructureRead` admission cost:
response count `1` and `format!("{:?}", read.value).len() <= 16,777,216`.
Oversized request, work, item, or Debug-byte results are outside scope.

The 4.0.0 candidate re-seals this foundation profile after reviewing the changed
source bundle. The mapped keyspace and structure implementation and its scoped
tests are unchanged; the bundle changes cover embedding, build manifests, and
CI. The historical 3.0.0 classification source remains identified separately
from the executed candidate tree. This review creates no Valkey compatibility
or comparative performance claim.

Validate the authority and run its mutation tests without building the Hyphae
product graph:

```bash
python3 benchmarks/baseline-harness/valkey/check_profile.py
python3 -m unittest discover -s benchmarks/baseline-harness/valkey -p 'test_*.py'
python3 benchmarks/baseline-harness/valkey/check_receipt.py /path/to/keyspace.json \
  --expected-source-commit <commit> --expected-source-tree <tree> \
  --harness /path/to/hyphae-baseline-harness \
  --valkey-binary-artifact /path/to/retained-valkey-server \
  --source-root /path/to/retained-clean-source \
  [--require-authoritative]
```

A quick local smoke (small scale, no external Valkey):

```bash
test -z "$(git status --porcelain=v1 --untracked-files=all)"
export HYPHAE_SOURCE_PRE_COMMIT="$(git rev-parse 'HEAD^{commit}')"
export HYPHAE_SOURCE_PRE_TREE="$(git rev-parse 'HEAD^{tree}')"
export HYPHAE_SOURCE_PRE_CLEAN=true
test -z "$(compgen -A variable CARGO_PROFILE_)"
unset RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
export RUSTFLAGS=
export CARGO_ENCODED_RUSTFLAGS=
export CARGO_INCREMENTAL=0
cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml
test -z "$(git status --porcelain=v1 --untracked-files=all)"
export HYPHAE_SOURCE_POST_COMMIT="$(git rev-parse 'HEAD^{commit}')"
export HYPHAE_SOURCE_POST_TREE="$(git rev-parse 'HEAD^{tree}')"
export HYPHAE_SOURCE_POST_CLEAN=true
test "$HYPHAE_SOURCE_PRE_COMMIT" = "$HYPHAE_SOURCE_POST_COMMIT"
test "$HYPHAE_SOURCE_PRE_TREE" = "$HYPHAE_SOURCE_POST_TREE"
export HYPHAE_RUSTC="$(rustc --version)"
export HYPHAE_RUSTC_VERBOSE="$(rustc -vV)"
export HYPHAE_CARGO_VERSION="$(cargo --version)"
export HYPHAE_BUILD_PROFILE=release
export HYPHAE_RUSTFLAGS_STATE=empty
export HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE=empty
export HYPHAE_RUSTC_WRAPPER_STATE=unset
export HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE=unset
export HYPHAE_CARGO_PROFILE_OVERRIDES_STATE=absent
export HYPHAE_CARGO_INCREMENTAL_STATE=disabled
export HYPHAE_BUILD_COMMAND="cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml"
export HYPHAE_HARNESS_PRODUCT_BINARY_SHA256="$(sha256sum \
  benchmarks/baseline-harness/target/release/hyphae-baseline-harness | cut -d ' ' -f 1)"
export HYPHAE_RECEIPT_AUTHORITY=non-authoritative-diagnostic
export HYPHAE_HARDWARE_QUALIFICATION=local-unqualified
export HYPHAE_EC2_NVME_MODEL=unqualified
export HYPHAE_NVME_DEVICE_ID=unqualified
export HYPHAE_NVME_FILESYSTEM=unqualified
export HYPHAE_NVME_ROTATIONAL=unqualified
export HYPHAE_NVME_QUEUE_DEPTH=unqualified
benchmarks/baseline-harness/target/release/hyphae-baseline-harness \
  all /tmp/hyphae-bench-scratch /tmp/hyphae-bench.json --scale small
```

Receipts feed the evidence documents under `docs/gates/evidence/`; raw JSON
outputs are attached there verbatim, never post-processed by hand.
