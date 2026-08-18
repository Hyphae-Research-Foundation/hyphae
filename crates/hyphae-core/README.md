<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-core

[![crates.io](https://img.shields.io/crates/v/hyphae-core?logo=rust)](https://crates.io/crates/hyphae-core)
[![docs.rs](https://img.shields.io/docsrs/hyphae-core)](https://docs.rs/hyphae-core)

Shared product, API, disk-format, and proof-format version constants for
[Hyphae](https://hyphae.dev), the autonomous, embeddable, verifiable Rust data
engine.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-core = "0.2.1"
```

Most applications should depend on `hyphae-engine` instead. This crate exists
for consumers that must inspect compatibility without importing storage or
server behavior.

Code is Apache-2.0; documentation is CC-BY-SA-4.0. Source and security
policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
