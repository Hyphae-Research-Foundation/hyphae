<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/Hyphae-Research-Foundation/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-cli

[![crates.io](https://img.shields.io/crates/v/hyphae-cli?logo=rust)](https://crates.io/crates/hyphae-cli)
[![GitHub release](https://img.shields.io/github/v/release/Hyphae-Research-Foundation/hyphae?logo=github)](https://github.com/Hyphae-Research-Foundation/hyphae/releases/latest)

The single `hyphae` executable: local data engine, operations CLI, `/v1`
server, remote client, offline proof verifier, and MCP stdio adapter.

The crate is published at `3.0.0`:

```bash
cargo install hyphae-cli --version 3.0.0 --locked
hyphae version --json
```

The base deployment is one binary and one data directory. KV, structured
query, recovery, backup/restore, and verification work without an external
database, cache, cloud, embedding provider, or LLM.

Remote request/proof JSON and bearer-token inputs are byte bounded before
decoding. Named files are metadata-preflighted, read through one detection byte
past their limit, and rejected when a regular file's observed length changes.
The remote CLI's 4 MiB request and 32 MiB proof JSON ceilings are fixed; it
does not negotiate larger custom-server values from capabilities.

Code is Apache-2.0; documentation is CC-BY-SA-4.0. Quickstart, release
verification, and security policy:
[`Hyphae-Research-Foundation/hyphae`](https://github.com/Hyphae-Research-Foundation/hyphae).
