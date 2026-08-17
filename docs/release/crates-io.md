# Publish the Rust crates

The current `1.1.0` manifests remain unchanged while the Apache release is
prepared. The first Apache-2.0 registry publication is authorized only for
exact version `1.2.0` from annotated tag `v1.2.0`. The version, immutable
dependency layers, and exact source authority are defined in
[`config/crates-io-release.json`](../../config/crates-io-release.json).
Conformance runners and independent verifiers remain private workspace tools
and are not registry packages.

crates.io publication is permanent: an uploaded version cannot be overwritten
or deleted. Live crates.io and npm publication is therefore a GitHub promotion
protocol, never a maintainer-workstation command and never authority supplied by
the tag being published. The trusted checker and policy are loaded from the
`Registry publish` workflow definition on protected `main`.

## Preconditions

1. Land version `1.2.0`, then create existing annotated tag `v1.2.0`. The
   registry workflow fetches the exact remote tag object and requires its peeled
   target to equal the current `origin/main` commit, not merely an ancestor.
2. Complete the pinned exact-SHA GitHub check suite in
   [`config/registry-publish-authority.json`](../../config/registry-publish-authority.json).
   The gate queries the Checks, Actions run, Jobs, and artifact APIs and requires
   the exact workflow path, job name, event, branch, run attempt, successful
   conclusion, commit, and GitHub Actions app identity. A similarly named check
   or an operator-supplied run claim does not satisfy the gate.
3. Complete the signed GitHub Release and exact-SHA G8 closure. Their named,
   unexpired workflow artifacts must include the release candidate, signed
   release G8 receipt, and closed G8 aggregate for the same commit. The trusted
   checker verifies release evidence, required checks, checksums, SBOMs,
   Sigstore signatures/attestations, receipt semantics, and artifact digests.
4. Require the accepted `1.2.0` relicensing transition receipt to validate
   against the exact tagged tree, including its content digest and clean-tree
   binding. Release readiness invokes `tools/relicensing_checks.py --readonly`;
   it fails on stale evidence and cannot regenerate or alter the receipt.
   Maintainers refresh it separately and explicitly with `--refresh` before the
   candidate enters CI. Require the complete crates.io or npm package inventory
   and exact Apache-2.0 manifest version from that same source.
5. Protect the `registry-production` GitHub environment with required reviewers.
   Live publication is allowed only by `workflow_dispatch` from protected
   `main`; the job receives registry credentials/OIDC only after environment
   approval. The policy currently requires an annotated tag plus the
   Sigstore-signed Release. If repository policy changes to require a signed Git
   tag, set `tag_signature.required=true` and ensure the runner has the trusted
   verification keyring before publication.
6. Run the workspace validation and package-content audit:

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

7. Store a least-privilege crates.io token only in the protected environment.
   Never place the token in a command line, repository file, workflow log, or
   maintainer shell history. npm publication uses provenance and the registry's
   protected publisher identity.

The live runner image must already provide the Rust and Node versions pinned by
the trusted `main` workflow. The privileged job does not execute a third-party
toolchain setup action after checkout; this keeps tag-controlled action metadata
out of the live pre-publication path.

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

Any development dependency between crates in the same layer must be path-only,
without a version requirement. Cargo strips those dependencies from the
published manifest. A versioned development dependency remains part of the
publication graph and must point to an earlier layer; the package audit rejects
same-layer or forward edges before publication.
The dependency policy permits wildcard requirements only when Cargo metadata
also identifies the edge as a local path; registry wildcard requirements
remain denied.

Use the `Registry publish` workflow. Pull requests and manual dry runs remain
unprivileged and execute package audits plus `cargo publish --locked --dry-run`
or `npm publish --dry-run`. A live dispatch must select `dry_run=false` from
`main`. Before any publish command, the workflow checks out trusted main control
code separately from the source tag, compares every target-side policy/checker
file byte-for-byte with trusted main, resolves live GitHub authority, downloads
the exact evidence artifacts, and repeats all remote/tag/evidence checks. Tag
code is not executed as the publication checker before this validation.

Do not invoke an unguarded live publish. If publication returns an ambiguous
network result, query the registry for the exact package version before retrying;
never assume the upload failed. The live checker does this before every package:
an absent exact version is published, while a present version is downloaded and
compared cryptographically with the freshly packaged source, Cargo VCS metadata
or npm SLSA source commit, registry metadata, integrity checksum, and npm
signature/provenance verification. Exact matches are recorded as already
complete; any mismatch fails closed. Every upload, including a failed or timed
out command, is polled until the exact artifact is visible or the run records an
ambiguous/propagation timeout. The content-bound publication-state JSON is
retained as a workflow artifact, so a rerun can reconcile completed crates and
continue at a partial topological layer without republishing them. Every crates
layer must be completely visible before the next layer starts. The workflow is
also intentionally fail-closed if a required artifact expires or any check,
tag, main tip, policy file, or digest changes between gate and upload.

## Verify consumers

Use clean temporary projects, not workspace paths:

```bash
cargo install hyphae-cli --version 1.2.0 --locked
hyphae version --json
```

Also create a minimal Rust application with exact `=1.2.0` dependencies on
`hyphae-engine`, `hyphae-query`, and `hyphae-native-product`; build it with
`--locked`. Verify that docs.rs has accepted every library package, then record
all crates.io URLs, checksums, and the Git tag in the publication receipt.

After this first full-ecosystem publication, configure crates.io trusted
publishing for the release workflow so later releases use short-lived OIDC
credentials instead of a stored API token.
