# Native local telemetry v1

Status: implemented G6 contract

Telemetry is an optional local observation surface. It is not durable data
authority and requires no cloud service, metrics database, or exporter.

## Model

The product facade owns a bounded registry of monotonic counters, gauges,
bounded histograms, and structured lifecycle events. A snapshot reports a
registry version, process start identity, process-local session/open identity,
capture time, catalog version, and stable metric rows.

Required timing families remain separate:

- admission and queueing;
- parse, bind, optimize, or prepared lookup;
- engine execution;
- local or HTTP transport;
- request decoding, result encoding, proof construction, and proof verification;
- WAL append;
- page synchronization;
- WAL synchronization; and
- the complete selected durability boundary.

Metric IDs are fixed numeric discriminants in registry order. New metrics append
IDs; callers cannot register metrics or labels. Counters, histogram counts,
sums, buckets, and dropped-event counts saturate at `u64::MAX`.

Required operational families include scheduler saturation, active expiry,
checkpoint, compaction, vacuum, retention, blob collection, ANN consolidation,
backup, restore, cancellation, deadline, error category, recovery, and doctor.

## Cardinality and privacy

Labels come only from a fixed registry plus bounded stable object kinds.
Arbitrary keys, field values, document text, SQL text, credentials, request
IDs, host paths, and user-provided names are prohibited labels. Per-object
diagnostics use bounded admin queries rather than permanent metric labels.

Counters define overflow behavior and snapshots are internally consistent
under concurrent updates. Disabling optional event capture preserves required
counters and has a measured bounded overhead.

## Exposure

Embedded administration returns typed snapshots. CLI and HTTP `/v2` can render
or encode the same snapshot. Local-protocol and SDK access follows the same
authorization policy. Optional Prometheus or OpenTelemetry exporters may be
added later as public adapters; they are not G6 core dependencies.

The `Telemetry` product operation is the cross-surface authority. Embedded,
native-local, HTTP `/v2`, CLI, and SDK adapters encode the same snapshot type.
Transport adapters record only fixed decode, encode, and transport clocks into
the registry owned by the sole product service.

## Verification

Tests cover monotonicity, histogram bounds, concurrent snapshots, saturation,
redaction, cardinality limits, disabled-mode overhead, restart identity, and
cross-surface equality.
