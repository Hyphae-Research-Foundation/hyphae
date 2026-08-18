<!-- SPDX-License-Identifier: Apache-2.0 -->
# Durable vector record format v1

Status: normative for Hyphae `0.2` disk format `2`.

All integers are little-endian. Every length is validated before allocation.
The complete mutation remains bounded by the log frame limit.

## Canonical identifiers

An object key is `u32 length || bytes`, nonempty, and no longer than 1 MiB.

A vector-space identifier is `u8 length || ASCII bytes`, has 1 through 128
bytes, and matches `[A-Za-z][A-Za-z0-9._-]*`.

## Space definition

Mutation kind `3` defines a named space:

| Size | Field |
|---:|---|
| 1 | Kind `3` |
| 1 + N | Canonical vector-space identifier |
| 2 | Dimension in `1..=4096` |
| 1 | Metric tag; cosine is `1` |
| 1 | Element tag; Q15 signed i16 is `1` |

Repeating the identical definition is idempotent. A different definition for
an existing name is a conflict.

## Vector upsert

Mutation kind `4` upserts one vector:

| Size | Field |
|---:|---|
| 1 | Kind `4` |
| 1 + N | Canonical vector-space identifier |
| 4 + K | Canonical object key |
| 2 | Element count |
| 2 * D | Signed Q15 elements |

The element count must equal the space dimension. Values are
`[-32767, 32767]`; `-32768` is rejected. At least one element must be nonzero.
An upsert replaces the complete vector at the identity.

## Vector delete

Mutation kind `5` deletes one vector:

| Size | Field |
|---:|---|
| 1 | Kind `5` |
| 1 + N | Canonical vector-space identifier |
| 4 + K | Canonical object key |

Deleting a missing vector succeeds. It does not delete the space definition
or the corresponding KV document.

## Canonical ordering

Snapshots order space definitions by identifier bytes. Vector records order by
`(space identifier bytes, object key bytes)`. Duplicate definitions or vector
identities are invalid.

## Validation

Decoders reject unknown kinds/tags, invalid identifiers, empty/oversized keys,
invalid dimensions, dimension mismatch, `-32768`, all-zero vectors, trailing
bytes, truncated bytes, and arithmetic overflow.
