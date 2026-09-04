# Publish the Rust crates

The Apache registry publication is authorized only for exact version `3.0.0`
from annotated tag `release-v3.0.0-crates`. The version, immutable
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

1. Land version `3.0.0`, then create the annotated tag
   `release-v3.0.0-crates`. The registry workflow fetches the exact remote tag
   object, requires its target to be an ancestor of current `origin/main`, and
   separately requires the trusted workflow SHA to equal current `origin/main`.
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

   This protection must actually exist on the environment, not only in
   workflow YAML that names it: the GitHub environment must carry a
   `required_reviewers` protection rule, or the `registry-production` job
   dispatches without ever pausing for approval. Before the `3.0.0`
   publication the environment (created 2026-08-19) held the registry token
   but no protection rule at all, and the `2.2.0` live run published without
   any approval; the required-reviewer rule was added on 2026-09-04, and the
   `3.0.0` live run (`33859889168`) is the first one that paused for and
   recorded an approval. Confirm the rule before every publication with
   `gh api repos/OWNER/REPO/environments/registry-production` and read
   `protection_rules` back, rather than assuming the environment name in the
   workflow file is enough. The [`3.0.0` receipt](receipts/3.0.0.md) records
   the approval.
6. Run the workspace validation and package-content audit:

   ```bash
   cargo fmt --all --check
   cargo check --workspace --all-targets --all-features --locked
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
    python3 tools/check_crate_packages.py
    python3 tools/verify_crate_packages.py
   ```

   The package audit rejects an incomplete publication set, a mismatched
   version, a non-exact internal dependency, an invalid dependency layer, or a
   compile-time asset absent from the generated crate. Package verification
   extracts every exact `.crate`, patches all release dependencies to those
   extracts, proves Cargo resolved no older registry copy, and checks the full
   packaged workspace. Release readiness runs the historical SemVer comparison
   only for packages present in the v0.2.1 baseline; newly introduced Native
   packages have no fabricated baseline.

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
unprivileged and execute package audits plus exact crate tarball verification
with local packaged dependency patches or `npm pack ./path --dry-run`. A
live dispatch must select `dry_run=false` from
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

## Readiness, G8 closure, and release dispatch inputs

Three separate `workflow_dispatch` calls carry the source commit forward from
readiness through the signed GitHub Release. Each is a distinct workflow with
its own input names; do not assume one workflow's input name applies to
another. The worked example throughout is the `3.0.0` publication, recorded in
full in [the `3.0.0` receipt](receipts/3.0.0.md).

1. **Readiness matrix** — `.github/workflows/native-g7-g8-readiness.yml`,
   dispatched at the release readiness tag (`release-vX-registry`). Its
   inputs are `g7_mode` (`authority` or `benchmark`; `authority` runs no
   hardware benchmark) and `g8_mode` (`authority` or `matrix`). For `3.0.0`,
   this ran at tag `release-v3.0.0-registry` with `g7_mode=authority`,
   `g8_mode=matrix`, producing [run `33836088262`](https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/33836088262).

2. **G8 exact-SHA closure** — `.github/workflows/native-g8-closure.yml`,
   dispatched from a branch sitting at the source commit (for `3.0.0`, the
   merge-evidence branch `release/fix/release-readiness-semver-offline-merge-evidence`).
   Its inputs, verified against the workflow file, are:
   - `source_commit` — the exact 40-character merge commit G8 closes;
   - `release_source_commit` — the exact second-parent commit that produced
     the Release evidence;
   - `readiness_run_id` — the successful readiness run containing the G8
     matrix evidence;
   - `release_run_id` — a successful Release workflow run for that same
     commit.

   For `3.0.0`: `source_commit=24bce1accdff8d14127797afe6f237a57c1cd4f3`,
   `readiness_run_id=33836088262`, producing
   [run `33836655173`](https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/33836655173).
   `release_run_id` here does not name the later recovery dispatch that cut
   the tag (below); the `Release` workflow's own required checks bind to
   `pull_request` events on the reviewed PR head, so the qualifying run is
   the one triggered by PR `#262` itself. Querying the Checks API for that
   PR's head commit (`d27546fda8b65cb253b88213e085f88c4d8b026d`) shows its
   `Assemble and verify release candidate` job on
   [run `33832418699`](https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/33832418699) —
   independently confirmed here via `gh api`, since the `3.0.0` receipt does
   not itself record this particular run ID.

3. **Release** — `.github/workflows/release.yml`. Its normal trigger is a
   `v*` tag push; its `workflow_dispatch` inputs (`release_tag`,
   `release_commit`) exist only for the documented recovery path — an
   existing immutable tag whose initial tag-triggered run failed. For
   `3.0.0`, the Release workflow was dispatched from `main` this way:
   `release_tag=release-v3.0.0-crates`, `release_commit=24bce1accdff8d14127797afe6f237a57c1cd4f3`,
   producing [run `33838703304`](https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/33838703304)
   at main tip `8a58749d892a52e38c651669ade03df5a6ee54af`. That main-tip
   commit is what `config/registry-publish-authority.json` and
   `tools/check_registry_publish.py` record as `release_run_commit` — the
   commit `main` was at when the qualifying Release run executed, not a
   dispatch input of `release.yml` itself.

## Control-plane commits

Publication authority is promoted to a new version in two separate control
commits on `main`, mirroring the pattern used for `2.2.0`. Both are plain
config/workflow edits — neither touches source crates or the tagged tree.

- **Tag-pin control commit** — [`8a58749d…`](https://github.com/Hyphae-Research-Foundation/hyphae/commit/8a58749d892a52e38c651669ade03df5a6ee54af)
  ([PR `#263`](https://github.com/Hyphae-Research-Foundation/hyphae/pull/263)).
  Moves every registry authority, the live-publish guard, the `Release` tag
  guard, the trusted checker and its fixtures, and the crates.io runbook to
  name the new annotated tag (`release-v3.0.0-crates`) rather than an exact
  commit. Touches `.github/workflows/registry-publish.yml`,
  `.github/workflows/release.yml`, `config/crates-io-release.json`,
  `config/npm-release.json`, `config/registry-publish-authority.json`,
  `docs/gates/evidence/relicensing-1.2.0-transition.json`,
  `docs/release/crates-io.md`, `packaging/README.md`,
  `tools/check_registry_publish.py`, and
  `tools/test_check_registry_publish.py`. Source, tree, tag-object, and
  release-run pins are left at their prior release's values in this commit —
  they only move in the next one.

- **Exact-SHA pin control commit** — [`74ea4d8d…`](https://github.com/Hyphae-Research-Foundation/hyphae/commit/74ea4d8d3496014d5299205f307bc7df77758e89)
  ([PR `#264`](https://github.com/Hyphae-Research-Foundation/hyphae/pull/264)).
  Follows the `Release` dispatch above and promotes `SOURCE_COMMIT`,
  `SOURCE_TREE`, `TAG_OBJECT`, `RELEASE_RUN_COMMIT`, and every pinned
  `head_sha`/artifact literal in `config/registry-publish-authority.json` and
  `tools/check_registry_publish.py` from the prior release's exact SHAs to
  `3.0.0`'s. Touches `config/registry-publish-authority.json`,
  `docs/gates/evidence/relicensing-1.2.0-transition.json`, and
  `tools/check_registry_publish.py`. `Registry publish` is dispatched from
  `main` only after this commit merges — dry run first, then live with
  `registry-production` approval.

(File lists above come from `gh pr view --json files` against the live
repository, not from the receipt.)

## Verify consumers

Use clean temporary projects, not workspace paths:

```bash
cargo install hyphae-cli --version 3.0.0 --locked
hyphae version --json
```

Also create a minimal Rust application with exact `=3.0.0` dependencies on
`hyphae-engine`, `hyphae-query`, and `hyphae-native-product`; build it with
`--locked`. Verify that docs.rs has accepted every library package, then record
all crates.io URLs, checksums, and the Git tag in the publication receipt.

After this first full-ecosystem publication, configure crates.io trusted
publishing for the release workflow so later releases use short-lived OIDC
credentials instead of a stored API token.
