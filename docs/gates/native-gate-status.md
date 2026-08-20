# Native gate status

This document is the current status index for the ordered Native Phase 1
program. The normative outcomes remain in
[`native-local-phase-1.md`](native-local-phase-1.md); the machine-readable
mirror is [`config/native-gate-status.json`](../../config/native-gate-status.json).

| Gate | Current status | Closure authority |
|---|---|---|
| G0 | Closed at `14b4ec9` | [8/8 hosted and governance exact-SHA closure](evidence/closures/native-g0-14b4ec9.json) |
| G1 | Closed at `14b4ec9` | [7/7 hosted exact-SHA substrate closure](evidence/closures/native-g1-14b4ec9.json) |
| G2 | Closed at `a839037` | [8/8 hosted bounded relational closure](evidence/closures/native-g2-a839037.json); no universal SQL or official benchmark claim |
| G3 | Closed at `a839037` | [11/11 hosted suite-bound structure closure](evidence/closures/native-g3-a839037.json) |
| G4 | Closed at `0059fce` | [12/12 hosted corpus-bound search closure](evidence/closures/native-g4-0059fce.json) |
| G5 | Closed at `b7cf651` | [8/8 hosted exact-SHA convergence closure](evidence/closures/native-g5-b7cf651.json) |
| G6 | Closed at `c57cc07` | [14/14 requirements and 42/42 exact-SHA platform cells](evidence/closures/native-g6-c57cc07.json) |
| G7 | Closed at `ff188af` | [11/11 surfaces and 33/33 C-60 operational-scale cells](evidence/closures/native-g7-ff188af.json); environment-bound, with no dedicated-hardware, interference, or canonical-latency certification |
| G8 | Closed at `e88f2ea` | [Nine-requirement exact-SHA release closure](evidence/closures/native-g8-e88f2ea.json); Linux/macOS/Windows functional evidence, crash/corruption/resource/power-loss matrices, four signed packages, SBOM/provenance, migration, soak, and independent restore all passed |

`bounded-readiness-only` records useful hosted evidence without promoting a
deliberately restricted vertical into the broader normative gate outcome.

G0 through G7 form the ordered product-readiness prefix. G8 closed independently
on exact release commit `e88f2ea` through readiness run `31796827556`, signed
release run `31797867994`, and fail-closed aggregate run `31798866604`. G7
closed from the source-bound DigitalOcean C-60
run after the final bare-metal attempt failed before product measurement. The
closure certifies the million-observation operational-scale matrix, concurrency,
correctness, ANN recall, accounting, and durable recovery. It does not certify
the canonical dedicated-hardware latency targets, background interference, or
bare-metal performance.
Temporary workflow artifacts are inputs to an aggregate, not durable closure
authority by themselves. The retained G8 aggregate is byte-identical to the
official closure artifact and binds every constituent receipt by SHA-256.

## Scope of the closed G7 profile for 1.2.x

G7 closed on a pre-1.2 commit, before durable access control entered the
commit path. The closed G7 profile therefore certifies the operational-scale
matrix of the engine without per-operation authorization; the 1.2 access
control adds work to the measured commit and read paths that G7 did not
observe. Performance claims for 1.2.x releases must either cite this scoped
profile explicitly or wait for a G7 re-execution on a 1.2.x release commit.

## 1.2.x exact-SHA G8 release closures

Each 1.2.x release repeated the nine-requirement exact-SHA G8 closure on its
own release commit through the readiness, signed-release, and fail-closed
closure workflows. The aggregates were retained as run artifacts and are
bound by SHA-256 into each release's publication evidence:

| Release | Commit | Readiness run | Release run | Closure run | Aggregate SHA-256 |
|---|---|---|---|---|---|
| `v1.2.0` | `fc48d27` | `32200238284` | `32200240005` | `32201136285` | `e3cb7eaca04e2c28681b2ff5f969731df9a6cfd8bfcc4e3c82f9a9a9d8e2c6a1` |
| `v1.2.1` | `bc971c5` | `32213097522` | `32213098884` | `32214588620` | `e045430c3a99a2cf3d81a45c730031f741c094549375a2c3ecc3d033ef3a06cb` |
| `v1.2.2` | `0471ae2` | `32253721973` | `32253724833` | `32256031172` | `aa7a76f3e8aacc87a8da4afb412f7c8fb152b1703975da22bda1f74deaa44543` |

The `v1.2.2` closure aggregate additionally anchors the crates.io, npm, and
PyPI registry publications of `1.2.2`; the PyPI publication receipt binds it
by the same digest.

## G3 local validation record

The implementation checkout that introduced the G3 evidence lane completed
the following local validation before hosted execution:

- workspace format check;
- workspace Rust tests with all features;
- workspace rustdoc with warnings denied;
- all Python evidence-tool tests;
- the all-family atomicity, controlled-expiry visibility, and restart matrix;
- the mixed-family physical-amplification regression test.

The final workspace test run completed all workspace unit, integration, and
doc tests, including the 380 native-runtime unit tests, without a failure.
Workspace Clippy, format, rustdoc, and all Python evidence-tool tests also
passed. These remain local observations rather than hosted gate receipts. The
retained G3 closure summary above binds the exact-SHA output from
`native-g3.yml`.

The all-family test exposed an expiry-index retirement defect in explicit
sorted-set deletion. The deletion path now validates and retires the matching
TTL index entry. One cleanup transaction spanning scalar, hash, set, list,
sorted set, and stream is covered together with reopen and no-op retry.
