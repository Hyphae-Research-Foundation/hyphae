<!-- SPDX-License-Identifier: Apache-2.0 -->
# Structured document format v1

Status: normative for the Hyphae `0.1.0` engine facade.

`hyphae-engine` stores each structured query value as one self-verifying
binary KV value. All integers are little-endian. The 56-byte envelope is:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYDOC001` |
| 8 | 2 | Document format version (`1`) |
| 10 | 2 | Reserved flags; must be zero |
| 12 | 8 | Payload length |
| 20 | 4 | CRC32C of bytes `0..20` followed by payload |
| 24 | 32 | BLAKE3 of bytes `0..24` followed by payload |
| 56 | N | Canonical typed payload |

## Typed payload

Each value starts with a one-byte tag:

| Tag | Value | Following bytes |
|---:|---|---|
| 0 | null | none |
| 1 | false | none |
| 2 | true | none |
| 3 | signed integer | 8-byte two's-complement `i64` |
| 4 | UTF-8 string | `u64` byte length, then bytes |
| 5 | binary bytes | `u64` length, then bytes |
| 6 | array | `u64` element count, then values in order |
| 7 | object | `u64` field count, then repeated UTF-8 key length/key/value |

Object keys must be strictly increasing by UTF-8 bytes. The encoder obtains
this order from `BTreeMap`; the decoder rejects duplicates or alternate order,
unknown tags, invalid UTF-8, trailing bytes, and noncanonical lengths.

Payload is limited to 16 MiB, nesting to 64 levels, and decoded values to one
million nodes. Bounds are checked before proportional allocation. CRC32C and
BLAKE3 detect accidental redb or transfer corruption but are not authenticity
signatures.
