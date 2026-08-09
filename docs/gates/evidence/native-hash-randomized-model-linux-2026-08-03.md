# Native hash randomized-model evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux semantic gates complete; hosted CI, concurrent
histories, other structure-family models, memory amplification, complete G3,
and G7 remain open

Source branch: `codex/native-hash-model-equivalence`

Stacked base:
`codex/native-hash-field-ttl@1544a4faca3902046181df58d8b7bef883f81d96`

Contract commit:
`3af6c63d4e9ef1020d71ba2adbc9f86c48a2ef4e`

Implementation commit:
`0cf10e92f73eb5701c1de20aff975a7041a8cc2d`

Coverage-reporting commit:
`316310f37d56c7ea3bd9228a1a29a78525b467d0`

Verified source tree:
`107b7946eab646609243fe17ac2472d4e0e8ee65`

## Scope

This gate is a deterministic dependency-free state-machine comparison between
the independent logical `StructureState` oracle and the native hash engine.
Every generated step compares the exact command outcome and all visible hash
state through:

- the uncommitted private batch;
- the retained pre-publication snapshot;
- a newly materialized snapshot at the same logical time;
- current-root physical point, TTL, cardinality, ascending, descending, and
  pattern-scan reads; and
- periodic close and reopen.

The fixed corpus uses SplitMix64, 16 checked seeds, 256 steps per seed, four
binary keys, 32 binary fields, exact expiry-boundary time advances, and memory
durability publication. The harness SHA-256 is
`292cdfdd20da9aceeef8748508836d8eaea8239514a08966a8418a2b58717d13`.

## Contract-first red and discovered divergence

The compiler-reaching red gate named the deliberately absent
`run_fixed_hash_model_corpus` entry point before the implementation existed.
After the first complete harness compiled, seed ordinal 0, step 0 found a real
contract divergence on the physical surface:

```text
surface=current-root-physical
check=ttl-field ... unexpected_error=UnknownStructureHash
```

`TTL_HASH_FIELD` is a read-only TTL surface. Its frozen contract returns
`Missing` for an absent, due, or non-hash key. The physical implementation had
reused the mutating hash-kind validator and returned `UnknownStructureHash`.
The correction now reads hash metadata without projecting mutating kind
semantics, preserves malformed-state failures, and returns `Missing` for the
three contracted cases. A focused regression covers absent, whole-hash due,
non-hash, and reopened state.

## Fixed corpus result

The authoritative direct-Linux run reported:

| Measurement | Count |
|---|---:|
| Fixed seeds | 16 |
| Steps per seed | 256 |
| Total actions | 4,096 |
| Total comparisons | 4,524,373 |
| Private-batch audits | 4,096 |
| Retained-snapshot audits | 4,096 |
| Materialized-snapshot audits | 4,096 |
| Physical audits | 4,240 |
| Reopens | 128 |
| Rust test elapsed | 51.36 s |
| Whole command elapsed | 52.94 s |

Action coverage is exact:

| Action | Count |
|---|---:|
| Create hash | 370 |
| Delete hash | 267 |
| Expire whole hash | 295 |
| Set one field | 729 |
| Set multiple fields | 405 |
| Delete one field | 352 |
| Delete multiple fields | 304 |
| Increment field | 322 |
| Expire field | 413 |
| Advance logical time | 382 |
| Read-only probe | 257 |

The checked seeds are:

```text
0x4859504841450001 0x4859504841450002 0x4859504841450003
0x4859504841450004 0x4859504841450005 0x4859504841450006
0x4859504841450007 0x4859504841450008 0x4859504841450009
0x485950484145000a 0x485950484145000b 0x485950484145000c
0x485950484145000d 0x485950484145000e 0x485950484145000f
0x4859504841450010
```

The fixed prelude reaches field-expiry equality, persistent replacement,
whole-hash expiry equality, recreation, multi-set, increment, multi-delete,
and immediate-due field expiry. Generated steps cover every action kind and
all three exact/prefix/wildcard pattern routes.

## Negative control and diagnostics

`perturbed_hash_oracle_reports_exact_trace_location` deliberately changes one
oracle field value while leaving native state intact. The test passes only
when the audit rejects the mismatch and its message includes the exact seed,
seed ordinal, step, logical time, action, physical surface, and hex identities.
Unexpected native errors use the same trace envelope.

This proves the harness can detect a known divergence. It does not prove that
every possible defect or state is detectable.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary databases on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust `1.96.0`; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host:

```text
cargo fmt --all -- --check
cargo test -p hyphae-native-runtime --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p hyphae-native-runtime \
  hash_model_equivalence::fixed_hash_trace_matches_every_execution_surface \
  --locked -- --exact --nocapture
cargo test -p hyphae-native-runtime \
  hash_model_equivalence::perturbed_hash_oracle_reports_exact_trace_location \
  --locked -- --exact
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-randomized-model-linux.json
git diff --check
```

The native-runtime suite reports 275 passing tests. The workspace test,
Clippy, and documentation gates pass with all targets and features where
applicable. No dependency, unsafe-Rust allowance, public command, WAL opcode,
physical format, sidecar, or network path was added.

The checked machine-readable receipt SHA-256 is
`dc05a3b13904c7cf043247fae23727eb61681f8d5a902579be618dd703ff96dd`.

## Evidence boundary

This receipt proves deterministic semantic equivalence for the declared hash
corpus and read surfaces, including logical expiry and restart equivalence
under `DurabilityClass::Memory`.

It does not prove exhaustive state space, concurrent optimistic histories,
scheduler fairness, allocation or RSS bounds, strict-fsync behavior, process
kill, physical power loss, other collection-family models, local-protocol
exposure, complete G3, or G7. Hosted CI is not claimed by this local receipt.
