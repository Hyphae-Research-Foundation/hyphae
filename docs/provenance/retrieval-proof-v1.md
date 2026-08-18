<!-- SPDX-License-Identifier: Apache-2.0 -->
# Retrieval proof format v1

Status: normative for Hyphae `0.2` exact, lexical, and hybrid retrieval.

A retrieval proof is a canonical envelope. Its referenced format-2 logical
snapshot is the complete witness and is retained separately. All integers are
little-endian.

## Header

The header is 132 bytes.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYRPF001` |
| 8 | 2 | Proof format version `1` |
| 10 | 2 | Reserved flags; zero |
| 12 | 2 | Operation: exact `1`, lexical `2`, hybrid `3` |
| 14 | 2 | Operation semantics version |
| 16 | 8 | Snapshot checkpoint sequence |
| 24 | 32 | Checkpoint commit digest, or zero for sequence zero |
| 56 | 32 | Canonical snapshot BLAKE3 digest |
| 88 | 8 | Payload length |
| 96 | 4 | CRC32C of header bytes `0..96` plus payload |
| 100 | 32 | Domain-separated BLAKE3 of header bytes `0..100` plus payload |

The proof digest domain is UTF-8 `hyphae-retrieval-proof-v1`. Complete file
length is exactly `132 + payload length`.

## Trusted anchor

The caller-pinned anchor is:

```text
BLAKE3(
  "hyphae-retrieval-anchor-v1" ||
  checkpoint_sequence_le ||
  checkpoint_digest_or_zero ||
  snapshot_digest
)
```

The proof carries these fields but not trust. Verification without an external
expected anchor proves self-consistency but not rollback resistance.

## Payload

| Size | Field |
|---:|---|
| 8 | Canonical request length |
| N | Canonical request bytes fixed by operation/semantics |
| 8 | Canonical outcome length |
| M | Canonical outcome bytes fixed by operation/semantics |

Exact payloads follow exact semantics v2. Lexical payloads follow lexical
semantics v1. Hybrid payloads embed both branch requests/outcomes, fusion
parameters, explanations, and final outcome under hybrid semantics v1.
Lengths consume the payload exactly.

## Verification

The offline verifier:

1. validates framing, versions, lengths, CRC32C, and proof digest;
2. compares the computed anchor with the caller-pinned anchor;
3. verifies the complete logical snapshot and matching checkpoint fields;
4. decodes logical records under explicit limits;
5. rebuilds the required vector spaces and lexical definitions;
6. reexecutes the operation's reference semantics; and
7. requires exact canonical outcome equality.

The end-to-end cooperative deadline is reduced by proof parsing and then
propagated through snapshot verification/loading and replay loops. File
length, logical-count, decoded-byte, and candidate-byte policies are checked
before or during the work they bound.

The standalone exact, lexical, and hybrid proof readers accept only regular
files. They preflight the opened-file length, read no more than the smaller
caller/canonical proof limit plus one detection byte in bounded chunks, and
reject a file whose observed length changes during the read. These decoders are
byte bounded but have no timeout parameter; the complete verification methods
run the same reads under their end-to-end deadline.

Exact replay, including the exact branch of hybrid replay, first scans matching
vectors to enforce candidate count, aggregate key/vector bytes, and deadline.
Only after that preflight succeeds does it allocate and clone the candidate
collection used by deterministic scoring.

Any mismatch, unknown version, exhausted limit, timeout, truncation, or
trailing byte is failure. No partially verified result is success.

## Security properties and limits

The proof demonstrates that the encoded outcome is the deterministic result of
the encoded request over the complete encoded snapshot under the declared
semantics. With a trusted external anchor it also detects rollback or wrong
witness selection.

It does not authenticate an anchor controlled by the attacker, prove semantic
quality, prove ANN recall, prove an embedding model's behavior, or sign an
identity.
