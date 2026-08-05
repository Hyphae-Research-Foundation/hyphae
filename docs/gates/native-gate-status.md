# Native gate status

This document is the current status index for the ordered Native Phase 1
program. The normative outcomes remain in
[`native-local-phase-1.md`](native-local-phase-1.md); the machine-readable
mirror is [`config/native-gate-status.json`](../../config/native-gate-status.json).

| Gate | Current status | Closure authority |
|---|---|---|
| G0 | Closure claimed; revalidation required | Reconstruct all 8 hosted/governance requirements on one exact commit |
| G1 | Closure claimed; revalidation required | Reconstruct all 7 hosted requirements after G0 authority is restored |
| G2 | Closed at `a839037` | [8/8 hosted bounded relational closure](evidence/closures/native-g2-a839037.json); no universal SQL or official benchmark claim |
| G3 | Closed at `a839037` | [11/11 hosted suite-bound structure closure](evidence/closures/native-g3-a839037.json) |
| G4 | Closed at `0059fce` | [12/12 hosted corpus-bound search closure](evidence/closures/native-g4-0059fce.json) |
| G5 | Closure candidate | 8/8 hosted exact-SHA bounded convergence evidence after predecessor validation |
| G6-G8 | Open | Their exit evidence is not yet defined as retained machine-readable profiles |

`closure-claimed-revalidation-required` preserves a historical closure claim
without treating missing retained evidence as proof. `bounded-readiness-only`
records useful hosted evidence without promoting a deliberately restricted
vertical into the broader normative gate outcome.

A later gate may be implemented and measured early. It cannot be declared
closed until every earlier gate has a retained exact-SHA closure aggregate.
Temporary workflow artifacts are inputs to that aggregate, not durable closure
authority by themselves.

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
passed. These remain local observations rather than hosted gate receipts;
`native-g3.yml` is the authority for exact-SHA G3 evidence.

The all-family test exposed an expiry-index retirement defect in explicit
sorted-set deletion. The deletion path now validates and retires the matching
TTL index entry. One cleanup transaction spanning scalar, hash, set, list,
sorted set, and stream is covered together with reopen and no-op retry.
