<!-- SPDX-License-Identifier: Apache-2.0 -->
# Append-only log format v1

Status: normative for Hyphae `0.1.0` disk format `1`.

All integers are unsigned little-endian. A segment is a sequence of frames;
there is no mutable segment header. The initial segment starts with sequence
`1` and an all-zero previous digest. A later segment is anchored by its active
immutable manifest: its first frame uses anchor sequence plus one and the
anchor digest as its previous digest. Every later sequence increases by
exactly one and links to the prior frame digest.

## Frame layout

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYPHAE01` |
| 8 | 2 | Disk format version (`1`) |
| 10 | 1 | Kind: begin `1`, operation `2`, commit `3` |
| 11 | 1 | Reserved flags; must be zero |
| 12 | 8 | Sequence |
| 20 | 16 | UUID transaction identifier |
| 36 | 8 | Payload length |
| 44 | 32 | Previous-frame BLAKE3 digest |
| 76 | 4 | CRC32C of bytes `0..76` followed by payload |
| 80 | 32 | BLAKE3 of bytes `0..80` followed by payload |
| 112 | N | Payload, limited to 16 MiB per frame |

The CRC32C detects accidental corruption efficiently. The BLAKE3 digest is
the frame identity used by the chain and later provenance proofs. Neither is
an authenticity signature; the threat model does not claim resistance to an
attacker who can rewrite the entire directory and recompute every digest.

## Transaction grammar

```text
begin(tx, descriptor)
operation(tx, opaque bytes)+
commit(tx, descriptor)
```

The 36-byte descriptor is an operation count (`u32`) followed by a BLAKE3
transaction digest. The digest domain is `hyphae-transaction-v1` and covers
the operation count plus each operation length and bytes in order. Begin and
commit descriptors must match the observed operations exactly.

A transaction is visible only after its commit frame and after the segment is
synchronized. A later begin supersedes an earlier uncommitted attempt. This
makes retry after a crash deterministic even when the previous attempt left
complete begin or operation frames.

## Recovery

Opening streams and validates every complete frame before exposing replay:

- future versions, unknown kinds or flags, sequence gaps, broken chains,
  checksum failures, digest failures, and invalid transaction grammar fail;
- only an incomplete physical tail is truncated;
- complete uncommitted attempts remain in the history but are ignored;
- a repeated transaction UUID with identical content is replayed once;
- reuse of a committed UUID with different content fails as an idempotency
  conflict.

The materialized index is not part of this format and must be reconstructible
from verified committed transactions.

The finite operational policy used by the packaged CLI/server, and available
through bounded embedded open, preflights the complete segment length before
scan and applies aggregate ceilings to complete frames, unique committed
transactions, operation frames, and decoded operation payload bytes under one
60-second deadline. A writer opened under that policy retains the same
ceilings and rejects a new transaction before append if accepting it would
make the segment fail its next recovery under that policy. These are runtime
policies, not fields in log format v1. Published legacy open methods preserve
their `0.2.0` compatibility behavior; embedded callers select finite
`RecoveryLimits` through the additive bounded APIs.

Retiring a prefix follows
[`compaction-v1.md`](compaction-v1.md); the snapshot preserves logical state
and idempotency while the anchored segment preserves the global digest chain.
