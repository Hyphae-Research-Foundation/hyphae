# ADR-0026: Bounded resampling for calibration measurements

- Status: Accepted
- Date: 2026-08-11
- Owners: Celiums Solutions LLC

## Context

G7 readiness requires one hardware calibration whose `thread_scaling` summary
authorizes a worker count. `summarize_thread_scaling` marks the curve
`unavailable` when any single curve point is not `stable`, and
`NativeGovernorPolicy::derive` then rejects every policy request. One unstable
point therefore aborts the complete performance matrix before any cell runs.

Bare-metal and dedicated-host measurements of scheduler- and
durability-adjacent primitives are dominated by rare tail events: a single
interrupted batch inside 31 samples of 225 ms moves the median absolute
deviation beyond `maximum_relative_mad_ppm` (40_000 ppm thorough) even when
the other 30 samples are quiet. Resampling the whole curve by hand is
undocumented and loses the history of the failed attempt.

## Decision

Each calibration measurement is executed up to
`measurement_retry_limit(policy)` attempts. Attempt 0 is the nominal run.
Attempts 1..N run only when the previous attempt was `unstable`; a `rejected`
(correctness-failed) measurement is never retried. Between attempts the
runner re-executes the warmup batches, so a retry is a fresh measurement, not
sample appending.

The policy bound is 3 attempts in thorough mode and 2 in quick mode. Every
attempt is recorded: the receipt retains per-attempt stability statistics in
`retry_history`, and the final attempt supplies the measurement
`statistics`/ `status` consumed by all derivations. Elapsed retry time is
charged against `policy.maximum_duration_ms`; exhausting the budget aborts
the calibration with the existing deadline error, it never fabricates
stability.

The stability thresholds are unchanged. Retrying never edits statistics,
drops samples, or reconciles a bad curve; it re-measures the point honestly
and keeps the evidence of every attempt.

Non-goals: changing `maximum_relative_mad_ppm` or
`maximum_relative_range_ppm` for any primitive family, retrying correctness
failures, and any cross-attempt sample merging.

## Consequences

- Calibration on dedicated hardware converges with high probability in one
  invocation; a single 10 ms scheduler stall no longer voids a 6-minute
  thorough run.
- The receipt remains self-authenticating: `retry_history` is empty for
  first-attempt-stable points and lists each discarded attempt otherwise, so
  checkers can recompute the exact decision surface.
- Python checkers require the new receipt keys and reject their absence, so
  old receipts remain parseable for shape but fail the current conformance
  revision, which is the intended fail-closed direction for G7/G8 work.
- Aggregate runtime is bounded: worst case is `attempts × sample budget` and
  stays under `maximum_duration_ms`, which callers already enforce.
