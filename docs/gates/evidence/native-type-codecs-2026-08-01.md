# Native canonical type-codec evidence

Date: 2026-08-01

Status: primitive type and ordered-key prerequisite; G0, G1, and G2 remain open

Source commit:
`493391545f33d75217151f0a0dc9ae1e7a3addf9`

Source tree:
`cfe26b69d5be4f5565643a70bae3bb5e63d15164`

Branch: `main`

## Change

`hyphae-native-types` now owns the first canonical binary contract shared by
typed relational rows and future secondary indexes:

- recursive, self-delimiting `LogicalType` descriptors for every declared
  logical type;
- checked primitive `ScalarValue` storage payloads;
- self-delimiting memcomparable primitive index components;
- explicit SQL-null separation between row null bitmaps and index keys; and
- typed failures for malformed, noncanonical, unsupported, overlong, and
  out-of-domain values.

This implementation does not delegate encoding, comparison, validation, or
ordering to an external SQL engine or key-value store.

## Storage and ordering boundary

Row payloads keep fixed-width numbers little-endian. Field boundaries remain
owned by the row directory, so text and binary payloads are raw bytes.

Index components use a separate representation:

1. `0x00` alone represents SQL `NULL`;
2. `0x01` introduces a non-null value;
3. signed fixed-width values toggle the high sign bit after big-endian
   conversion;
4. unsigned values use big-endian bytes;
5. canonical float bits use the standard negative-invert/non-negative-sign
   transform;
6. text and binary bytes escape zero as `00 ff` and terminate with `00 00`;
7. temporal and interval values preserve their declared tuple order; and
8. UUID values retain their 16 network-order bytes.

The resulting bytes preserve primitive logical total order under unsigned
lexicographic comparison. Row/storage bytes and ordered-index bytes are not
interchangeable.

## Failure behavior

The codecs fail closed for:

- unknown, truncated, over-nested, parameter-invalid, or trailing type
  descriptors;
- integer and decimal overflow;
- time values at or beyond one day;
- noncanonical negative zero or NaN payloads;
- invalid UTF-8;
- wrong fixed widths and trailing bytes;
- missing terminators, bytes after terminators, and invalid binary escapes;
- values over the 16 MiB scalar format bound; and
- use of JSON, array, map, or vector value codecs that do not yet have their
  canonical validators.

The last case is deliberate. Declaring those logical types is supported;
pretending that arbitrary bytes are canonical values is not.

## Verification

The source commit contains 13 unit tests covering:

- every logical-type descriptor and three stable descriptor byte fixtures;
- the exact 64-level nesting boundary and malformed descriptors;
- storage round trips for every implemented primitive family and all integer
  widths;
- primitive ordered round trips and increasing byte order across null,
  negative, zero, positive, infinity, NaN, prefixes, embedded zero bytes,
  temporal tuples, and UUIDs;
- decimal, integer, time, size, UTF-8, float, terminator, escape, and fixed
  width failures; and
- explicit unsupported nested/JSON/vector value behavior.

Windows passed:

```text
cargo test -p hyphae-native-types
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Debian 13 under WSL2 passed:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The WSL run includes the 41 native-runtime recovery and all-engine transaction
tests, so introducing the codec API did not regress the existing vertical.

## Remaining boundary

This is not typed SQL yet. Still required:

- a versioned persistent catalog-definition codec;
- catalog roots that retain column IDs, types, nullability, keys, and indexes;
- row validation and decoding by catalog schema;
- typed DDL/DML expressions, casts, predicates, and result metadata;
- physical secondary-index namespaces and maintenance;
- property-generated ordering/equality suites and cross-crate golden
  consumers; and
- canonical JSON, array, map, and vector value codecs.

No G0, G1, or SQL-completeness gate closes from this evidence alone.
