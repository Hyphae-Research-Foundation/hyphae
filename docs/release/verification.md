# Verify a Hyphae release

Do not install an archive until its digest, identity, and provenance all
verify. Replace `VERSION` and `TARGET` with the downloaded release values.

## Maintainer tag-target invariant

The 17-check release report is bound to the exact pull-request head commit.
Every required workflow checks out that head rather than GitHub's synthetic
merge commit, and the Release assemble job separately requires the event's
synthetic merge tree to equal the checked head tree. `Review dependency
changes` is PR-only; the five Release jobs also run for manual candidates and
tag pushes, but the tag run is excluded from its own required-check report.
Merge a release PR with a merge commit, never squash or rebase it, and retain
the reviewed candidate as a commit reachable from protected `main`:

```bash
git merge-base --is-ancestor CANDIDATE origin/main
```

After hosted closure and explicit publication authorization, create the
version tag on `CANDIDATE`, not on the merge commit. Verify the target before
push:

```bash
test "$(git rev-parse vVERSION^{commit})" = "CANDIDATE"
```

The tag workflow excludes every check from its own run and must find all 17
completed successful prior checks on that candidate SHA. It also fetches the
remote tag, records both the ref object and its peeled commit, requires that
commit to equal the candidate and remain reachable from `main`, and repeats
those checks immediately before publication. Tagging the merge commit, moving
the tag, or losing reachability through a squash/rebase merge therefore fails
closed.

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
bound to the tagged Hyphae release workflow:

```bash
cosign verify-blob \
  --bundle hyphae-VERSION-TARGET.tar.gz.sigstore.json \
  --certificate-identity \
    'https://github.com/celiumsai/hyphae/.github/workflows/release.yml@refs/tags/vVERSION' \
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
report must identify `celiumsai/hyphae`, the same source commit, and exactly
the 17 canonical checks in fixed order. Every record must have a unique
check-run ID, the matching workflow-run ID, canonical workflow path and
GitHub job URL, GitHub Actions app ID/slug, the same `head_sha` and head branch,
`event=pull_request`, run attempt, and
`status=completed`/`conclusion=success`. All jobs from one workflow path must
come from one workflow run. The report also identifies the unique merged,
in-repository PR to `main`, including its number, head/base commits, merge
commit, and merge time; querying all PRs for that in-repository head branch must
return that same PR and no other, and its complete issue-event history must
contain no base-ref change or successful automatic base change. The producer
verifies each referenced workflow run's exact ID, path, commit, branch, event,
repository, attempt, state, and conclusion. It fetches the Jobs API record for
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
This provenance allowance is separate from the required-check report: all 17
required-check jobs must match their workflow runs' current attempts.
Require the native runner identity that matches the archive target:
Linux/X64, macOS/X64, macOS/ARM64, or Windows/X64.

The archive also has `.spdx.attestation.sigstore.json` and
`.cyclonedx.attestation.sigstore.json` bundles. Verify them with the same
identity and `--type spdxjson` or `--type cyclonedx`, respectively.

## 5. Inspect and smoke-test

The SPDX and CycloneDX JSON files are dependency inventories. Retain them with
the installed binary. Extract the archive into an empty directory and confirm
that it contains one executable plus `LICENSE`, `README.md`, and
`THIRD_PARTY_NOTICES.md`:

```bash
tar -xzf hyphae-VERSION-TARGET.tar.gz
./hyphae-VERSION-TARGET/hyphae version --json
```

The reported product must be `hyphae` and `engine_version` must equal the tag
without the leading `v`. A release tag that differs from the workspace version
is rejected by the publication workflow.
