# Public contracts

`openapi/hyphae-v1.yaml` is the canonical HTTP surface and the JSON Schema
2020-12 documents are the canonical payload definitions for `/v1`. The data
operations cover KV put/get/delete, deterministic structured query, durable
vector and lexical definitions/mutations, exact/lexical/hybrid retrieval, and
result/retrieval proof witness download. Health and capabilities disclose no
data.

The Rust wire models in `hyphae-contracts` generate every checked-in schema.
`cargo run -p hyphae-contracts --example generate_schemas` refreshes them and
tests fail when generated and checked-in models differ. TypeScript and Python
SDK generation consumes only these versioned public documents.

## Structured values

The natural JSON surface accepts null, booleans, signed 64-bit integers,
strings, arrays, and objects. Floating-point and out-of-range numbers are
rejected. Opaque bytes use the exact reserved object form
`{"$hyphae_bytes_hex":"00ff"}`; an object containing exactly that one key is
therefore reserved and cannot represent an ordinary user object.

Binary record keys are nonempty, even-length hexadecimal strings. Object-path
segments cannot be empty. Runtime conversion enforces these semantic rules in
addition to JSON Schema shape validation.

Aggregation group keys use an explicit tagged form so a missing path remains
distinct from a path whose value is JSON null. Sort cursors intentionally
normalize both to null because their ordering semantics are identical.

## Compatibility

Unknown fields are rejected. A compatible `/v1` change may add optional
fields or new endpoints without changing existing semantics. Removing a
field, changing its meaning, or widening accepted data in a way that changes
deterministic query/proof behavior requires a new versioned contract.
