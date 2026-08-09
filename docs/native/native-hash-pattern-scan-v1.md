# Native hash pattern scan v1

Status: accepted implementation target.

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md),
[Native whole-hash TTL v1](native-hash-ttl-v1.md), and
[Native reverse hash scan v1](native-hash-reverse-scan-v1.md). It adds a
bounded binary-glob scan whose continuation reports physical progress even
when a page contains no matches.

## Surface

The embedded native surface adds:

```text
HSCAN_MATCH(
  key,
  pattern,
  start_after?,
  output_limit,
  visit_limit,
  match_step_limit
) -> {
  entries: [(field, value)],
  continuation: field?,
  stop: exhausted | output_limit | visit_limit,
  visited: integer,
  match_steps: integer
}
```

The Rust request and response types are `HashPatternScanRequest` and
`HashPatternScanPage`. Methods are named `hscan_match` on private batches and
retained snapshots, and `hscan_match_latest_hash` /
`hscan_match_latest_hash_at` on the current-root physical surface.

The operation requires one existing visible native hash. Another live
structure family fails with `StructureKindMismatch`. A missing or logically
expired hash fails with `UnknownStructureHash`.

## Binary glob grammar

Patterns and fields are arbitrary bytes. Matching is case-sensitive and does
not decode UTF-8.

The grammar is:

- an ordinary byte matches itself;
- `?` matches exactly one byte;
- `*` matches zero or more bytes;
- `[abc]` matches one listed byte;
- `[a-z]` matches one byte in an inclusive ascending range;
- `[^abc]` negates one class;
- `\` quotes the following byte both inside and outside a class;
- `]` is literal only when it is the first class member; and
- `-` is literal only when first or last in a class, unless quoted.

An unclosed or empty class, descending range, dangling quote, or dangling
class negation is invalid. Adjacent `*` tokens compile as one token. The
matcher is Hyphae-owned and does not invoke regex, a third-party query engine,
or another serialized execution surface.

The pattern is bounded by `MAX_HASH_PATTERN_BYTES`. Compiled token and class
range counts are bounded before any hash lookup. Invalid patterns fail before
state inspection.

## Cursor and page progress

`start_after` is an exclusive exact-field cursor:

- `None` starts at the least field inside the pattern's derived physical
  range;
- `Some(field)` visits only physical field identities greater than `field`;
- a cursor need not identify a current live field; and
- `Some(empty)` resumes after a real empty field.

`output_limit` bounds returned live matches. `visit_limit` bounds physical
field identities visited, including canonical tombstones and nonmatches.
Both limits must be in `1..=MAX_HASH_FIELD_BATCH_SIZE`.

`visited` counts every field identity consumed from the selected physical
range. Private and materialized snapshot execution has no physical
tombstones, so every visit there is a live field. Page boundaries may differ
between materialized and physical routes when retained tombstones consume the
physical visit budget; concatenating pages against one unchanged state must
produce the same ordered live matches without duplicates.

`continuation` is the last visited field when `stop` is `output_limit` or
`visit_limit`. It may therefore name a nonmatching or tombstoned field. The
caller resumes by passing it as `start_after`. `continuation` is absent when
`stop` is `exhausted`.

After each physical visit, a live matching entry is appended before stop
conditions are evaluated. `output_limit` takes precedence when one visit
reaches both limits.

Reaching a limit on the final candidate may conservatively return that limit
as the stop reason. A subsequent call may return an empty exhausted page.
Every non-exhausted successful page consumes at least one physical field
identity, so repeated continuation cannot stall.

## Match work budget

`match_step_limit` is in `1..=MAX_HASH_PATTERN_MATCH_STEPS`. A step is one
compiled-token/input comparison, star transition or backtrack, or class-range
test. Pattern compilation is bounded separately and does not consume this
runtime budget.

The matcher checks the budget before every step. Exhaustion returns
`HashPatternMatchStepLimitExceeded` and no page. Because the operation is
read-only, no state or cursor is published on failure. The error reports the
configured maximum, not partial entries. A caller that needs the same
candidate cohort must retry with a larger permitted budget or a narrower
pattern.

## Physical execution

The compiler derives the longest leading literal byte prefix before the first
wildcard or class token.

- A pattern containing only literal tokens uses an exact field point lookup.
- A nonempty leading literal prefix maps to lower and upper physical B+tree
  bounds for only that field prefix.
- A leading wildcard scans the hash-field namespace from `start_after`.

The current-root route captures one root set, validates live hash metadata and
logical time once, intersects the cursor with the derived field bounds, and
uses the native cached forward range visitor. It decodes reached envelopes,
skips canonical tombstones after advancing physical progress, runs the
compiled matcher only for live fields, and stops at the first configured
limit.

The physical route must not:

- materialize the complete hash;
- run through another query or search engine;
- evaluate fields outside a derived nonempty literal-prefix range;
- count nonmatching or tombstoned fields toward `output_limit`; or
- continue into later leaves after reaching `output_limit` or `visit_limit`.

Reached malformed metadata, field envelope, expiry, blob, page order, or cycle
fails the complete call rather than returning a partial page. Inside the
canonical per-hash field prefix, every remaining byte suffix is a valid binary
field identity by construction; a cross-hash or truncated compound identity
cannot enter the selected range. Oversized caller cursor or derived-prefix
identities fail before traversal.
When a cursor-free leading-wildcard traversal reaches physical exhaustion
without a limit, the live count must equal hash metadata cardinality.

## Validation order

Every route validates in this order:

1. pattern byte length and grammar;
2. output, visit, and match-step limits;
3. cursor and derived-prefix compound identities;
4. structure kind, existence, and whole-hash visibility; and
5. physical state reached by execution.

Rejected request inputs cannot trigger buffer-pool traversal or private-state
mutation.

## Durability and concurrency

This is a read-only operation. It adds no mutation, WAL opcode, conflict key,
page format, catalog object, dependency, or durability mode. Retained
snapshots use their pinned state and current-root calls use one captured root,
so every page belongs to one committed CSN.

Current-root continuation across a later commit has ordinary cursor semantics:
new fields at or below the cursor are not returned, while later fields may be
observed. Stable pagination across mutations requires a retained snapshot.
Whole-hash expiry is evaluated before traversal.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before model and public methods exist;
- every grammar token over binary bytes plus malformed-pattern rejection;
- empty, all-match, no-match, exact-literal, literal-prefix, and
  leading-wildcard patterns;
- live, dead, empty, below-prefix, inside-prefix, and above-prefix cursors;
- output-limit, visit-limit, and match-step-limit termination;
- an empty non-exhausted page that still advances through nonmatches or
  tombstones;
- concatenated private, retained-snapshot, current-root, explicit-time, and
  reopened equivalence;
- whole-hash TTL visibility before, at, and after expiry;
- height-two literal-prefix pruning and early stop;
- fail-closed reached metadata, value, and blob corruption plus pre-traversal
  rejection of oversized cursor/prefix identities;
- a direct-Linux release comparison of a prunable prefix glob and a
  leading-wildcard glob against full `HSCAN` plus application filtering;
- a matched parent/current persistent `HGET` control; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

This contract does not add reverse-pattern scans, opaque cursors, field TTL,
relative or sliding expiry, regex, Unicode collation, protocol exposure,
randomized model equivalence, or a complete G3/G7 claim.
