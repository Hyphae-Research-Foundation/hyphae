<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-contracts

[![crates.io](https://img.shields.io/crates/v/hyphae-contracts?logo=rust)](https://crates.io/crates/hyphae-contracts)
[![docs.rs](https://img.shields.io/docsrs/hyphae-contracts)](https://docs.rs/hyphae-contracts)

Versioned Rust models and embedded OpenAPI 3.1 / JSON Schema 2020-12 contracts
for the [Hyphae](https://hyphae.dev) `/v1` API.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-contracts = "0.2.1"
```

Use this crate when implementing a client, server adapter, or conformance tool
against the public wire contract.

The crate ships a byte-identical package-local mirror of the canonical files
under the repository-level `contracts/` directory. Workspace tests reject
drift between the two copies.

Apache-2.0. Canonical contract files and security policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
