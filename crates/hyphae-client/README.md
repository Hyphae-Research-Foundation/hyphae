<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-client

[![crates.io](https://img.shields.io/crates/v/hyphae-client?logo=rust)](https://crates.io/crates/hyphae-client)
[![docs.rs](https://img.shields.io/docsrs/hyphae-client)](https://docs.rs/hyphae-client)

Bounded asynchronous Rust client for the [Hyphae](https://hyphae.dev) `/v1`
HTTP API.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-client = "0.2.1"
```

The client consumes only public versioned contracts and never opens or owns a
local Hyphae data directory.

Apache-2.0. Source, examples, and security policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
