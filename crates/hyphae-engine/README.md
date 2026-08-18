<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-engine

[![crates.io](https://img.shields.io/crates/v/hyphae-engine?logo=rust)](https://crates.io/crates/hyphae-engine)
[![docs.rs](https://img.shields.io/docsrs/hyphae-engine)](https://docs.rs/hyphae-engine)

The recommended embeddable facade for [Hyphae](https://hyphae.dev), an
autonomous, durable, and verifiable Rust data engine.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-engine = "0.2.1"
```

Open one data directory, store structured records, run deterministic queries,
create snapshots and backups, and emit portable result proofs. The base path
works offline without an external database, cache, cloud, embedding provider,
or LLM.

The `0.2.0` `open` and `query` methods remain source- and
behavior-compatible. New embeddings should use
`open_with_limits(StorageLimits::default())` for finite recovery/maintenance
policy and `query_with_byte_limit` for aggregate scanned-byte accounting. The
`*_with_proof_with_limits` methods also carry an operation's remaining timeout
through snapshot creation; legacy proof methods snapshot afterward under the
stored maintenance policy. The standalone server selects the bounded paths.

Code is Apache-2.0; documentation is CC-BY-SA-4.0. Source, examples, and
security policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
