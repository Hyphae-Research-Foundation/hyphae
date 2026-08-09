# Native set algebra evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux semantic, restart, corruption, and latency
gates complete; hosted CI, destination-set writes, set TTL, sorted-set
algebra/TTL, streams, complete G3, and G7 remain open

Source branch: `codex/native-set-algebra`

Stacked base:
`codex/native-hash-model-equivalence@72ff5f5828838df70e3be3681456492a5cae5629`

Contract commit: `9a482490aefbc198d5740130827bac722269a43d`

Implementation commit: `3543d5530f428e559fd30597602e7b6d3da912f2`

Logical-time correction commit:
`503f09e3d01365c2a6b597f82e513cc5e3254e5e`

Hardening commit: `7cf4e4f138147119fb3b3903153f34a12000a50b`

Benchmark commit:
`700e4ef4d4c3be224b59180b4314be7344acfcc4`

Verified source tree:
`2acbbe2331f51f426d0945e3860c520a6575a15b`

## Scope

This gate implements the frozen bounded read-only set-algebra contract without
a Valkey sidecar, compatibility projection, destination mutation, WAL opcode,
or serialized internal protocol. One checked request selects union,
intersection, or ordered difference over 1–64 exact binary set keys with hard
output and visit limits. Results are complete and exact-byte ascending or the
operation returns a typed limit error with no partial result.

The same request executes against the private write batch, a retained
snapshot, and the current physical B+tree root at an explicit logical time.
Missing keys are mathematical empty sets. Every live input position is
type-checked before an empty-result short circuit, so a live scalar, hash,
list, or sorted set returns `StructureKindMismatch`.

The physical path reads native set metadata and member namespaces directly; it
does not reconstruct `StructureState`. Union scans every source. Intersection
chooses the smallest live cardinality, using the lowest caller position as the
deterministic tie-break, and probes the other sources. Difference scans the
first source and probes later sources in caller order. Source envelope,
tombstone, and membership lookup work all consume the declared visit budget.

## Contract-first red and logical-time correction

Before implementation, a test imported the deliberately absent
`SetAlgebraOperation` and `SetAlgebraRequest` public types. The compile-only
gate failed with Rust `E0432`, establishing that the new contract surface did
not exist.

The first physical implementation reused current-root type metadata without
applying caller logical time. That made due scalar and hash incarnations look
like wrong-kind live inputs. Commit `503f09e` binds the physical request to an
explicit logical time: an incarnation is live immediately before its expiry
and missing at equality or later. Malformed reached state still fails closed.

## Semantic, restart, and corruption result

Seven focused tests cover:

- exact request minima, maxima, rejection boundaries, and caller position
  preservation;
- private, retained-snapshot, current-root physical, and reopen equivalence;
- missing-key algebra, repeated first-key difference, and wrong-kind
  preflight;
- exact expiry equality for scalar and hash identities;
- output and visit exhaustion without partial results;
- maximum-size identity handling;
- multilevel smallest-set intersection and physical tombstone visits; and
- reached metadata-count and member-envelope corruption.

An additional 64 deterministic cases compare every operation against an
independent `BTreeMap`/`BTreeSet` oracle across private, retained, physical,
and reopened state.

The multilevel intersection case uses two 2,048-member sources and one
four-member source. Before deletion, the smallest-source route uses 12 visits.
After one member is deleted, all surfaces return the same three members: the
logical model uses 9 visits and the physical route uses 10 because it reaches
the durable tombstone. Reopen preserves that result and route accounting.

The direct-Linux native-runtime suite reports 282 passing tests and zero
failures.

## Latency observation

The checked release harness is
`crates/hyphae-native-runtime/examples/set_algebra_smoke.rs`, SHA-256
`f7518611e17ed664f8e41adf7f3cf9d062e0d8f68d9b366a6c2c751525394c08`.
Its deterministic dataset BLAKE3 is
`7429a4f17b6ff8cc436ba0b50e0fa34dc7ca9759a8d1f07f27d7c10b7ebe6729`.
The physical tree height is two. The authoritative raw observation SHA-256 is
`e1f7d9bdb9c3351c42dc9c2ac76498096b36154d609e42fd4182308a8983820b`.

| Route | Result members | Visits | p50 | p95 | p99 |
|---|---:|---:|---:|---:|---:|
| Small private union | 56 | 96 | 5.962 us | 6.056 us | 6.153 us |
| Small private intersection | 8 | 80 | 1.994 us | 2.059 us | 2.098 us |
| Small private difference | 8 | 80 | 1.867 us | 1.915 us | 1.942 us |
| Small snapshot union | 56 | 96 | 6.013 us | 6.115 us | 6.217 us |
| Small snapshot intersection | 8 | 80 | 1.983 us | 2.046 us | 2.084 us |
| Small snapshot difference | 8 | 80 | 1.838 us | 1.885 us | 1.913 us |
| Small physical union | 56 | 96 | 61.645 us | 71.002 us | 88.188 us |
| Small physical intersection | 8 | 80 | 93.814 us | 102.591 us | 104.897 us |
| Small physical difference | 8 | 80 | 93.838 us | 102.736 us | 105.430 us |
| Large physical union | 6,144 | 8,192 | 2.604951 ms | 2.657982 ms | 2.877073 ms |
| Large physical intersection | 64 | 192 | 218.443 us | 228.533 us | 234.614 us |
| Large physical difference | 2,048 | 8,192 | 6.783755 ms | 6.831848 ms | 6.928138 ms |

The small embedded paths are microsecond-first. The small physical paths
remain below 106 microseconds at p99 in this observation. The smallest-set
intersection strategy keeps the large physical intersection at 218.443
microseconds p50 despite two 2,048-member peer sources. Large union and
difference are explicitly cardinality-sensitive: materializing 6,144 output
members takes 2.605 milliseconds p50, while scanning and excluding across
8,192 physical visits takes 6.784 milliseconds p50. These are current pain
points and are not hidden behind a universal latency claim.

Two earlier raw observations are excluded. Observation
`09af3a811482778355dc0e05840544dd71131bdbd9fab9f93e1255d9d0fc92ad`
let the remote non-login shell expand the Rust-version command before Cargo's
PATH was available, leaving `rustc` empty. Observation
`167ec40bc5d96603a7fb2c437948de84235708fe2cd639bb381898c58a3458ca`
received a commit argument that does not resolve to the actual Git commit. No
latency value from either observation is used here.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary databases on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust `1.96.0 (ac68faa20 2026-05-25)`; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host:

```text
cargo fmt --all -- --check
cargo test -p hyphae-native-runtime --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo run -p hyphae-native-runtime \
  --example set_algebra_smoke --release --locked
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-set-algebra-linux.json
git diff --check
```

The machine-readable receipt records 282 native-runtime tests, 221 Markdown
files, and 12 JSON examples. The workspace test, Clippy, formatting,
documentation, JSON, and diff gates pass with all targets and features where
applicable.

The checked machine-readable receipt SHA-256 is
`f4a82a6be749489d138af1a68da60be74e3faef976e3b5492381f0fb3377b3c2`.

## Evidence boundary

This receipt proves the declared bounded read-only set algebra across private,
retained-snapshot, current-root physical, and reopened state at explicit
logical time. It also proves the selected reached-corruption failures and the
reported release-harness latency on the named AWS host.

It does not add or prove destination-set store variants, set or member TTL,
sorted-set algebra/TTL, streams, local-protocol exposure, network
compatibility, exhaustive state-space coverage, concurrent optimistic
histories, strict-fsync behavior, process kill, physical power loss, complete
G3, or G7. Hosted CI is not claimed by this local receipt.
