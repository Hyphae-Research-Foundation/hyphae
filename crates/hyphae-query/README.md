<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-query

[![crates.io](https://img.shields.io/crates/v/hyphae-query?logo=rust)](https://crates.io/crates/hyphae-query)
[![docs.rs](https://img.shields.io/docsrs/hyphae-query)](https://docs.rs/hyphae-query)

Pure deterministic query types and reference execution for
[Hyphae](https://hyphae.dev). It provides structured values, filters, global
sorting, logical cursors, aggregations, and explicit execution budgets.

The registry coordinate below is valid only after crates.io lists version
`0.2.1`:

```toml
[dependencies]
hyphae-query = "0.2.1"
```

The executor has no database, network, embedding, or LLM dependency. The
published `execute` and `execute_with_clock` entry points retain their `0.2.0`
count-bounded behavior. Additive `execute_with_byte_limit` and
`execute_with_clock_and_byte_limit` apply an explicit aggregate scanned-input
budget and return `BoundedQueryError`; the standalone server passes the
256 MiB `DEFAULT_QUERY_SCAN_BYTES` policy. Accounting includes every inspected
binary key and its exact canonical document bytes, including nonmatches and
records from every shard. Budget or timeout exhaustion returns an error rather
than partial success.

Code is GPL-3.0-only; documentation is CC-BY-SA-4.0. Source and security
policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
