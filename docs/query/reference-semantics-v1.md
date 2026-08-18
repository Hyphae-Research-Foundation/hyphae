<!-- SPDX-License-Identifier: Apache-2.0 -->
# Structured query reference semantics v1

Status: normative for the Hyphae `0.1.0` reference executor.

## Logical records and values

A record has one nonempty globally unique binary key and one typed value.
Object field paths are ordered lists of exact UTF-8 keys; an empty path selects
the root, while an empty segment is invalid. Traversal through a non-object or
absent key is missing.

The total value order is:

```text
null < boolean < integer < string < bytes < array < object
```

Values of one type use their natural lexicographic order. Objects use their
`BTreeMap` key/value order. Structured query deliberately excludes binary
floating point.

## Filters

- `exists` is true for an explicit null and false for a missing path.
- equality is exact by type and value; comparisons against a missing path are
  false;
- ordered comparison requires the same value variant;
- prefix supports string/string and bytes/bytes;
- contains supports array element equality, string substring, and byte
  subsequence;
- empty `all` is true and empty `any` is false;
- `not` applies ordinary two-valued negation, including to a failed comparison
  on a missing path.

## Sort and cursors

Each sort field specifies direction and explicit null placement. Missing and
explicit null are equivalent only for sort placement. After all requested
fields, binary key ascending is the mandatory final tie-breaker and is not
reversed. Duplicate keys across shards are errors.

A logical cursor stores the normalized sort value for every sort field plus
the final key. Cursor filtering occurs after global sort and retains positions
strictly after the cursor. Its versioned wire encoding belongs to the public
contract phase.

## Aggregation and global merge

All shards contribute to one filtered set. Global sort, cursor, and final row
limit are applied only after this merge. Aggregations are evaluated over the
complete filtered set before cursor pagination:

- `count` includes every matched record;
- `sum` accepts integers, ignores missing/null, accumulates checked `i128`, and
  errors on any other value type;
- `min` and `max` ignore missing/null and use the total value order;
- group keys preserve missing separately from explicit null and are emitted in
  deterministic order.

An ungrouped plan emits one empty-key group even for zero matches. A grouped
plan emits no groups for zero matches.

## Budgets and timeout

Before execution, shape limits validate filter nodes and recursion depth, sort
fields, grouping fields, metric count, and requested page size. During
execution, global scan, matched-record, and group budgets are checked before
the corresponding retained input grows beyond the limit. The additive bounded
entry points also enforce an aggregate scanned-byte budget.

Scanned bytes are counted exactly once per inspected record across all shards,
including records rejected by the filter:

```text
binary key length
+ 56-byte HYDOC001 envelope
+ canonical Value payload length
```

`execute_with_byte_limit` and the durable facade's `query_with_byte_limit`
accept the byte policy explicitly and return separate bounded error types. The
standalone server passes a fixed 256 MiB ceiling without changing the v1 wire
shape. Legacy `execute`, `execute_with_clock`, and durable `query` retain their
`0.2.0` count-bounded behavior and do not apply this aggregate byte ceiling.
This is an input-scan budget, not a claim that every result or aggregation
allocation has the same byte size; separate count/shape/result limits still
apply.

A caller-visible monotonic timeout is checked cooperatively during durable
scan, document decoding, reference execution, aggregation, and after sort.
Any exceeded budget returns an error and no partial result.
