# Result proof format v1

Status: normative for Hyphae `0.1.0` proof format `1`.

A result proof is a portable canonical envelope. Its referenced logical
snapshot is the complete state witness; it is not embedded repeatedly in each
proof. All integers are unsigned little-endian.

## Header

The header is exactly 128 bytes.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYPRF001` |
| 8 | 2 | Proof format version (`1`) |
| 10 | 2 | Reserved flags; must be zero |
| 12 | 8 | Snapshot checkpoint sequence |
| 20 | 32 | Snapshot checkpoint commit digest, or all zero when sequence is zero |
| 52 | 32 | Canonical snapshot BLAKE3 digest |
| 84 | 8 | Payload length |
| 92 | 4 | CRC32C of header bytes `0..92` followed by payload |
| 96 | 32 | Domain-separated BLAKE3 of header bytes `0..96` followed by payload |

The proof digest domain is UTF-8 `hyphae-result-proof-v1`. The complete file
length must equal `128 + payload length`; trailing and truncated bytes fail.

## Trusted anchor

The caller-pinned anchor digest is:

```text
BLAKE3(
  "hyphae-proof-anchor-v1" ||
  checkpoint_sequence_le ||
  checkpoint_digest_or_zero ||
  snapshot_digest
)
```

The proof carries the anchor fields but not the trust decision. Trusted
verification requires an expected anchor digest from outside the proof and
snapshot pair. A mismatch is a rollback or wrong-witness failure.

## Payload envelope

| Size | Field |
|---:|---|
| 1 | Operation tag: KV get `1`, structured query `2` |
| 8 | Canonical request byte length |
| N | Canonical request bytes |
| 8 | Canonical result byte length |
| M | Canonical result bytes |

Lengths must consume the payload exactly. The operation tag fixes both the
request and result grammar; mixed variants fail.

## Canonical primitives

- Keys are `u32 length || nonempty bytes` and obey the storage key limit.
- A structured value is the complete canonical `HYDOC001` encoding from
  [`document-format-v1.md`](../storage/document-format-v1.md), prefixed by its
  `u64` byte length.
- A field path is a `u16` segment count followed by `u32 UTF-8 length || bytes`
  for every nonempty segment. A zero count selects the root value.
- Optional values use tag `0` for missing/null-normalized cursor state and tag
  `1` followed by a canonical structured value. Other tags fail.
- Collection counts are fixed-width and validated against verifier limits
  before allocation.

## KV get

The request is one canonical key. The result starts with presence tag `0` or
`1`; a present result then contains the same canonical key and one structured
value. The verifier scans the complete sorted snapshot, so both presence and
absence are proven for the anchored state.

## Structured query

The request encodes the complete query AST in field order: filter, sort list,
optional cursor, page limit, and optional aggregation plan. Variant tags are
stable and defined by the reference codec tests. Recursive filters and all
collection counts remain subject to the phase-3 shape limits.

The result encodes, in order:

1. every returned key and structured value;
2. the optional continuation cursor;
3. the optional complete aggregation result;
4. global scanned-record and matched-record counts.

The verifier loads the snapshot, verifies every document, executes
[`reference-semantics-v1.md`](../query/reference-semantics-v1.md), and requires
exact equality with this result. This detects result-row insertion, deletion,
editing, and reordering in addition to snapshot corruption.

## Resource limits

Proof byte length, snapshot byte length, snapshot entries, decoded document
bytes, filter nodes, rows, aggregation groups, and verification time are all
bounded before or during allocation. Exhausting a limit is a verification
error and never returns a partially verified result.

`read_result_proof` accepts only a regular file. It checks path and opened-file
metadata, rejects an oversized length before allocation, reads no more than
the smaller caller/canonical proof limit plus one detection byte in bounded
chunks, and rejects a file whose observed length changes during the read. This
standalone decoder is byte bounded but has no timeout parameter.

The verifier subtracts proof parsing from one cooperative deadline, then
uses the same bounded reader with deadline checks between chunks and propagates
the remaining duration through snapshot verification/loading and query replay.
Snapshot file length and declared logical counts are checked on the opened
handle before the canonical scan; decoded bytes are accounted during that scan.

## Security properties and limits

CRC32C detects accidental corruption quickly. BLAKE3 binds proof bytes,
snapshot bytes, and log checkpoint identities. Neither is a signature.
Replay or rollback is detected only when the verifier supplies the expected
anchor digest. An attacker who controls that trusted expectation can replace
the entire history; see the baseline threat model.
