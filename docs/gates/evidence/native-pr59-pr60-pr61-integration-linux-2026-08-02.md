# PR #59-#61 direct-Linux integration evidence

Date: 2026-08-02

Status: clean non-protected integration staging; protected `dev` and `main`
promotion, complete G2/G3/G7, and mutation testing remain open

Source commit:
`0cdf8838b72da1bf168e6bf058cb5a84b567d50b`

Source tree:
`8aba8c4d89849a0a3ad45560836fbebb88fe5a64`

Source branch: `codex/integrate-pr59-pr60-pr61`

Base: `dev@7cf616a8f8dfad10ca4168a1724fcd12a6da2876`

Draft integration PR:
[celiumsai/hyphae#62](https://github.com/celiumsai/hyphae/pull/62)

## Exact included heads

The integration commit preserves every reviewed feature head as an ancestor:

| PR | Feature head | Capability |
|---|---|---|
| #59 | `445716624e4eb37721f508ee27fd2c2d3d15165f` | Composite secondary-index prefix ranges |
| #60 | `641575c11e7826f7b954aac9a1c73c9c1471e293` | Bounded sorted-set score ranges |
| #61 | `aaa1b4980829c5c4424f57f81ea5797b7a9e97ba` | Bidirectional sorted-set member ranks |

Each exact SHA passed `git merge-base --is-ancestor <sha> HEAD`. No feature
commit was rebased, squashed, copied, or omitted.

PR #59 merged without conflict. PR #60 then merged without conflict. PR #61
required manual integration in five files:

- `crates/hyphae-native-runtime/src/lib.rs`;
- `docs/README.md`;
- `docs/gates/evidence/README.md`;
- `docs/gates/native-local-phase-1.md`; and
- `docs/native/structures-semantics-v1.md`.

The runtime resolution retains score-bound canonicalization, physical
`ZRANGE_BY_SCORE`, `ZRANK`, `ZREVRANK`, and model rank execution together. The
documentation resolution retains both evidence sets and removes obsolete
claims that either score ranges or member-rank lookup is still missing.
`crates/hyphae-native-runtime/src/model.rs`, the reverse B+tree visitor, rank
benchmark, and rank receipts merged without conflict.

## Direct-Linux mechanical validation

All commands ran in the clean source tree above on the canonical Linux host:

```text
mario@10.77.10.10
/home/mario/celiumsai/hyphae
Ubuntu 24.04.4 LTS
EC2 m6i.2xlarge, 8 vCPU, 30 GiB RAM
/dev/root ext4 on EBS
rustc 1.96.0 (ac68faa20 2026-05-25)
```

Focused integration checks passed:

- `cargo test --locked -p hyphae-native-runtime --lib sorted_set_`: 16 passed;
- `cargo test --locked -p hyphae-native-runtime --lib
  secondary_prefix_range`: 5 passed; and
- `cargo test --locked -p hyphae-native-btree --lib reverse_prefix_range`:
  1 passed.

The complete local gate then passed:

- `cargo fmt --all -- --check`;
- `cargo test --workspace --all-targets --all-features --locked`, including
  222 native-runtime library tests;
- `cargo clippy --workspace --all-targets --all-features --locked --
  -D warnings`, without a new lint suppression;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for all inherited feature receipts;
- release checks for `microsecond_smoke`, `sorted_set_smoke`, and
  `sorted_set_rank_smoke`;
- `git diff --check`; and
- a clean worktree assertion.

Mutation testing was not executed. The repository still has no accepted
mutation tool, operator set, or surviving-mutant threshold for this
milestone. This open gate must not be relabeled as a pass.

At the source commit, the hosted PR matrix executed 17 checks across Linux,
Windows, macOS, MSRV, quality, public-client conformance, dependency review,
security policy, bounded parser fuzzing, stress, packaging, and release
assembly. All 17 passed. The publish-release job was correctly skipped for the
draft PR.

## Same-corpus latency observations

These are warm, single-process, concurrency-one observations pinned to logical
CPU 2. They exclude cold I/O, fsync, local-protocol transport, saturation,
allocation/RSS, and physical power loss. They are not universal latency
promises.

The SQL harness uses the exact source SHA and Rust version as report
arguments. Its indexed route remains inside both halves of the provisional
phase-1 target and is effectively unchanged at p50 from the admitted feature
receipt (-0.228%); the observed p99 is lower by 39.817%.

| SQL operation | p50 | p99 | Provisional target |
|---|---:|---:|---:|
| Physical composite secondary prefix range | 46.429 us | 59.201 us | <= 50 us / <= 250 us |

The integrated sorted-set score-range receipt has the same schema and dataset
digest as the admitted feature receipt:

| Operation | p50 | p99 |
|---|---:|---:|
| Physical `ZCARD` | 0.852 us | 1.971 us |
| Physical middle `ZSCORE` | 1.897 us | 4.188 us |
| Physical head-ten `ZRANGE` | 18.228 us | 46.462 us |
| Physical middle-ten `ZRANGE_BY_SCORE` | 10.221 us | 23.654 us |

Its four p50 values differ from the admitted feature receipt by -3.511% to
+0.518%. The integrated score-bound route remains a microsecond operation.

The integrated rank receipt also has the same schema and dataset digest as its
admitted feature receipt:

| Operation | p50 | p99 |
|---|---:|---:|
| Forward rank of head | 15.351 us | 37.097 us |
| Reverse rank of tail | 10.631 us | 23.674 us |
| Forward rank of middle | 153.990 us | 371.435 us |
| Reverse rank of middle | 134.938 us | 329.137 us |
| Forward rank of tail | 288.196 us | 670.167 us |
| Reverse rank of head | 276.051 us | 584.366 us |

All six p50 values differ from the admitted feature receipt by less than 1%.
The p99 observations were noisier, but every bounded 2,048-member route
remained below one millisecond. The feature evidence explicitly labels these
position-sensitive observations as characterization rather than regression
thresholds.

Raw receipt SHA-256 values:

- SQL:
  `8cbc8ce27afce39b56e1b08844f12de1433ec2f807595f81539a3bcc32f3dd2a`;
- score range:
  `7adf6a909126997831e2dac48fca17d589ba8b776f7d907d2089bdc5f7a31bd1`;
  and
- member rank:
  `ca154fba4b28089fb9c00e22f14cd9e460a3e47dc86969a322be7a32ac3a9c97`.

## Remaining gates

This evidence admits the three feature heads as one reviewable source tree. It
does not merge or promote `dev` or `main`, close the original draft PRs, close
G2/G3/G7, add subtree-count order statistics, add reverse score-range output,
or prove a universal microsecond latency bound. Protected promotion remains an
owner-authorized action after review.
