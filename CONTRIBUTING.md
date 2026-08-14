# Contributing

Repository access does not imply permission to publish source, artifacts,
benchmarks, or design documents. A release may be tagged or published only
after its complete gate passes on the exact selected commit and publication is
explicitly authorized.

## Branch flow

`main` is the sole integration and release branch. Every pull request targets
`main` and must pass the complete hosted check suite before merge. Direct
pushes, force pushes, and branch deletion remain prohibited by repository
protection; releases are tags on an exact, verified `main` commit.

## Development rules

1. Keep the base path local: one binary, one data directory, no required
   network or external service.
2. Change public behavior contract-first under `contracts/`.
3. Add or update an ADR for durable format, compatibility, security boundary,
   dependency direction, or provider changes.
4. Add a source-ledger entry before porting any historical code or test.
5. Keep framework adapters and providers outside core crates.
6. Add tests that prove the invariant, including failure behavior.
7. Update the documentation index, capability matrix, relevant guide, and
   executable example whenever shipped behavior changes.

## Required checks

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --all-features --no-deps --locked
python tools/generate_sdk_models.py --check
python tools/check_documentation.py --binary target/debug/hyphae
python tools/run_documentation_examples.py --binary target/debug/hyphae
cargo deny check
cargo audit
```

Commits must be focused and must not include generated secrets, data
directories, benchmark corpora, or attribution trailers added by automation.
See the [development guide](docs/development.md) for contract, durable-format,
documentation, compatibility, and release procedures.

## Contribution licensing

Contributors must have authority to submit their work. Unless explicitly
accepted under different terms, submitted software, tests, machine-readable
contracts, and tooling are licensed under `AGPL-3.0-only`; submitted
documentation is licensed under `CC-BY-SA-4.0`. See
[LICENSE-POLICY.md](LICENSE-POLICY.md) for the complete scope.
