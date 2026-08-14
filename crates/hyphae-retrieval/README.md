<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-retrieval

[![crates.io](https://img.shields.io/crates/v/hyphae-retrieval?logo=rust)](https://crates.io/crates/hyphae-retrieval)
[![docs.rs](https://img.shields.io/docsrs/hyphae-retrieval)](https://docs.rs/hyphae-retrieval)

Exact provider-neutral cosine retrieval and explicit abstention semantics for
[Hyphae](https://hyphae.dev).

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-retrieval = "0.2.1"
```

The caller owns vectors and any optional embedding provider. This crate does
not contact a model, persist embeddings, or introduce a cloud dependency.

Code is AGPL-3.0-only; documentation is CC-BY-SA-4.0. Source and security
policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
