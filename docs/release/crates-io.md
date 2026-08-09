# Publish the Rust crates

Hyphae published ten independently consumable Rust packages at version
`0.2.1`; the immutable list and checksums are in the
[publication receipt](receipts/0.2.1.md). crates.io publication is permanent:
an uploaded version cannot be overwritten or deleted. Run this procedure only
from an exact, newly versioned release commit after its complete hosted gate is
green.

The native crates remain unpublished on crates.io. The `1.0.0` release
`hyphae-client`, `hyphae-server`, and `hyphae-cli` source packages use those
native internals and are therefore also marked `publish = false`.
`hyphae-pliegors` is likewise unpublished because its normal dependency on
`hyphae-client` would not resolve from crates.io. These packages remain in the
workspace for local builds and packaging, but are not publication candidates.
is intentionally distributed by the signed multiplatform archive workflow;
the Rust registry publication boundary remains the six-crate compatibility
closure below. Expanding crates.io publication to the Native crates requires a
separate accepted dependency-closure and registry receipt.

## Preconditions

1. Confirm `git status --short` is empty and `git describe --exact-match`
   reports the intended `vVERSION` tag.
2. Confirm CI, Security, Dependency Review, Fuzz, Stress, and the native Release
   matrix succeeded on that exact commit.
3. Run the workspace tests and the package-content audit:

   ```bash
   cargo test --workspace --all-features --locked
   python tools/check_crate_packages.py
   ```

   The audit rejects compile-time assets that resolve outside a crate or are
   absent from its generated package file list.

4. Authenticate with a least-privilege crates.io token using `cargo login`.
   Never place the token in a command line, repository file, workflow log, or
   shell history.

## Dependency order

The current publishable package audit covers these packages in dependency
order. The commands show the current publishable closure but must not be run
at `0.2.1`, which already exists. A future release procedure must first update
the workspace release version:

```bash
cargo publish --locked -p hyphae-core
cargo publish --locked -p hyphae-query
cargo publish --locked -p hyphae-retrieval
cargo publish --locked -p hyphae-storage
cargo publish --locked -p hyphae-engine
cargo publish --locked -p hyphae-contracts
```

After each upload, wait until crates.io and the registry index expose that
exact version before publishing a dependent package. Do not bypass package
verification. If a publish returns an ambiguous network result, query
crates.io for the version before retrying; never assume the upload failed.

## Verify consumers

Use clean temporary projects, not workspace paths:

```bash
cargo install hyphae-cli --version VERSION --locked
hyphae version --json
```

Also create a minimal Rust application with exact `=VERSION` dependencies on
`hyphae-engine` and `hyphae-query`, build it with `--locked`, and verify that
docs.rs has accepted every library package. Record the crates.io URLs and the
Git tag in the GitHub release notes.

Once the initial packages exist, configure crates.io trusted publishing for
the release workflow so future releases use short-lived OIDC credentials
instead of a stored API token.
