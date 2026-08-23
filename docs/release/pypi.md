# Publish the Python SDK

Hyphae publishes the pure-Python client as the `hyphae-sdk` distribution. Its
import package is `hyphae_sdk`. The `hyphae` name on PyPI belongs to an
unrelated project and must never be used by this repository.

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
2. Create the immutable annotated `vVERSION` tag. The Python version must equal
   `workspace.package.version` in the source tree.
3. Dispatch `Python package` from `main` with `source_tag=vVERSION`,
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

The PyPI dispatch downloads the named receipt artifact from that exact prior
run, including the already-published wheel and sdist. It accepts only a
terminal published TestPyPI receipt with the same source commit/tree, version,
wheel, sdist, and canonical workflow identity. The fresh double build is a
reproducibility check; PyPI uploads the exact distribution bytes retained by
the TestPyPI run, not those rebuilt in the PyPI run. PyPI is never a direct
first publication and a PyPI receipt cannot substitute for the TestPyPI
authority.

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
