# Packaging

`package.py` produces a deterministic archive containing one native `hyphae`
binary plus the license, readme, and third-party notices. It never bundles a
database, cache, model, provider credential, or runtime installer.

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
before creating a release. Every package job also extracts its own archive and
executes the documented offline version, KV, query, compaction, result proof,
durable vector/lexical/hybrid retrieval, retrieval-proof verification,
backup/restore, and doctor flow from the installed binary.

A manual workflow run, including one dispatched from a tag ref, executes native
build, provenance, SBOM, signing, and verification, then uploads a candidate
artifact without publishing a release. Publication is reachable only from a
`push` event for an explicit `v*` tag, and
`finalize_release.py` rejects a tag that does not equal `v` plus the workspace
version. The workflow binds the fetched tag object and peeled commit, requires
that commit to remain reachable from `main`, and re-fetches both immediately
before publication. A tag may be pushed only after the complete gate is green
and publication is explicitly authorized.

A tagged `push` publication also records exactly 17 required checks. Each check
is bound to the expected canonical workflow path and to successful workflow-run
metadata for the same commit; a same-named job from another workflow is
rejected. All selected runs must be `pull_request` runs for the same head
branch, every workflow path must resolve to one run, and the commit must belong
to exactly one merged in-repository PR targeting `main`; the complete PR
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
