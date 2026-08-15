# Executable examples

The examples exercise public, supported surfaces and require no external
database, cache, cloud service, model, or LLM.

## Embedded Rust

[`crates/hyphae-engine/examples/embedded.rs`](../crates/hyphae-engine/examples/embedded.rs)
opens one data directory, commits a document, executes a deterministic query,
creates a proof-bearing result, and verifies it offline:

```bash
cargo run -p hyphae-engine --example embedded -- ./example-data
```

Use a disposable or intentionally retained new path. Rerunning against the
same path appends a new transaction for the same key.

## HTTP and remote CLI

[`http/`](http/README.md) contains valid put, get, query/aggregation, and
delete API v1 request files plus commands for `hyphae remote`.

## MCP host configuration

[`mcp/host-config.json`](mcp/host-config.json) is a minimal host-neutral stdio
configuration. The MCP process expects a separately running managed Native
HTTP v2 service and a restricted durable API-key file.

All JSON files are parsed by the documentation gate. The Rust example is
compiled by workspace all-target Clippy/tests and run explicitly by the
documentation validation workflow.
