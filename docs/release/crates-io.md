# Publish the Rust crates

Hyphae `1.0.1` publishes the complete 24-crate Native Rust ecosystem. The
version and immutable dependency layers are defined in
[`config/crates-io-release.json`](../../config/crates-io-release.json).
Conformance runners and independent verifiers remain private workspace tools
and are not registry packages.

crates.io publication is permanent: an uploaded version cannot be overwritten
or deleted. Run this procedure only from an exact, newly versioned release
commit after its complete hosted gate is green.

## Preconditions

1. Confirm `git status --short` is empty and `git describe --exact-match`
   reports the intended `vVERSION` tag.
2. Confirm CI, Security, Dependency Review, Fuzz, Stress, and the Native Release
   matrix succeeded on that exact commit.
3. Run the workspace validation and package-content audit:

   ```bash
   cargo fmt --all --check
   cargo check --workspace --all-targets --all-features --locked
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --all-features --locked
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
   python3 tools/check_crate_packages.py
   ```

   The package audit rejects an incomplete publication set, a mismatched
   version, a non-exact internal dependency, an invalid dependency layer, or a
   compile-time asset absent from the generated crate. Release readiness runs
   the historical SemVer comparison only for packages present in the v0.2.1
   baseline; newly introduced Native packages have no fabricated baseline.

4. Authenticate with a least-privilege crates.io token using `cargo login`.
   Never place the token in a command line, repository file, workflow log, or
   shell history.

## Dependency order

Publish every crate in one layer and wait for crates.io plus the registry index
to expose that exact version before starting the next layer:

1. `hyphae-core`, `hyphae-native-types`
2. `hyphae-native-ann`, `hyphae-native-catalog`, `hyphae-native-mvcc`,
   `hyphae-native-pages`, `hyphae-native-records`, `hyphae-native-wal`,
   `hyphae-query`
3. `hyphae-contracts`, `hyphae-native-blobs`, `hyphae-native-btree`,
   `hyphae-native-manifest`, `hyphae-retrieval`
4. `hyphae-native-runtime`, `hyphae-storage`
5. `hyphae-engine`, `hyphae-native-product`
6. `hyphae-native-protocol`
7. `hyphae-client`, `hyphae-native-daemon`, `hyphae-server`
8. `hyphae-cli`, `hyphae-pliegors`

For each crate, run `cargo publish --locked -p CRATE`. Do not bypass package
verification. If a publish returns an ambiguous network result, query crates.io
for the exact crate version before retrying; never assume the upload failed.

## Verify consumers

Use clean temporary projects, not workspace paths:

```bash
cargo install hyphae-cli --version 1.0.1 --locked
hyphae version --json
```

Also create a minimal Rust application with exact `=1.0.1` dependencies on
`hyphae-engine`, `hyphae-query`, and `hyphae-native-product`; build it with
`--locked`. Verify that docs.rs has accepted every library package, then record
all crates.io URLs, checksums, and the Git tag in the publication receipt.

After this first full-ecosystem publication, configure crates.io trusted
publishing for the release workflow so later releases use short-lived OIDC
credentials instead of a stored API token.
