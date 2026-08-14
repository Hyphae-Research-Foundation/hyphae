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
