# Native result proof v2

Status: implemented G6 native operation proof

`HYNPRF02` is the canonical, bounded proof envelope for native product results.
Version 2 is intentionally incompatible with `HYNPRF01`: object bindings now
carry the complete canonical 128-bit `ObjectId`, and recognized operation
proofs carry canonical executable requests rather than arbitrary caller bytes.
The v2 decoder does not claim to decode v1.

## Trust boundary

Self-consistency is not trust. Verification requires an
`ExternalTrustedAnchor` obtained independently of the proof and witness. The
domain-separated anchor binds:

- 24-byte directory lineage and nonzero history epoch;
- visible CSN and catalog version;
- complete immutable all-engine root digest; and
- durable checkpoint visible CSN and manifest digest.

The producer checkpoints the exact root that produced the result while it owns
the native directory lock. The verifier requires the reopened retained
authority's lineage, root, catalog version, visible CSN, and latest verified
checkpoint authority to equal the anchor.

## Envelope

All integers are unsigned little-endian. The 64-byte header is:

| Offset | Bytes | Meaning |
| --- | ---: | --- |
| 0 | 8 | ASCII magic `HYNPRF02` |
| 8 | 2 | format version `2` |
| 10 | 2 | flags, zero in v2 |
| 12 | 1 | proof-kind tag |
| 13 | 1 | completion tag |
| 14 | 2 | reserved zero |
| 16 | 8 | exact payload bytes |
| 24 | 4 | CRC32C over header bytes 0..32 with this field zero plus payload |
| 28 | 4 | reserved zero |
| 32 | 32 | BLAKE3 envelope digest |

The digest domain is `hyphae-native-proof-envelope-v2`. The file length must
equal the declared payload plus 64 bytes. Alternate encodings, truncation, and
trailing bytes are invalid.

## Payload

The canonical payload contains, in order:

1. The fixed-width trusted anchor.
2. Nonzero execution-semantics and ordering versions.
3. Admitted result-item, candidate-item, and evidence-byte limits.
4. The exact `HYNWIT02` digest and byte length.
5. Strictly increasing object bindings. Each binding is a nonzero `u128`
   `ObjectId` followed by the digest of the canonical definition bytes.
6. Canonical request, ordered-result, and execution-evidence sections. Each is
   encoded as exact length, digest, and complete bytes.
7. ANN or hybrid metadata when required by the proof kind.

Section digests use `hyphae-native-canonical-bytes-v1`; the section bytes are
defined by the v2 operation semantics, not by JSON, `Debug`, or a transport
codec.

## Integrated operations

`generate_native_operation_proof` and `ProductOperation::Prove` execute the
actual product read once, capture its `ProductRead`, SQL snapshot, catalog page,
or integrated-search snapshot, checkpoint that same root, and generate the
proof and witness. The integrated set is:

- point catalog lookup by stable ID;
- bounded SQL `SELECT` with canonical typed parameters;
- bounded compound lexical search with the exact work limits used;
- integrated exact-vector, ANN, and hybrid search;
- bounded catalog list; and
- catalog describe.

Prepared handles, mutations, name-only catalog point operations, catalog
dependencies/resolve, and non-result operations are rejected by this proof
surface. They are not silently reduced to artifact-only claims.

Canonical result bytes preserve executor order, hit order, facet bucket order,
aggregation order, catalog stable-ID order, cursor identity, and every typed
scalar. Canonical evidence binds the exact snapshot plus operation-specific
work counts and strategy receipts.

## Proof kinds

| Tag | Kind | Reexecuted claim |
| ---: | --- | --- |
| 1 | point | catalog object bytes at the anchored root |
| 2 | SQL | ordered bounded SQL rows and typed values |
| 3 | lexical | ordered hits and lexical work counters |
| 4 | exact vector | exact filtered vector result and strategy receipt |
| 5 | ANN | declared approximate algorithm and resulting ordered output |
| 6 | hybrid | all declared branches and deterministic RRF result |
| 7 | catalog | bounded list or complete describe output |

## ANN and hybrid

ANN proofs remain explicitly approximate. They bind the metric, canonical
collection/index definition, native base-plus-delta build identity, search
breadth, exact-seeded post-filter strategy, anchored eligibility
predicate/count digest, visited count, ANN candidate count, rerank count, and
approximation label. The
offline verifier reruns the same request and reconstructs all metadata. A
self-consistent proof that changes search breadth, graph identity, strategy,
counts, or eligibility evidence fails semantic verification.

An ANN proof proves faithful execution of the declared approximate algorithm;
it does not prove that omitted vectors could not be closer.

Hybrid proofs bind every lexical/vector branch in request order, including
branch bytes, weight, and candidate limit. Current product hybrid execution is
fail-closed weighted reciprocal-rank fusion with merge-by-object-ID duplicate
handling. The complete integrated operation is rerun, so both branches,
strategy receipts, fused ordering, facets, aggregations, and evidence must
match.

## Offline verification

`verify_native_proof_offline` always performs strict artifact verification:

1. decode and canonical re-encode both artifacts under explicit limits;
2. verify CRC32C, envelope, section, inventory, and file digests;
3. compare the proof anchor to the independently supplied anchor;
4. require identical proof/witness anchors and exact witness reference; and
5. account for every retained file and directory.

For a recognized `HYOPRQ02` request it additionally:

1. extracts the complete witness into a new private temporary directory;
2. opens that directory through `NativeProduct::open`, which verifies native
   WAL, manifests, pages, blobs, catalog, and all-engine roots;
3. checks retained root and durable checkpoint authority against the proof;
4. decodes the canonical operation under `NativeVerificationLimits`;
5. reexecutes it against the retained root and logical time;
6. compares canonical ordered result, evidence, and object bindings; and
7. reconstructs and compares ANN or hybrid metadata.

Only this path returns scope `SemanticReexecution` and
`semantic_reexecution_performed = true`. A manually constructed artifact with
opaque sections remains scope `ArtifactIntegrity` and cannot satisfy an
operation-proof requirement. A recognized semantic request that cannot be
opened, executed, or matched is an error, never an artifact-only success.

## Bounds and non-claims

Proof, witness, section, object, branch, decoded-byte, result-item, candidate,
and semantic-reexecution bounds are explicit. Witness extraction uses safe
canonical relative paths and create-new files.

The proof does not establish that the producer host was uncompromised, that the
external anchor channel is trustworthy, or that ANN is exact. Semantic replay
is correctness-first and may perform bounded full-state materialization; it is
not a microsecond-first hot path.

## Verification evidence

Product tests cover v2 round trips, v1 magic rejection, complete 128-bit high
IDs, origin deletion, trusted-anchor mismatch, witness substitution, every
strict prefix truncation, trailing bytes, codec bounds, SQL and lexical replay,
catalog list/describe replay, exact-vector replay, ANN declared-algorithm
replay, hybrid branch replay, and self-consistent result or ANN-metadata
forgeries.
