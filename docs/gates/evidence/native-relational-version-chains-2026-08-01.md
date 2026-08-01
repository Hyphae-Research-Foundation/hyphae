# Native relational version-chain evidence

Date: 2026-08-01

Status: local implementation and latency evidence; G1, G2, and G7 remain open

Source commit:
`c1c9874f39473b67c81461e63302bb424a622b90`

Source tree:
`2729d18c7074cc0800157ffc2f66523d603171f5`

Branch: `main`

## Change

New data directories use relational marker `HYRELBT2`. Each primary-key entry
stores the exact 16-byte `HYROWP01` pointer to an immutable native
`VersionChain` page instead of storing a row inline in the B+tree.

For a later write to the same key, the runtime:

1. reads and validates the prior open version;
2. appends a closed copy with `end_csn` equal to the new commit CSN;
3. appends the new open row or tombstone pointing to that closed copy; and
4. publishes the new pointer through a copy-on-write B+tree root.

The current root therefore retains explicit half-open history without changing
pages reachable from an older root. Rewrites of the same key inside one
transaction coalesce into one version. Existing `HYRELBT1` directories remain
readable and writable without an implicit format migration.

Recovery traverses every reachable V2 chain and validates page kind, page CSN,
row identity, interval continuity, cycles, canonical row bytes, and blob
content. Current-root point reads keep the allocation-free B+tree path and add
one pinned buffer-pool lookup for the latest version page.

## Correctness evidence

Before the source commit was created, the complete Debian 13/WSL2 workspace
passed:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The focused native set contains six row-codec tests and 22 runtime tests. New
coverage proves:

- exact pointer encoding and immutable row closing;
- insert/update/delete produces open tombstone, closed update, and closed
  insert intervals;
- repeated same-transaction writes create one physical version;
- V1 directory reopen, read, update, and second reopen compatibility; and
- a self-referential version page fails closed.

Strict Clippy passed on Windows. A Windows attempt to execute the freshly
rebuilt runtime test binary was blocked before execution by the machine's
Application Control policy with `os error 4551`; the policy was not weakened.
The same binary's tests executed successfully under WSL2. `cargo fmt --check`,
`git diff --check`, and the relative-link check across all 135 tracked Markdown
files passed.

## Matched multilevel observation

The exact
[receipt](native-microsecond-smoke-version-chains-wsl2.json) uses the same
schema, dataset digest, 2,049 relational rows, tree height two, one-million
observations, 32 operations per timer sample, warmup, buffer-pool
configuration, concurrency, and reported WSL2 environment as the preceding
borrowed-row receipt.

The V2 physical point path observed:

- p50 `0.841 us`;
- p95 `1.200 us`;
- p99 `2.422 us`;
- p99.9 `4.942 us`; and
- aggregate throughput `1,088,895 operations/s`.

Against commit `7b0053c`, p50 increased `79.70%`, p99 increased `113.20%`,
p99.9 increased `42.13%`, and throughput decreased `45.71%`. The added
version-page lookup is the principal intended path difference. This is a
matched diagnostic observation, not an affinity-controlled causal experiment.

The path still observes a sub-microsecond batch-average p50 and microsecond
tails on this machine. It does not establish individual-operation
sub-microsecond latency, scalable concurrency, cold behavior, transport
latency, saturation behavior, or interference resistance. G7 remains open.

## Next limits

- Decouple transaction preparation from the full-duration writer guard, then
  prove same-snapshot first-committer-wins and disjoint-key rebase behavior.
- Define snapshot retention horizons and a crash-safe chain/blob vacuum before
  reclaiming any historical page.
- Add randomized model equivalence, interval-corruption cases, fuzzing, and
  chains longer than the fixed 64-page hot-path detector.
- Measure allocations and hardware counters before changing the on-page
  pointer or version-page layout.
