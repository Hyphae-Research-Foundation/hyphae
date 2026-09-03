# Packaging

`package.py` produces a deterministic archive containing one native `hyphae`
binary plus the Apache-2.0 software license, CC BY-SA documentation license, licensing
scope policy, readme, and third-party notices. It never bundles a database,
cache, model, provider credential, or runtime installer.

```bash
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)" \
python packaging/package.py \
  --binary target/dist/hyphae \
  --target x86_64-unknown-linux-gnu \
  --output-dir artifacts
```

The release workflow builds native archives for Linux x64, macOS x64/arm64,
and Windows x64. It emits a SHA-256 checksum file, SPDX JSON and
CycloneDX JSON SBOMs, Sigstore bundles for every release asset, and GitHub
Actions SLSA v1 provenance plus SBOM attestations for every native archive
before creating a release.

The SBOM pipeline scans the exact checkout once with the pinned Syft version
and retains Syft JSON as the pre-conversion inventory. Before producing either
published format, [`conclude_release_sbom_licenses.py`](conclude_release_sbom_licenses.py)
resolves every supported discovered first-party Hyphae artifact against its
exact tracked package-manifest authority and the applicable local lock/source
evidence. It then records `Apache-2.0` as both the declared and concluded
first-party license. Because pinned Syft does not catalog the source
`pyproject.toml`, the same step adds the `hyphae-sdk` component explicitly with
`foundBy = hyphae-manifest-cataloger` and that manifest as its primary evidence;
it does not represent this component as a Syft discovery. An unsupported
first-party package type, conflicting
license, unknown identity, non-local first-party source, or unresolved linked
npm package fails closed. Third-party artifacts and their observed licenses
are never rewritten. Private development-only npm projects are omitted only
when their artifact identity matches an explicit manifest path, package name,
`"private": true`, and local lock inventory; a matching name at another path or
a non-private manifest fails closed.

SPDX and CycloneDX are converted from that same normalized Syft document. The
G8 release verifier requires `Apache-2.0` in every emitted first-party
license field and exact multiset equality with the lock-derived Rust/npm
inventory plus the explicit Python manifest component. It independently
requires the same first-party `(name, version, purl)` multiset in both output
formats, including duplicate observations. Thus neither published format may
silently add, omit, or substitute a first-party identity.

Every package job also extracts its own archive and
uses only its installed binary to execute native initialization, structures,
SQL, checkpoint, compaction, backup/restore, and doctor. The same installed
binary exercises the retained format-2 compatibility proof path: it generates
a result proof plus exact, lexical, and hybrid retrieval proofs, downloads the
complete witnesses, stops the server, deletes the originating data directory,
and verifies all four proofs offline. A tampered result-proof negative control
must fail; the smoke cannot report success unless every positive verification
succeeds and the negative control is rejected.

A manual workflow run without recovery inputs executes native build,
provenance, SBOM, signing, and verification, then uploads a candidate artifact
without publishing a release. If an immutable tag run fails after the tag is
created, one manual recovery may name both the existing tag and its exact
peeled commit. That path re-fetches and verifies the tag, rebuilds every native
archive from the tagged commit, binds the hosted checks, and may publish without
moving or recreating the tag. Publication is reachable only from a `v*` tag
push or that exact-tag recovery path, and
`finalize_release.py` rejects a tag that does not equal `v` plus the workspace
version. The workflow binds the fetched tag object and peeled commit, requires
that commit to remain reachable from `main`, and re-fetches both immediately
before publication. A tag may be pushed only after the complete gate is green
and publication is explicitly authorized.

A publication also records exactly 20 required checks. Nineteen checks bind
the reviewed PR-head commit and its head branch; the exact-SHA G8 closure binds
the tagged merge commit on `main`. Each check is bound to its expected
canonical workflow path and successful workflow-run metadata; a same-named job
from another workflow is rejected. This set explicitly includes the
`Security hard-kill aggregate` and `MCP real hosts` jobs. Registry publication
also downloads and independently validates both exact-SHA artifact sets before
each package boundary. The 19 PR checks must be `pull_request`
runs, while G8 closure is the sole `workflow_dispatch` run. Every workflow path
must resolve to one run, and the tagged merge commit must belong to exactly one
merged in-repository PR targeting `main`; the complete PR
history for that head branch must contain no second PR, and the PR's complete
issue-event history must contain no base-ref change or successful automatic
base change. GitHub's Jobs API must provide the successful job record for every
selected check, and every job's `run_attempt` must equal the current attempt of
its workflow run. A partial rerun that mixes jobs from different attempts fails
closed; a complete rerun of all jobs can restore one coherent attempt. For one
canonical job in each of the six workflow runs, the Jobs API must also show a
successful integration-tree guard. The workflow verifies that PR's merge
parents, tree, and reachability before publication.

Run the deterministic unit checks with:

```bash
python packaging/test_package.py
```

Consumer verification is documented in
[`../docs/release/verification.md`](../docs/release/verification.md).

crates.io and npm use the separate `Registry publish` workflow. Dry-runs are
allowed against the current manifests, but live publication is fail-closed
unless the source is a clean checkout of the annotated
`release-v3.0.0-crates` tag and every package manifest and registry policy
declares exact version `3.0.0` with
Apache-2.0 authority.
