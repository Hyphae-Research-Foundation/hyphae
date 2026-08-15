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

The additive `hyphae_client::v2` module exposes equivalent high-level Native
operations over canonical HTTP `/v2/execute` product envelopes and exact
`HYPHLCL1` AF_UNIX/Windows named-pipe transport. It uses product-owned contract
types and preserves typed product errors, deadlines, cancellation, and
transaction outcome state.

Native v2 bearer credentials may use plaintext `http://` only with a canonical
loopback host (`127.0.0.0/8`, `[::1]`, or exact `localhost`).
`HttpTransport::bearer_token` rejects every other plaintext origin before a
credential can be sent; remote managed endpoints require `https://`. This
restriction is additive to `hyphae_client::v2` and does not change the
published `/v1` client.

Code is AGPL-3.0-only; documentation is CC-BY-SA-4.0. Source, examples, and
security policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
