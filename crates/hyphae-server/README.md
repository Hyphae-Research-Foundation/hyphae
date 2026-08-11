<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-server

[![crates.io](https://img.shields.io/crates/v/hyphae-server?logo=rust)](https://crates.io/crates/hyphae-server)
[![docs.rs](https://img.shields.io/docsrs/hyphae-server)](https://docs.rs/hyphae-server)

Secure, loopback-first HTTP server for the [Hyphae](https://hyphae.dev) data
engine and its proof-bearing `/v1` API.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-server = "0.2.1"
```

Remote bind requires explicit authentication. Request, result, concurrency,
memory, witness, and timeout limits are part of the public behavior. HTTP
proof routes apply the witness limit before snapshot creation; download
verifies and streams one file handle while holding admission. Embedded hosts
can call `HyphaeServer::open_with_storage_limits` to select finite startup and
maintenance policy without changing `/v1`; ordinary `HyphaeServer::open`
selects `StorageLimits::default()`. The query route uses the additive bounded
engine API with a fixed 256 MiB aggregate scanned-input ceiling. Direct witness
admission returns HTTP `413` `result_too_large` when the witness policy is
exhausted.

Code is AGPL-3.0-only; documentation is CC-BY-SA-4.0. Source, threat model, and
security policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
