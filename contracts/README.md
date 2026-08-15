# Public contracts

`openapi/hyphae-v1.yaml` remains the canonical published format-2 HTTP surface,
and its JSON Schema 2020-12 documents define `/v1`. Native `dev` uses
`openapi/hyphae-v2.yaml` plus `json-schema/native-v2.schema.json` as the
versioned `/v2` edge and canonical binary-envelope authority. The `/v1` data
operations cover KV put/get/delete, deterministic structured query, durable
vector and lexical definitions/mutations, exact/lexical/hybrid retrieval, and
result/retrieval proof witness download. Health and capabilities disclose no
data.

The Rust wire models in `hyphae-contracts` generate every checked-in schema.
`cargo run -p hyphae-contracts --example generate_schemas` refreshes them and
tests fail when generated and checked-in models differ. TypeScript and Python
SDK generation consumes only these versioned public documents.

`native-access-control-v1.json` is the contract-first Native identity,
permission, built-in-role, operation, scope, and API-key authority. It is not a
wire payload schema and remains honestly labelled implementation-pending until
the durable RBAC surfaces and cross-transport conformance close. The
fail-closed `tools/check_native_access_control.py` checker binds every current
`ProductOperation` variant to a nonempty permission rule.

`native-mcp-v2.json` is the exact bounded tool contract for the read-only
Native MCP adapter. Its bytes are embedded in the CLI and their BLAKE3 digest,
together with `tool_schema_version`, is returned by initialization, tool-list
pages, and tool results. Every output schema admits exactly one bounded success
DTO or the typed redacted MCP `ProductError` envelope. It intentionally
contains no mutation tool.

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
