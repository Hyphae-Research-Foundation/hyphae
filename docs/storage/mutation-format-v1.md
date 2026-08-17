<!-- SPDX-License-Identifier: Apache-2.0 -->
# KV mutation format v1

Status: normative for Hyphae `0.1.0` KV operation payloads in disk format `1`.

Keys and values are arbitrary bytes. Keys must contain between 1 byte and
1 MiB. The complete encoded mutation must fit the log's 16 MiB frame limit.
Integers are unsigned little-endian.

## Put

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | Kind `1` |
| 1 | 4 | Key length (`u32`) |
| 5 | 8 | Value length (`u64`) |
| 13 | K | Key bytes |
| 13 + K | V | Value bytes |

A put replaces the complete value for its key.

## Delete

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | Kind `2` |
| 1 | 4 | Key length (`u32`) |
| 5 | K | Key bytes |

Deleting a missing key succeeds and leaves it missing.

Decoders require exact lengths and reject trailing bytes, unknown kinds,
empty keys, oversized keys, and oversized frames. The materialized index
applies every mutation in a committed transaction together with its commit
checkpoint in one immediate-durability redb transaction.
