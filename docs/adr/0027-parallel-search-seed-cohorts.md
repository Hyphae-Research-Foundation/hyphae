# ADR-0027: Parallel search-seed staging with ordinal commits

- Status: Accepted
- Date: 2026-08-12
- Owners: Celiums Solutions LLC

## Context

G7 builds its million-document lexical and filter corpus through the real
detached-delta and commit path before the initial ANN bulk build. The former
loop staged and committed one 512-document batch at a time. Staging left the
remaining bare-metal cores idle and dominated fixture construction, but commit
publication must remain single-writer and deterministic.

The installed governor is authoritative for mutation concurrency. Its
Mutation-class limit is at most two, but the effective limit can be one when
the calibrated global or class I/O or compute budget is one. Treating the
maximum as guaranteed capacity either fails admission or stalls a queued
implementation while an uncommitted batch retains the first permit.

## Decision

The runner derives a bounded cohort plan from process-visible parallelism and
the installed governor's effective global and Mutation-class compute and I/O
limits. The cohort count is clamped to one or two. Each detached delta retains
its own governor permit and continues to enforce the engine's per-batch memory
limit; no ungoverned staging buffer is introduced.

Each window stages disjoint batches concurrently. Every result crosses the
channel as `(batch_index, result)`. The sequencer sorts by `batch_index` and
commits strictly in ascending ordinal order, independently of worker completion
order. Progress advances only after the corresponding commit succeeds.

The plan uses 512-document batches and the stable partition rule
`batch-index-modulo-cohort-count-ordinal-commit-v1`. Those values are emitted in
the open `details` object of the `search-seed-lexical` progress checkpoint. The
exact-field `hyphae-native-g7-initial-ann-bulk-v1` evidence remains unchanged;
changing that authoritative schema requires a separate version and checker
update. Dataset identity remains a function of canonical corpus inputs, not
host-dependent staging concurrency.

The complete seed, ANN evidence, vacuum, and checkpoint are still written to a
private staging directory. Publication remains one directory rename after all
required evidence exists. A failed cohort never creates a reusable shared seed.

## Consequences

- Logical documents, filter values, commit order, and reopened state match the
  one-cohort path.
- Only staging overlaps. The commit sequencer remains serial, so lexical-stage
  speedup is bounded below 2x and must be measured rather than claimed.
- A one-slot governor automatically selects one cohort and retains the former
  serial behavior.
- At most two 32 MiB-capped detached delta batches can be live concurrently on
  the current engine contract; each remains fail-closed under retained-memory
  admission.
- Progress identifies the actual cohort plan used by a newly built seed without
  widening the final G7 receipt or ANN evidence schema.
