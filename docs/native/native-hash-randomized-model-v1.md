# Native hash randomized-model gate v1

Status: contract frozen; implementation and evidence pending.

This gate defines a deterministic state-machine comparison between the
independent logical `StructureState` model and the native hash engine's
private, retained-snapshot, current-root physical, and reopened execution
surfaces. It covers the accumulated hash command, scan, lifecycle, whole-hash
TTL, and field-TTL semantics before more structure families are expanded.

It is a verification contract, not a new public command, physical format, WAL
opcode, dependency, or compatibility claim.

## Deterministic trace corpus

The test owns a small dependency-free SplitMix64 generator whose transition
and byte-selection rules are fixed in the test source. The checked default
corpus executes:

- 16 fixed nonzero seeds recorded in source;
- 256 committed steps per seed;
- four exact binary hash keys;
- 32 exact binary fields, including empty, NUL, `0xff`, shared prefixes, and
  prefix-boundary identities;
- canonical signed-decimal and arbitrary binary values; and
- monotonically nondecreasing signed logical time with exact expiry-boundary
  hits.

Every failure reports the seed, seed ordinal, step, logical time, action, and
hex-encoded identities needed to replay the exact trace prefix. The default
corpus must not depend on wall clock, thread scheduling, operating-system
randomness, hash-map iteration order, or an environment variable.

An optional explicitly named developer test may accept one seed for focused
replay, but CI authority remains the checked fixed corpus.

## Action grammar

The generator selects among:

- create and delete complete hashes;
- absolute whole-hash expiry;
- singular and bounded multi-field set;
- singular and bounded multi-field delete;
- signed field increment, including noninteger and overflow rejection;
- absolute field expiry, including immediate-due and re-expiry;
- logical-time advance and exact expiry-boundary evaluation; and
- read-only point, TTL, cardinality, ascending, descending, and binary-glob
  probes.

Generated mutation batches are bounded by the public command limits. Duplicate
caller positions are retained for multi-read and excluded where the mutating
contract rejects duplicates. Invalid structure-kind fuzzing, oversized
identities, malformed glob syntax, injected corruption, and crash
interruptions remain in their focused gates rather than being silently
normalized by this model test.

## Outcome equivalence

Each mutation step begins from one cloned oracle state and one retained native
snapshot. The same action is evaluated against the logical model and a native
private transaction at the same logical time.

The gate compares:

1. the exact success/no-op/error class and command result before commit;
2. every visible key and field through the private transaction;
3. the retained pre-step snapshot after native publication;
4. a newly materialized snapshot at the step logical time;
5. current-root physical point, TTL, cardinality, and scan results; and
6. the same current state after periodic close and reopen.

An operation rejected by either side must leave that side's candidate state
unchanged. A native mutation may not be committed merely to make the final
state match after a divergent command result.

Missing, persistent, and positive remaining TTL are distinct results.
Equality with a whole-hash or field expiry is missing. Updating a due field is
an add and clears its field expiry. Whole-hash expiry dominates field expiry.

## Scan equivalence

For each visible hash, the oracle's complete exact-byte field order is the
result authority. The gate exhausts:

- ascending scans with exclusive live, due, deleted, below-range, and
  above-range cursors;
- descending scans with the corresponding exclusive cursors; and
- exact, leading-literal-prefix, and leading-wildcard binary-glob pages.

Page sizes and visit limits vary deterministically. Empty non-exhausted
pattern pages must advance their exact continuation. Due fields consume
physical visits but never appear in output, and they consume no matcher steps.
Per-page segmentation may differ between the materialized model and the
physical B+tree where tombstones exist; eventual ordered live results,
continuation progress, stop reason, physical visits, and matcher-step rules
must satisfy their respective contracts.

## Reopen and durability boundary

The corpus uses `DurabilityClass::Memory` so it tests native page/WAL
publication and recovery without turning filesystem synchronization latency
into test noise. The database closes and reopens after every 32 steps and at
the end of every seed. Each reopen must reproduce the oracle's logical state
at the same time.

This is semantic restart equivalence, not strict-fsync, process-kill,
block-replay, EC2-stop, or physical-power-loss evidence. Those remain in the
existing crash and durability gates.

## Required evidence

The slice is not complete until:

- a compiler-reaching red gate names the missing randomized harness;
- the fixed seed manifest and generator algorithm are checked in;
- at least 4,096 deterministic actions execute against both authorities;
- private, retained, materialized, physical, and reopened comparisons are all
  reached by every seed;
- whole-hash and field-expiry equality boundaries are reached;
- singular and multi-field commands, counters, ascending/descending scans,
  and all three pattern routes are reached;
- a deliberately perturbed oracle or expected result proves the harness can
  detect and report one exact seed/step divergence;
- the complete native-runtime and workspace test/clippy/documentation gates
  pass on direct Linux; and
- the evidence receipt records commit, tree, Rust version, host, seed list,
  action counts, reopen counts, comparison counts, elapsed time, and explicit
  exclusions.

## Boundaries

Passing this gate does not prove exhaustive state space, arbitrary production
workloads, concurrent optimistic writers, scheduler fairness, memory
amplification, saturation, protocol exposure, or complete G3/G7. It does not
replace focused corruption, crash-boundary, or benchmark evidence. A seed
corpus is reproducible evidence over its declared distribution, not proof
that ungenerated states are correct.
