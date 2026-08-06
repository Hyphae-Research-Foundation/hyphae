# Native local telemetry v1

Status: accepted G6 planning contract; implementation incomplete

Telemetry is an optional local observation surface. It is not durable data
authority and requires no cloud service, metrics database, or exporter.

## Model

The product facade owns a bounded registry of monotonic counters, gauges,
bounded histograms, and structured lifecycle events. A snapshot reports a
registry version, process start identity, capture time, catalog version, and
stable metric rows.

Required timing families remain separate:

- admission and queueing;
- parse, bind, optimize, or prepared lookup;
- engine execution;
- local or HTTP transport;
- result encoding and proof construction;
- WAL append;
- page synchronization; and
- WAL synchronization.

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

## Verification

Tests cover monotonicity, histogram bounds, concurrent snapshots, saturation,
redaction, cardinality limits, disabled-mode overhead, restart identity, and
cross-surface equality.
