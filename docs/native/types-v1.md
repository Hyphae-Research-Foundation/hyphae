# Native canonical types v1

Status: normative target contract; core identities, logical declarations, and
canonical floats have a partial implementation in `hyphae-native-types`

This specification defines the types and stable identities shared by the
native relational, structure, and search engines. Public SQL and local-wire
encodings may add syntax or framing, but they cannot change these semantics.

## Encoding rules

- Fixed-width integers use little-endian encoding.
- Lengths are unsigned and checked before allocation.
- Every encoded value has one canonical byte representation.
- Decoders reject trailing bytes, invalid discriminants, overlong lengths,
  invalid UTF-8, noncanonical floats, and values outside declared limits.
- A configurable resource policy may be stricter than the format maximum.

## Stable identities

| Identity | Width | Invalid value | Rule |
|---|---:|---:|---|
| `ObjectId` | 128 bits | all zero | Globally unique within one data directory and never reused |
| `RowId` | 128 bits | all zero | Stable identity for one relational row |
| `TransactionId` | 128 bits | all zero | Caller-visible idempotency and recovery identity |
| `ColumnId` | 32 bits | zero | Stable inside one relation and never renumbered |
| `PageId` | 64 bits | zero | Physical page slot identity |
| `CatalogVersion` | 64 bits | zero | Immutable catalog snapshot identity |
| `CSN` | 64 bits | zero | Commit sequence; the first committed transaction is one |
| `LSN` | 64 bits | zero | Byte position of a WAL record start |

Counters fail closed before overflow. Dropping and recreating a named object
allocates a new `ObjectId`.

## Logical types

| Family | Parameters and bounds |
|---|---|
| `BOOLEAN` | `false` or `true` |
| signed integer | 8, 16, 32 or 64 bits |
| unsigned integer | 8, 16, 32 or 64 bits |
| `DECIMAL` | precision 1 through 38; scale 0 through precision; signed i128 coefficient |
| floating point | IEEE-754 binary32 or binary64 |
| `TEXT` | valid UTF-8; v1 collation is binary UTF-8 byte order |
| `BINARY` | arbitrary bytes |
| `DATE` | signed days from 1970-01-01 |
| `TIME` | nanoseconds from midnight; leap seconds are rejected in v1 |
| `TIMESTAMP` | signed microseconds from Unix epoch; optional UTC offset is presentation only |
| `INTERVAL` | signed months, days and nanoseconds |
| `UUID` | 128 uninterpreted bits |
| `JSON` | canonical JSON object, array or scalar |
| `ARRAY<T>` | ordered homogeneous values |
| `MAP<K,V>` | unique canonical keys, stored in key order |
| `VECTOR<F32,N>` | dimension 1 through 65,535, finite or canonical-NaN elements |

`NULL` is a value state accepted only where the catalog declares nullability;
it is not a standalone storage type.

## Canonical floating point

- All NaN payloads normalize to one quiet NaN bit pattern per width.
- Negative zero normalizes to positive zero for equality, hashing and storage.
- Ordinary SQL comparisons involving NaN evaluate to `UNKNOWN`.
- `IS NAN` and `IS NOT NAN` test NaN explicitly.
- Index total order is finite values, positive infinity, canonical NaN.

Vector metrics reject NaN and infinity at index admission even though scalar
and array values may contain canonical NaN.

## Decimal

A decimal is `(coefficient, scale)`. Encoding removes no trailing zeroes
because the declared column type determines scale. Casts use checked
round-half-even unless the expression explicitly requests another rounding
mode. Overflow aborts the statement and transaction operation; it never
saturates.

## Text and collation

V1 provides only binary UTF-8 collation. SQL equality and ordering operate on
the original bytes. Search analyzers may normalize or case-fold text under a
separate versioned analyzer definition; that never mutates the source value.

Future locale collations require a new versioned collation identifier and
rebuild of dependent indexes.

## JSON

JSON objects are stored with keys ordered by UTF-8 bytes, no duplicate keys,
minimal escapes, and canonical numeric spelling. Decoders reject duplicate
keys and non-finite numbers. JSON null is distinct from SQL `NULL`.

## Type compatibility

- Implicit casts are allowed only when lossless for every source value.
- Narrowing, text parsing, float/decimal conversion, and signed/unsigned
  boundary changes require explicit casts.
- Comparison requires the same logical type after allowed promotion.
- Hash and ordered indexes use the same canonical equality.
- Schema changes cannot reinterpret existing bytes under a different type;
  they require validation and a new physical version.

## Bounds

The format maximum for one encoded scalar or nested value is 16 MiB. Nesting
depth is at most 64 and aggregate node count at most 100,000. Large binary or
text values may be represented by a blob reference in row and structure
layouts without changing their logical type.

## Verification

The implementation must provide:

- round-trip and canonical re-encoding tests for every type;
- property tests for ordering, equality and hashing agreement;
- negative fixtures for every invalid encoding and bound;
- decimal overflow and rounding fixtures;
- float NaN/zero canonicalization fixtures; and
- cross-crate golden bytes consumed by pages, WAL and the local protocol.
