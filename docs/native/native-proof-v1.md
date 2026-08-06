# Native result proof v1

Status: accepted G6 planning contract; implementation incomplete

Native proofs make bounded product results independently verifiable without
turning a self-consistent proof into an external trust anchor. Callers pin the
expected root-set or anchor digest through a trusted channel.

## Common binding

Every proof binds:

- proof kind and version;
- canonical request digest;
- directory lineage and history epoch;
- visible CSN, catalog version, and immutable root-set identity;
- relevant stable object IDs and definition digests;
- execution semantics and canonical ordering version;
- ordered result digest;
- admitted limits and completion status; and
- required witness references and digests.

Proof kinds cover point reads, bounded SQL, lexical search, exact vector,
filtered exact vector, ANN, hybrid, filtered/faceted search, and catalog
inspection.

## Exact and approximate proofs

An exact proof permits offline verification or bounded reexecution of the
claimed exact semantics against its witness.

An ANN proof proves provenance and faithful execution of a declared
approximate algorithm. It additionally binds vector metric, index definition,
graph generation digest, search breadth, filter strategy, eligible-set digest,
visited/candidate/rerank counts, and approximation label. It does not prove
that omitted vectors could not be closer. When an exact oracle receipt is
included, recall is a separate measured statement bound to that oracle.

A hybrid proof binds every branch proof, branch failure policy, fusion method,
weights, candidate limits, duplicate handling, and final ordered result.

## Offline verifier

Verification succeeds after the originating data directory is unavailable.
Readers enforce file, decoded-byte, candidate, depth, result, and deadline
limits before materialization. Tampering with request, rank, score, object ID,
catalog identity, CSN, root set, graph generation, fusion, or witness fails.

## Non-claims

The proof does not establish that the originating machine was uncompromised,
that the caller pinned a trustworthy anchor, or that an approximate result is
the exact nearest-neighbor set.

## Verification

Golden encodings, independent decoder tests, tamper matrices, unavailable-
origin verification, size/deadline failures, and exact-versus-approximate
negative controls are mandatory G6 evidence.
