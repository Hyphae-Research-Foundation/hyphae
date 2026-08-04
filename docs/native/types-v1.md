# Native canonical types v1

Status: normative target contract; identities, logical-type descriptors,
primitive scalar storage, and primitive ordered-index components are
implemented in `hyphae-native-types`. Canonical arrays, maps, and fixed-
dimension float32 vectors now have checked storage codecs; arrays/maps support
nested/null values and maps enforce strict canonical key order. Canonical JSON
remains a target-only value codec.

This specification defines the types and stable identities shared by the
native relational, structure, and search engines. Public SQL and local-wire
encodings may add syntax or framing, but they cannot change these semantics.

## Encoding rules

- Fixed-width integers use little-endian encoding in row/storage payloads.
- Ordered-index components use the distinct memcomparable encoding defined
  below; they are never interpreted as row/storage payloads.
- Lengths are unsigned and checked before allocation.
- Every encoded value has one canonical byte representation.
- Decoders reject trailing bytes, invalid discriminants, overlong lengths,
  invalid UTF-8, noncanonical floats, and values outside declared limits.
- A configurable resource policy may be stricter than the format maximum.

## Logical-type descriptor v1

A descriptor is self-delimiting. It contains exactly one type and no trailing
bytes.

| Tag | Type | Bytes after tag |
|---:|---|---|
| `0x01` | `BOOLEAN` | none |
| `0x02` | signed integer | width byte: `8`, `16`, `32`, or `64` |
| `0x03` | unsigned integer | width byte: `8`, `16`, `32`, or `64` |
| `0x04` | `DECIMAL` | precision byte, scale byte |
| `0x05` | `FLOAT32` | none |
| `0x06` | `FLOAT64` | none |
| `0x07` | `TEXT` | none |
| `0x08` | `BINARY` | none |
| `0x09` | `DATE` | none |
| `0x0a` | `TIME` | none |
| `0x0b` | `TIMESTAMP` | none |
| `0x0c` | `INTERVAL` | none |
| `0x0d` | `UUID` | none |
| `0x0e` | `JSON` | none |
| `0x0f` | `ARRAY<T>` | one recursive element descriptor |
| `0x10` | `MAP<K,V>` | key descriptor, then value descriptor |
| `0x11` | `VECTOR<F32,N>` | element byte `0x01`, then `N` as little-endian `u16` |

Recursive nesting is at most 64. Unknown tags, invalid parameters, truncation,
excessive nesting, and trailing bytes fail closed.

## Primitive row/storage payload v1

Row field boundaries are owned by the row directory, so variable-width scalar
payloads carry no internal length prefix.

| Type | Canonical payload |
|---|---|
| `BOOLEAN` | exactly `0x00` or `0x01` |
| signed/unsigned integer | declared width, little-endian |
| `DECIMAL` | checked signed `i128` coefficient, little-endian |
| `FLOAT32` / `FLOAT64` | canonical IEEE bits, little-endian |
| `TEXT` | raw valid UTF-8 bytes |
| `BINARY` | raw bytes |
| `DATE` | signed `i32` days, little-endian |
| `TIME` | `u64` nanoseconds, little-endian, less than `86,400,000,000,000` |
| `TIMESTAMP` | signed `i64` microseconds, little-endian |
| `INTERVAL` | `i32` months, `i32` days, `i64` nanoseconds; each little-endian |
| `UUID` | 16 bytes in network byte order |

SQL `NULL` is never a primitive scalar payload; it is represented in a row's
null bitmap. Inside canonical arrays, each element carries an explicit null or
value marker. Array storage is `u32 count` followed by each element as `0x00`
for null or `0x01 + u32 byte length + canonical element payload`. Counts above
100,000, truncation, trailing bytes, invalid markers, and aggregate values over
16 MiB fail closed. Map storage is `u32 count`; each entry carries a length-
prefixed non-null canonical key followed by `0x00` for a null value or `0x01 +
u32 byte length + canonical value payload`. Keys must be strictly increasing by
their declared ordered encoding, making duplicates and unsorted maps
noncanonical. Vector storage is exactly `N` canonical float32 values in
little-endian element order with no redundant dimension field; the declared
type fixes `N`, and noncanonical NaN/zero bits or wrong byte counts fail closed.
Canonical JSON is not defined in this slice and rejects use explicitly.

## Ordered-index component v1

Every component is self-delimiting and begins with:

- `0x00` alone for SQL `NULL`;
- `0x01` followed by one non-null payload.

Non-null payloads compare in logical total order under unsigned lexicographic
byte comparison:

| Type | Ordered payload |
|---|---|
| `BOOLEAN` | `0x00` or `0x01` |
| signed integer | declared-width big-endian bytes with the high sign bit toggled |
| unsigned integer | declared-width big-endian bytes |
| `DECIMAL` | big-endian `i128` coefficient with the high sign bit toggled |
| floating point | canonical bits; negative encodings bitwise-inverted, non-negative encodings have the high sign bit toggled |
| `TEXT` / `BINARY` | each zero byte becomes `00 ff`; final terminator is `00 00` |
| `DATE` / `TIMESTAMP` | big-endian signed value with the high sign bit toggled |
| `TIME` | big-endian `u64` |
| `INTERVAL` | ordered months, then days, then nanoseconds |
| `UUID` | 16 raw bytes |

The text/binary terminator makes both prefixes and embedded zero bytes
unambiguous. Decoders reject missing terminators, bytes after a terminator,
invalid escapes, noncanonical float bits, wrong fixed widths, and
out-of-domain values. Ordered arrays encode each complete element component
through the same zero-byte escaping used by text and binary, concatenate those
self-delimiting components, and end with `00 00`. This preserves lexicographic
array order, including SQL null elements and prefix arrays. Ordered maps use
the same framing over alternating key/value ordered components and require
strictly increasing non-null keys. This preserves lexicographic entry order and
nullable values. Ordered codecs for `JSON` and `VECTOR` remain undefined and
fail explicitly.

## Directory lineage identity

One native history is identified by exactly 24 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 16 | RFC 9562 UUIDv7 bytes in network byte order |
| 16 | 8 | nonzero history epoch, little-endian |

The UUID is generated once when the native directory is created. Its version
nibble is seven and its variant bits are `10`; all textual representations
use lowercase hyphenated hexadecimal. The history epoch starts at one,
increases only through a sanctioned history-divergent operation, never
decreases, and fails before overflow.

The pair is copied byte-for-byte into lineage-bearing manifests and retention
anchors. Equality is exact over all 24 bytes. A UUID with another version or
variant, a zero epoch, a noncanonical text form, or mixed lineage within one
digest chain fails closed.

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
| `DirectoryUuid` | 128 bits | non-v7 or non-RFC variant | Stable identity generated once per native directory |
| `HistoryEpoch` | 64 bits | zero | Monotonic identity for one nondivergent history |

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
- Index total order is negative infinity, finite values, positive infinity,
  canonical NaN.

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

- round-trip and canonical re-encoding tests for every implemented value type;
- property tests for ordering, equality and hashing agreement;
- negative fixtures for every invalid encoding and bound;
- decimal overflow and rounding fixtures;
- float NaN/zero canonicalization fixtures; and
- cross-crate golden bytes consumed by pages, WAL and the local protocol.

The current implementation provides deterministic unit fixtures for descriptor
round trips, all primitive storage codecs, byte-order agreement for each
primitive ordered codec, malformed inputs, limits, decimal domains, time
domains, float NaN/zero canonicalization, and descriptor golden bytes. Property
tests now cover every implemented primitive family: signed and unsigned
integers, decimal, float32/float64 canonicalization and total order, text,
binary, date, time, timestamp, interval, and UUID storage/ordered round trips
plus exact value-order agreement with memcomparable bytes. The records crate
consumes one frozen 13-family primitive corpus directly from
`hyphae-native-types`, proving cross-crate storage and ordered-byte identity.
Records, pages, WAL, and the native runtime/local-protocol test surface all
consume this exact corpus. Nested arrays, maps, and vectors have canonical
storage coverage, and arrays/maps have canonical ordered coverage. Canonical
JSON and the ordered vector codec remain required.
