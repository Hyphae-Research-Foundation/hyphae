# Publish the Python SDK

Hyphae publishes the pure-Python client as the `hyphae-sdk` distribution. Its
import package is `hyphae_sdk`. The `hyphae` name on PyPI belongs to an
unrelated project and must never be used by this repository.

The checked-in `4.0.0` Python SDK is source-only and integrates Lane14
`f2336d8664b32d812cea3291d942e286a89eea9d`. No terminal Python
publication receipt records `4.0.0` on PyPI, and the live workflow remains pinned to its retained `3.0.0` authority.
This runbook is not evidence that `4.0.0` has been published.

Publication is a promotion protocol, not a maintainer workstation command.
The `Python package` workflow runs only from the `main` control plane. Two
independent GitHub-hosted jobs build an existing immutable annotated source tag
with exact Python and uv versions. Their receipts retain the runner image and
toolchain identities, and their wheel and sdist bytes must match before the
only job that can mint an OIDC token starts. Each builder uploads one flat
artifact containing only its wheel, sdist, and builder receipt, so every
consumer receives those files directly in its requested download directory.
The source commit/tree, workflow
commit/run, exact files, builder receipts, and target registry are bound in a
strict v2 receipt. That privileged job contains no checkout, shell, or
repository code: it downloads the preflighted artifact by immutable GitHub
artifact ID and invokes only the pinned publisher action. The final receipt
retains that artifact ID and digest.

The only live source-tag form is `release-vVERSION-crates`, shared with the
native Release and registry authorities. Do not create a second Python- or
SDK-specific tag. The workflow rejects `vVERSION`, `release-vVERSION`,
`vVERSION-crates`, prerelease aliases, and malformed tags before a job that can
reach the OIDC publisher. It strips the exact `release-v` prefix and `-crates`
suffix, then requires that version to equal both the Python project version and
`workspace.package.version`.

## One-time registry setup

Configure Trusted Publishers separately on TestPyPI and PyPI:

- owner/repository: `Hyphae-Research-Foundation/hyphae`;
- workflow: `python-publish.yml`;
- TestPyPI environment: `testpypi`;
- PyPI environment: `pypi`;
- project name: `hyphae-sdk`.

Protect both GitHub environments. Require review for `pypi`; TestPyPI may use a
separate reviewer policy. Neither environment contains a password or API
token. The publish job alone receives `id-token: write`.

Those environment protections and Trusted Publisher registrations are
external registry/GitHub state. The repository freezes the expected identity
and fails closed when the PyPI Integrity API reports a different publisher,
workflow, environment, filename, or SHA-256 subject.

## Release sequence

1. Land the exact SDK version and complete hosted conformance.
2. Create the immutable annotated `release-vVERSION-crates` tag. Its push is
   the normal Release authority. The Python version must equal
   `workspace.package.version` in the source tree.
3. Dispatch `Python package` from `main` with
   `source_tag=release-vVERSION-crates`,
   `repository=testpypi`, and both TestPyPI authority inputs empty.
4. Wait for the TestPyPI job to publish, install on Python 3.11 and 3.14, and
   produce `python-publish-receipt.json`. Record both its workflow run ID and
   the SHA-256 of those exact receipt bytes.
5. Run any release-specific public conformance against the TestPyPI package.
6. Complete the signed GitHub Release for the same immutable tag and the exact
   Native G8 closure for its source commit. Record the successful Release and
   G8 run IDs and attempts, the SHA-256 of the release-evidence manifest, both
   release SBOMs, and the G8 aggregate.
7. Dispatch the same source tag from `main` with `repository=pypi`, the exact
   `testpypi_run_id` and `testpypi_receipt_sha256`, plus two strict JSON
   inputs. `release_authority` contains only `run_id`, `run_attempt`,
   `release_evidence_sha256`, `spdx_sha256`, and `cyclonedx_sha256`.
   `g8_closure_authority` contains only `run_id`, `run_attempt`, and
   `aggregate_sha256`. IDs and attempts are positive decimal integers; every
   digest is 64-character lowercase hexadecimal.

For a normal release, the Python receipt requires the Release workflow run to
have `event=push`, `head_branch=release-vVERSION-crates`, and
`head_sha=SOURCE_COMMIT`. Its release evidence must use
`ref=refs/tags/release-vVERSION-crates`. Arbitrary manual Release runs are not
Python publication authority.

The sole retained exception is the exact `3.0.0` Release recovery already
observed by the registry authority. It is accepted only as this complete tuple:

- source tag `release-v3.0.0-crates`, annotated object
  `0bc6fe56498472804c3cc376b5b28d7652955701`;
- source commit `24bce1accdff8d14127797afe6f237a57c1cd4f3` and tree
  `52bdbb3ea7cd8d12e2cbd6cbe5f53cbcaa80d0ff`;
- Release run `33838703304`, attempt `1`, `event=workflow_dispatch`, branch
  `main`, and run head/control SHA
  `8a58749d892a52e38c651669ade03df5a6ee54af`;
- release-evidence ref `refs/heads/main` and SHA-256
  `34c791a0cda982389cd55fb055376af20e174a7ef1921c4816d38ad6ec798c61`;
- SPDX SHA-256
  `c146fde572531fe665f8a2b1460035cb9deb251b95cb95ca65568865009ee209`
  and CycloneDX SHA-256
  `460093bcbe2943e4803e48225b624a41ff44599f98d028a39f2b23486fde472d`;
- G8 run `33836655173`, attempt `1`, `event=workflow_dispatch`, branch
  `release/fix/release-readiness-semver-offline-merge-evidence`, head SHA equal
  to the source commit, and aggregate SHA-256
  `41dacc41bde4420ec3f2d735828669231966dd53c545a2fdd7a6bf0691205ebf`.

Changing any recovery source, run, ref, head, or evidence digest fails closed.
These retained Release facts do not establish a TestPyPI or PyPI publication;
only a terminal Python publication receipt can do that.

The PyPI dispatch downloads the named receipt artifact from that exact prior
run, including the already-published wheel and sdist. It accepts only a
terminal published TestPyPI receipt with the same source commit/tree, version,
annotated tag object, wheel, sdist, and canonical workflow identity. The fresh
double build is a reproducibility check; PyPI uploads the exact distribution
bytes retained by the TestPyPI run, not those rebuilt in the PyPI run. PyPI is
never a direct first publication and a PyPI receipt cannot substitute for the
TestPyPI authority.

TestPyPI intentionally precedes the signed release so public-package
conformance can run without granting production publication authority. All
Release/G8 inputs must therefore be empty for TestPyPI. PyPI is different: it
fails closed unless the exact tagged Release run and exact G8 closure run are
terminally successful and source-bound to the same tag, commit, and tree. The
workflow downloads their named artifacts by exact run ID, verifies the live
run attempts and canonical workflow paths, validates release evidence against
the live annotated tag, checks both SBOM hashes, and requires a closed
`claims=["G8"]` aggregate. Expired, missing, duplicated, or digest-mismatched
authority is rejected before OIDC publication.

The v2 receipt embeds a `hyphae-python-publication-authority-v1` object. It
contains `source`, exactly two `independent_builds` (builder receipt digest,
immutable GitHub artifact identity/digest, toolchain, runner image, and
distributions), and `release_authority`. The last field is `null` for
TestPyPI. For PyPI it contains the exact Release run and GitHub artifact
identity/digest, release-evidence filename/digest, SPDX and CycloneDX
filenames/digests, and the exact G8 closure run, artifact
identity/digest, aggregate digest, claim, and closure declaration. This object
is generated from independently downloaded bytes and live GitHub metadata; it
is not supplied as an unverified operator assertion.

New v2 receipts require the canonical tag and retain its annotated tag object;
their Release evidence record also retains the exact tag or recovery ref. The
schema and semantic validator continue to accept the previous `vVERSION`
source shape under the current Foundation workflow identity. That compatibility
is validation-only and does not broaden authority to receipts issued under a
different repository identity: neither the live workflow nor the receipt
builder can generate another legacy-tag receipt.

Before accepting that prior run, the workflow queries the GitHub Actions API
and requires the exact run to be `completed/success`, dispatched by the
canonical workflow from `main`, with matching workflow SHA and run attempt.
The observed run metadata is retained in the PyPI build receipt; fields stated
only by a downloaded JSON receipt are not trusted as run authority.

## Evidence and failure handling

Both independent builder jobs use the same source timestamp and normalized
sdist. Their wheel and sdist bytes must match across runners. A separate,
non-privileged `candidate-validation` job depends on those builders, downloads
their artifacts by name, runs the SDK suite, validates both archive forms, and
installs and imports the candidate wheel and sdist. It produces no artifact or
output consumed by the authority path. The `build` authority job waits for that
validation to succeed, but downloads only the original independent-builder
artifacts. It never imports candidate modules or installs candidate
distributions: it parses the tagged manifest and authority evidence, validates
each downloaded wheel and sdist with the canonical control-plane archive
checker, compares and rehashes their bytes, and assembles the receipt. The OIDC
job then only downloads that immutable artifact by ID and publishes its
retained bytes. It never uses `skip-existing`; an ambiguous upload requires
operator review.

After upload, the verifier requires the registry inventory to contain exactly
the receipt filenames and SHA-256 digests. It installs both the registry wheel
and the registry sdist on Python 3.11 and 3.14; the sdist build uses the pinned
backend without an unreviewed isolated build environment. After each successful
import, that environment's own interpreter writes canonical JSON installation
evidence. Each of the four files binds the Python boundary, wheel/sdist kind,
exact package version, retained distribution filename and SHA-256, observed
CPython version, and passed status. The verifier rejects missing, duplicate,
unknown, non-canonical, or source-unbound evidence and retains each evidence
SHA-256 plus its observed fields in `registry_verification`.
The expected distribution digests reach the installation step through outputs
computed from the independently retained bytes; they are not inferred from an
installer cache or from the installed package.

It then queries the PEP 740 Integrity API for both files. The receipt retains the PyPI-verified
GitHub Trusted Publisher identity, publish predicate, filename, subject digest,
and material installation evidence. The `attestations: true` action input alone is
not treated as provenance evidence.

The pinned `pypi-attestations` verifier uses production trust roots for both
registries. TestPyPI attestations emitted by `gh-action-pypi-publish` are signed
under those production roots, so verification deliberately does not pass
`--staging` for either TestPyPI or PyPI. This is local cryptographic verification
of the selected Integrity API provenance and retained distribution bytes.
If the registry inventory, provenance, supported interpreter checks, or exact
receipt promotion is unavailable, publication remains open; do not fabricate
or hand-edit a receipt.

The v2 JSON contract is structural; cross-field equality and publication
semantics are enforced by the mandatory
`tools/python_distribution_receipt.py` validator. The schema is
[`schema/python-distribution-receipt-v2.schema.json`](schema/python-distribution-receipt-v2.schema.json).
