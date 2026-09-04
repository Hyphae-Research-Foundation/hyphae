<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/Hyphae-Research-Foundation/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-pliegors

[![crates.io](https://img.shields.io/crates/v/hyphae-pliegors?logo=rust)](https://crates.io/crates/hyphae-pliegors)
[![docs.rs](https://img.shields.io/docsrs/hyphae-pliegors)](https://docs.rs/hyphae-pliegors)

Optional public-contract adapter between PliegoRS applications and the
[Hyphae](https://hyphae.dev) `/v1` API.

The crate is published at `3.0.0`:

```toml
[dependencies]
hyphae-pliegors = "=3.0.0"
```

The crate depends on `hyphae-client`, not PliegoRS internals or Hyphae storage.
Omit it and the host application continues to build and run without Hyphae.

When neither `HYPHAE_BASE_URL` nor `HYPHAE_BEARER_TOKEN` exists,
`PliegoHyphaeConfig::from_env()` returns `Ok(None)`. A PliegoRS application can
therefore keep Hyphae completely absent. When enabled, the application owns
the decision to place the cloneable `PliegoHyphae` value into its public state
mechanism.

This crate intentionally does not prescribe or copy a PliegoRS internal state
API. It imports no private PliegoRS API and never opens a Hyphae data directory.

Code is Apache-2.0; documentation is CC-BY-SA-4.0. Source and integration
boundary:
[`Hyphae-Research-Foundation/hyphae`](https://github.com/Hyphae-Research-Foundation/hyphae).
