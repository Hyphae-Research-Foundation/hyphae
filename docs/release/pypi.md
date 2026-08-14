# Publish the Python SDK

Hyphae publishes the pure-Python client as the `hyphae-sdk` distribution. Its
import package is `hyphae_sdk`. The `hyphae` name on PyPI belongs to an
unrelated project and must never be used by this repository.

Publication is a release gate, not a maintainer workstation command. The
top-level `Python package` workflow builds an sdist and universal wheel from an
existing immutable annotated tag, verifies their contents, and publishes with
PyPI Trusted Publishing. No long-lived PyPI token is stored in GitHub.

## One-time registry setup

Configure pending Trusted Publishers separately on TestPyPI and PyPI:

- owner/repository: `celiumsai/hyphae`;
- workflow: `python-publish.yml`;
- TestPyPI environment: `testpypi`;
- PyPI environment: `pypi`;
- project name: `hyphae-sdk`.

Protect both GitHub environments. Require review for `pypi`; TestPyPI may use a
separate, less restrictive reviewer policy. Neither environment contains a
password or API token. The publish job alone receives `id-token: write`.

## Release sequence

1. Land the exact SDK version and complete hosted conformance.
2. Create the immutable annotated `vVERSION` tag. Its version must equal
   `sdks/python/pyproject.toml` exactly.
3. Dispatch `Python package` once with `source_tag=vVERSION` and
   `repository=testpypi`.
4. Download the publication receipt, create a clean Python 3.11 environment,
   install `hyphae-sdk==VERSION` only from TestPyPI, and run the public local
   and HTTP conformance clients against the tagged Hyphae binary.
5. After that receipt is accepted, dispatch the same tag once with
   `repository=pypi`.

The workflow verifies that the ref is an annotated tag, binds its peeled commit
to the checkout, builds with a pinned backend and source timestamp, normalizes
the sdist, rejects unsafe archive members, and records exact SHA-256 digests.
The privileged job receives only those already-built artifacts. PyPI generates
Sigstore-backed digital attestations from the same short-lived OIDC identity.

After upload, the workflow queries the selected registry and requires the
published filenames and SHA-256 digests to equal the build receipt. A missing,
partial, duplicated, or different registry release fails closed. It never uses
`skip-existing`; an ambiguous upload must be resolved by inspecting the
registry and receipt before any operator action.
