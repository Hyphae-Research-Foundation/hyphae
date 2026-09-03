<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Formal models

Machine-checked models of Hyphae's core protocols. A model is evidence about
the protocol as specified, not a proof of the Rust implementation; each model
states its abstraction boundary and the implementation anchors it mirrors.

## HyphaeCommit — cross-engine commit protocol

- Spec: [`HyphaeCommit.tla`](HyphaeCommit.tla), model:
  [`HyphaeCommit.cfg`](HyphaeCommit.cfg).
- Mirrors: the ordered commit boundary walk (`CommitBoundary`,
  `crates/hyphae-native-runtime/src/lib.rs`), serialized writer admission
  with first-committer-wins, per-transaction durability classes
  (`DurabilityClass`, fsync gating in `commit_report_at`), sequential WAL
  recovery with broken-tail truncation, and conflict-table reconstruction —
  the same state machine the runtime crash matrix
  (`tests/all_engine_transaction_g5.rs`) exercises physically.
- Checked invariants: cross-engine atomicity (no partial commit is ever
  observable, including through every modeled crash/recovery shape), strict
  durability (an acknowledged `Strict` commit survives every crash),
  first-committer-wins, contiguous visible CSN prefix, and type safety.
- Crash model: a crash truncates the volatile WAL suffix at an arbitrary
  prefix-closed point; fsync is file-wide; `Memory` commits acknowledge
  without entering the durable set, so the model demonstrates (rather than
  hides) that a crash may drop acknowledged `Memory` commits while never
  splitting one.

Run with TLC (Java 11+):

```bash
java -XX:+UseParallelGC -jar tla2tools.jar \
  -config docs/formal/HyphaeCommit.cfg docs/formal/HyphaeCommit.tla \
  -workers auto -deadlock
```

`-deadlock` is required: the model intentionally has terminal states (all
transactions used, crash budget exhausted) that are not errors.

Checked results are recorded as evidence receipts under
[`docs/gates/evidence/`](../gates/evidence/) with the exact spec digest,
TLC version, state counts, and wall time.
