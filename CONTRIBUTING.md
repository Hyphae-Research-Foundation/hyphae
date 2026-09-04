# Contributing

Repository access does not imply permission to publish source, artifacts,
benchmarks, or design documents. A release may be tagged or published only
after its complete gate passes on the exact selected commit and publication is
explicitly authorized.

## Branch flow

`main` is the sole integration and release branch. Every pull request targets
`main` and must pass the complete hosted check suite before merge. Direct
pushes, force pushes, and branch deletion remain prohibited by repository
protection; releases are tags on an exact, verified `main` commit. Release and
evidence pull requests merge with a merge commit, never squash or rebase:
their receipts cite commit SHAs, and squashing or rebasing would break that
citation. See [release verification](docs/release/verification.md).

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
accepted under different terms, submitted software, tests, tooling,
machine-readable contracts, and implementable normative specifications are
licensed under `Apache-2.0`. Narrative documentation is licensed under
`CC-BY-SA-4.0`. The inbound license for each contribution is therefore the
same as the outbound license assigned by [LICENSE-POLICY.md](LICENSE-POLICY.md).
No CLA or copyright assignment is required by this repository.

Every commit must certify the [Developer Certificate of Origin 1.1](DCO) by
including a real-name sign-off matching the contributor identity:

```text
Signed-off-by: Legal Name <email@example.com>
```

Use `git commit -s` to add the sign-off. The sign-off certifies contributor
authority and the DCO statements; it does not assign copyright or grant the
project an unstated relicensing right.

CI checks every commit introduced by a pull request after DCO-policy adoption;
earlier repository history is exempt. The only documented automated-author
exception is `dependabot[bot]`, and its commits must still carry the bot's DCO
sign-off. No other bot or automation attribution substitutes for a sign-off.
