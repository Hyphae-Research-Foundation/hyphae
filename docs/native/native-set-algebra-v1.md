<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native set algebra v1

Status: contract implemented and evidenced on direct Linux; hosted CI remains
separate from the local receipt.

This contract adds bounded read-only algebra to Hyphae's existing native
binary set family. It does not wrap Valkey, add a sidecar, project through SQL,
introduce a WAL opcode, or mutate a destination set.

## Public surface

The embedded native runtime adds one checked request and three explicit
operations:

```text
SET_UNION(keys, output_member_limit, visit_limit)
SET_INTERSECTION(keys, output_member_limit, visit_limit)
SET_DIFFERENCE(keys, output_member_limit, visit_limit)
    -> Complete(members, visited)
    | OutputLimitExceeded
    | VisitLimitExceeded
```

The Rust surface uses a shared `SetAlgebraRequest`, a
`SetAlgebraOperation` enum, and one `SetAlgebraResult`. Private write batches,
retained/materialized snapshots, and current-root physical reads expose the
same operation and result semantics.

Private batches and snapshots use their captured logical time. The
current-root physical surface requires an explicit logical time so an expired
scalar or hash incarnation is missing rather than a live wrong-kind input.

This version returns only a complete result. It never returns an ambiguous
partial prefix and has no cursor. Pagination, destination-set store variants,
and protocol exposure require separate contracts.

## Input and resource bounds

A request must satisfy all of these rules before reading member data:

- one through 64 caller positions;
- each key is one exact binary identity admitted by the native set metadata
  namespace;
- `output_member_limit` is in `1..=65,536`; and
- `visit_limit` is in `1..=1,048,576`.

Caller positions are semantically significant for difference and therefore
are not globally deduplicated. Repeated positions in union and intersection
are accepted and do not change the mathematical result.

The output member limit bounds the retained result set. The visit limit bounds
physical or model work before another member or membership probe is performed.
Exceeding either bound fails the complete call and returns no members.

Member bytes remain subject to the existing canonical B+tree identity bound.
The implementation must not copy unrelated values, rows, hashes, lists,
sorted sets, search state, or the complete structure engine.

## Type and missing-key semantics

Every caller position is type-checked before algebra execution, including
positions whose value could be skipped by an empty intersection.

- an existing native set participates normally;
- a missing key is an empty set;
- a live scalar, hash, list, or sorted-set key returns
  `StructureKindMismatch`; and
- a legacy whole-state structure directory returns
  `LegacyStructureFamilyUnsupported`.

An explicitly typed empty set is distinct from a missing key for catalog and
mutation purposes, but both contribute the empty mathematical set to this
read-only operation.

## Exact operation semantics

Members are arbitrary binary strings. Equality and ordering use exact byte
comparison. Every successful result is strictly ascending with no duplicates.

`SET_UNION(k0, ..., kn)` returns every member present in at least one
participating set. Missing and empty inputs contribute no members.

`SET_INTERSECTION(k0, ..., kn)` returns every member present in every
participating set. Any missing or empty input produces a complete empty result
after all input kinds have been validated.

`SET_DIFFERENCE(k0, k1, ..., kn)` is ordered and returns members present in
`k0` and absent from every later input. A missing or empty first input returns
empty. Missing later inputs subtract nothing. Repeating `k0` in a later
position therefore returns empty.

Integer cardinality arithmetic must be checked. No operation may silently
truncate an output, wrap a counter, or change result order based on set
cardinality, insertion order, tombstone layout, hash-map iteration, or
platform.

## Execution strategy

The logical model may use its ordered native set representation. The physical
surface must operate directly on the set metadata/member B+tree namespaces.
It may not reconstruct `StructureState`.

The required first implementation uses these bounded strategies:

- union visits each source member prefix and inserts live members into one
  ordered result set, stopping before it would exceed either request bound;
- intersection chooses one live input with the smallest declared cardinality,
  visits that candidate prefix, and performs exact physical membership probes
  against every other input; and
- difference visits only the first input's member prefix and probes later
  inputs until a subtracting membership is found.

Choosing a different equal-cardinality intersection source must not change the
result. The deterministic implementation uses the lowest caller position as
the tie breaker.

One source-member envelope encountered by a range visit consumes one visit,
including a tombstone. One exact membership lookup consumes one visit whether
the member is present, absent, or tombstoned. Metadata/type preflight does not
consume member visits.

Private and materialized model surfaces have no physical tombstones, so their
reported visit counts may be lower than current-root physical counts. Result
members and success/error class remain the cross-surface equivalence
authority.

## Physical validation

Every reached metadata envelope, member identity, and member value is decoded
with the existing canonical codecs.

A complete source-prefix visit verifies that its live member count equals the
declared metadata cardinality. A bounded call that fails on its visit or
output limit does not claim complete cardinality validation. A point probe
validates the exact reached member envelope but does not imply that unrelated
members in that set were scanned.

Malformed reached identities, values, metadata, impossible cardinalities, or
B+tree pages fail closed with the existing typed runtime errors. A corrupt
input may not be normalized to missing, empty, or a partial result.

## Snapshot and concurrency boundary

Private-batch algebra includes the batch's read-your-writes set mutations.
A retained snapshot remains fixed at its captured root. Current-root physical
execution captures one root set at the caller's explicit logical time before
preflight and uses it for the complete operation, so concurrent publication
cannot mix commit sequences across input sets.

The operation is read-only. It publishes no conflict identities, consumes no
CSN, changes no TTL, and cannot make a rejected write batch dirty.

## Required evidence

This slice is not complete until:

- a compiler-reaching red gate names the missing checked request or runtime
  surface;
- union, intersection, and ordered difference match an independent exact
  oracle for binary members, empty sets, missing keys, duplicates, and
  repeated first-position difference;
- private read-your-writes, retained snapshots, materialized snapshots,
  current-root physical state, and reopen produce identical complete members;
- zero and over-hard-limit requests fail before member visitation;
- output and visit exhaustion return no partial members;
- all input positions are type-checked before empty-result short-circuiting;
- multilevel member trees prove physical range pruning and deterministic
  smallest-set intersection choice;
- reached metadata, member identity, member value, tombstone, cardinality, and
  page corruption fail closed;
- the complete native-runtime and workspace test/clippy/documentation gates
  pass on direct Linux; and
- one direct-Linux receipt separates small-set embedded/snapshot/physical
  latency from large-cardinality work and records visits, tree height, commit,
  tree, Rust version, host, and explicit exclusions.

## Boundaries

Passing this slice does not add `SUNIONSTORE`, `SINTERSTORE`, `SDIFFSTORE`,
set TTL, per-member TTL, probabilistic sets, streams, blocking behavior,
network compatibility, randomized models for other families, or complete G3
or G7.

The small-set hot path is a measured microsecond-first objective, not a
universal bound. Large union output and large first/smallest-set scans are
cardinality-sensitive by definition.

The implementation and measured gate are bound by the
[native set algebra Linux evidence](../gates/evidence/native-set-algebra-linux-2026-08-03.md).
