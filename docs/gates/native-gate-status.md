# Native gate status

This document is the current status index for the ordered Native Phase 1
program. The normative outcomes remain in
[`native-local-phase-1.md`](native-local-phase-1.md); the machine-readable
mirror is [`config/native-gate-status.json`](../../config/native-gate-status.json).

| Gate | Current status | Closure authority |
|---|---|---|
| G0 | Closure claimed; revalidation required | Reconstruct all 8 hosted/governance requirements on one exact commit |
| G1 | Closure claimed; revalidation required | Reconstruct all 7 hosted requirements after G0 authority is restored |
| G2 | Bounded readiness only | The current 8/8 workflow does not claim broad SQLLogicTest, canonical TPC-H, or canonical TPC-C completion |
| G3 | In progress | 11/11 hosted, suite-bound, exact-SHA evidence after prior-gate authority is restored |
| G4-G8 | Open | Their exit evidence is not yet defined as retained machine-readable profiles |

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

The all-family test exposed a real open defect: one active-expiry cleanup call
that spans all six structure families returns `InvalidStructureTree`. Logical
expiry visibility and reopen semantics remain fail-closed and covered. G3 must
not be declared closed until the cleanup path is repaired and represented in
the suite manifest.
