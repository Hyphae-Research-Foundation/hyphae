# Verify a Hyphae release

Do not install an archive until its digest, identity, and provenance all
verify. Replace `VERSION` and `TARGET` with the downloaded release values.

## Maintainer tag-target invariant

The version tag targets the canonical merge commit for the reviewed release
PR. The release report binds 17 required PR checks to that PR's exact head
commit and binds the exact-SHA G8 closure to the tagged merge commit on `main`.
Every required PR workflow checks out the head rather than GitHub's synthetic
merge commit, and the Release assemble job separately requires the merge
commit's second parent and tree to equal that reviewed head.
Merge a release PR with a merge commit, never squash or rebase it, and retain
the reviewed candidate as a commit reachable from protected `main`:

```bash
git merge-base --is-ancestor CANDIDATE origin/main
```

After hosted closure and explicit publication authorization, create the
version tag on `MERGE_COMMIT`. Verify the target before push:

```bash
test "$(git rev-parse vVERSION^{commit})" = "MERGE_COMMIT"
```

The publication workflow excludes every check from its own run and must find
all 18 completed successful prior checks on their two exact authority commits.
It fetches the remote tag, records both the ref object and peeled merge commit,
requires that commit to remain reachable from `main`, and repeats those checks
immediately before publication. Moving the tag, crossing PRs, changing the
reviewed tree, or losing merge ancestry therefore fails closed. If the initial
tag run fails, recovery must use the existing tag plus its exact peeled commit;
it never recreates or moves the tag.

## 1. Verify checksums

Download every file named in `SHA256SUMS`, including the archive, provenance
predicates, both SBOMs, and
`hyphae-vVERSION.release-evidence.json`. A tagged release must also list
`hyphae-vVERSION.required-checks.json`. Download `SHA256SUMS` and the
corresponding `.sigstore.json` bundles into the same directory:

```bash
sha256sum --check SHA256SUMS
```

Every listed archive and SBOM must report `OK`. On PowerShell, compare a file
with its `SHA256SUMS` entry:

```powershell
(Get-FileHash .\hyphae-VERSION-TARGET.zip -Algorithm SHA256).Hash.ToLowerInvariant()
```

## 2. Verify the keyless signature

Use Cosign 3.1.1 or a later compatible verifier. The certificate identity is
bound to the exact Release workflow ref recorded in the release evidence. It
is `refs/tags/vVERSION` for a tag push and `refs/heads/main` for an authorized
exact-tag recovery:

```bash
cosign verify-blob \
  --bundle hyphae-VERSION-TARGET.tar.gz.sigstore.json \
  --certificate-identity \
    'https://github.com/celiumsai/hyphae/.github/workflows/release.yml@RELEASE_WORKFLOW_REF' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  hyphae-VERSION-TARGET.tar.gz
```

Run the same verification for `SHA256SUMS`, both SBOM files, every provenance
predicate, `hyphae-vVERSION.required-checks.json`, and
`hyphae-vVERSION.release-evidence.json`. A bundle from another repository,
workflow, branch, or tag must fail the identity check.

## 3. Verify the release evidence binding

Inspect `hyphae-vVERSION.release-evidence.json`. Its
`release`, `source`, and `workflow` objects must identify the expected version,
tag, commit, tree, fetched tag object, peeled tag target, full tag ref, release
workflow, and GitHub Actions run. Its artifact inventory must contain every
archive, provenance predicate, SPDX SBOM, CycloneDX SBOM, and the
required-checks report exactly once. The checks
report must identify `celiumsai/hyphae`, the tagged merge commit, and exactly
the 20 canonical checks in fixed order. Every record must have a unique
check-run ID, the matching workflow-run ID, canonical workflow path and
GitHub job URL, GitHub Actions app ID/slug, the authoritative `head_sha` and head branch,
run attempt and
`status=completed`/`conclusion=success`. All jobs from one workflow path must
come from one workflow run. The report also identifies the unique merged,
in-repository PR to `main`, including its number, head/base commits, merge
commit, and merge time; querying all PRs for that in-repository head branch must
return that same PR and no other, and its complete issue-event history must
contain no base-ref change or successful automatic base change. The producer
verifies each referenced workflow run's exact ID, path, commit, branch, event,
repository, attempt, state, and conclusion. Nineteen checks require
`event=pull_request` on the reviewed PR head; the exact-SHA G8 closure requires
`event=workflow_dispatch` on the tagged merge commit and `main`. It fetches the Jobs API record for
every selected check and requires the job's exact ID, workflow-run ID, name,
commit, state, conclusion, and `run_attempt` to agree with the check and the
workflow run's current attempt. A partial rerun that mixes jobs from different
attempts must fail; a complete rerun of all jobs can restore one coherent
attempt. For one canonical job in each of the six workflow runs, the report
also records the successful Jobs API step named
`Verify the pull-request integration tree`; that step requires its event merge
SHA and tree to equal the tested head SHA and tree. Before publishing, the
release workflow verifies the recorded merge commit's two parents and tree and
requires it to remain reachable from `main`. The selected record carries its
start and completion timestamps; the producer rejects an ambiguous latest
completion or a relevant run that remains incomplete.

The canonical set includes `Security hard-kill aggregate` and `MCP real hosts`.
The separate registry publication authority re-resolves those successful jobs
on the exact source SHA, downloads their named artifacts from that same CI run,
and runs the security crash and real-host receipt validators over the downloaded
bytes. Omission, expiry, digest drift, or validation failure blocks publication.

From a checkout of the exact source commit, fetch the tag and validate its live
object/target binding together with all inventoried payload hashes:

```bash
git checkout --detach COMMIT
git fetch --force --no-tags origin \
  '+refs/tags/vVERSION:refs/hyphae/verify-tag'
TAG_OBJECT="$(git rev-parse refs/hyphae/verify-tag)"
TAG_TARGET="$(git rev-parse 'refs/hyphae/verify-tag^{commit}')"
python packaging/release_evidence.py verify \
  --directory /path/to/downloaded-release \
  --manifest /path/to/downloaded-release/hyphae-vVERSION.release-evidence.json \
  --commit COMMIT \
  --tag-object "$TAG_OBJECT" \
  --tag-target "$TAG_TARGET"
```

The verifier requires both live tag values together and rejects a moved object
or peeled target. The evidence manifest intentionally does not inventory itself,
`SHA256SUMS`, or signature and attestation bundles. This avoids an impossible
self-reference: `SHA256SUMS` hashes the completed evidence manifest, and
Cosign signs both files.

The two JSON schemas validate structure. The command above also runs the
authoritative semantic verifiers, which reject crossed commits, versions,
workflow/check IDs, URLs, provenance fields, or artifact digests even when
each individual value is syntactically valid. The report is hosted evidence,
not publication authorization or proof that a successful check never passed
only after a flaky rerun. The release workflow re-fetches and byte-compares
the report and rechecks the remote tag and `main` ancestry immediately before
publication. A later external check rerun, tag move, or asset replacement
remains detectable through the signed evidence and checksums but requires
immutable-release and protected-tag repository governance for preventive
enforcement.

The signed report is not an independent trust root for repository writers. If
branch policy permits a writer to change workflows without required independent
review, that writer can weaken a guard before producing new successful checks.
Require protected workflow ownership, independent review, last-push approval,
protected release tags, and immutable releases when prevention against that
authority is part of the threat model.

## Registry publication authority

Live crates.io and npm publication has a separate final authority boundary. It
cannot be initiated by a tag event and cannot trust a checker first loaded from
the tag. `Registry publish` must be manually dispatched from protected `main`
with `dry_run=false`; its `registry-production` environment supplies the
external immutable approval boundary. Pull requests may run only the dry-run
path.

The workflow checks out `github.workflow_sha` as the trusted control plane and
`v1.2.2` as source in separate directories. Before executing source package
tools, the trusted checker requires the tag to be annotated, its peeled commit
to equal the exact fetched `origin/main` tip, and all pinned control files to be
byte-identical between trusted main and the tag tree. It then uses the GitHub
Checks, Actions, Jobs, and Artifacts APIs to bind each expected workflow/job
name to one successful current exact-SHA authority, rejects other apps or paths,
and downloads only the named unexpired Release and G8 artifacts from those run
IDs.

The final gate semantically revalidates the accepted relicensing transition for
the exact Git tree, the complete registry package inventory, signed Release
checksums/SBOMs/provenance, required-check report, signed-release receipt, and
closed G8 aggregate. It records all file and service digests and re-fetches the
tag, `origin/main`, checks, jobs, workflow runs, artifacts, and evidence
immediately before each ecosystem's live upload. Any mutation fails closed.
The current policy requires an annotated tag and the Sigstore-signed Release;
`config/registry-publish-authority.json` also carries the explicit switch for a
cryptographically signed tag if repository policy adopts that requirement.

## 4. Verify build provenance and SBOM attestations

The native package job emits a SLSA provenance v1 attestation whose subject is
the exact archive digest:

```bash
cosign verify-blob-attestation \
  --bundle hyphae-VERSION-TARGET.tar.gz.intoto.sigstore.json \
  --type slsaprovenance1 \
  --certificate-identity \
    'https://github.com/celiumsai/hyphae/.github/workflows/release.yml@refs/tags/vVERSION' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  hyphae-VERSION-TARGET.tar.gz
```

`verify-blob-attestation` checks that the archive digest appears in the signed
in-toto subject. Inspect the bundle's predicate and require the expected target,
Git commit, full tag ref, release workflow digest, Cargo lockfile digest,
GitHub-hosted builder, and invocation URI. The invocation must use the same
workflow run ID as the release-evidence manifest. Its attempt may be earlier
than the assemble attempt when a successful package job was reused by a
failed-job rerun, but it may never name another run or a later attempt.
This provenance allowance is separate from the required-check report: all 18
required-check jobs must match their workflow runs' current attempts.
Require the native runner identity that matches the archive target:
Linux/X64, macOS/X64, macOS/ARM64, or Windows/X64.

The archive also has `.spdx.attestation.sigstore.json` and
`.cyclonedx.attestation.sigstore.json` bundles. Verify them with the same
identity and `--type spdxjson` or `--type cyclonedx`, respectively.

## 5. Inspect and smoke-test

The SPDX and CycloneDX JSON files contain the normalized package inventory,
including first-party Hyphae components and third-party dependencies. Retain
them with the installed binary. For every first-party Hyphae identity, require:

- SPDX `licenseDeclared` and `licenseConcluded` both equal
  `Apache-2.0`;
- the CycloneDX license expression or identifier equals `Apache-2.0`;
- the complete multiset of `(name, version, purl)` identities, including
  duplicate observations, is identical between SPDX and CycloneDX.

The release pipeline derives both formats from one normalized Syft inventory.
Each discovered first-party license conclusion is backed by an exact Cargo or
npm package manifest authority plus applicable local lock/source evidence. The
Python SDK, which the pinned scanner does not discover from `pyproject.toml`,
is added as an explicitly identified manifest-backed component rather than as
scanner evidence. An unsupported discovered first-party package type fails
closed.
Third-party artifacts retain the licenses observed by Syft; the first-party
conclusion step must not rewrite them. The signed G8 release verifier rejects
missing or mismatched first-party license fields, any difference from the
complete lock-derived plus Python-manifest inventory, and any cross-format
first-party identity drift.

Extract the archive into an empty directory and confirm that it contains one
executable plus `LICENSE`, `LICENSE-DOCUMENTATION`, `LICENSE-POLICY.md`,
`NOTICE`, `README.md`, and `THIRD_PARTY_NOTICES.md`:

```bash
tar -xzf hyphae-VERSION-TARGET.tar.gz
./hyphae-VERSION-TARGET/hyphae version --json
```

The reported product must be `hyphae` and `engine_version` must equal the tag
without the leading `v`. A release tag that differs from the workspace version
is rejected by the publication workflow.
