# Third-party notices

This distribution contains third-party components recorded in the exact locked
inventories `Cargo.lock` and the repository `package-lock.json` files. Their
copyrights and licenses remain with their respective authors. The project
Apache-2.0 grant does not relicense those components.

The `1.2.0` relicensing-transition inventory aggregates the Rust, JavaScript,
and Python dependencies reviewed at that transition, by exact digest, in
`docs/gates/evidence/relicensing-1.2.0-dependency-license-aggregate.json`. The
shipped dependency closure of each release is its SBOM, attached to that
release's GitHub release.

## DCO 1.1

`DCO` is the canonical Developer Certificate of Origin 1.1, copyright The Linux
Foundation and its contributors. It is third-party legal material distributed
verbatim under the copy-and-distribute-verbatim permission stated in that file.
It is not a project relicensing grant.

## Rust dependency bundle

The shipped Rust product dependency closure is attributed by package name,
version, source, and declared license in `Cargo.lock`,
`config/native-dependency-policy.json`, and the release SBOM. Release archives
bundle this notice and the exact SBOM; source archives retain the dependency
license files supplied by each package. `cargo deny check` validates the locked
license expressions before release.

## JavaScript tooling boundary

The JavaScript SDK, optional adapters, host build smoke tests, and MCP host
conformance each have an exact `package-lock.json` with registry URL, integrity,
version, and license metadata. These development graphs are not included in the
native Hyphae runtime archive.

`sharp` is Apache-2.0. Its reviewed optional prebuilt `@img/sharp-libvips-*`,
`@img/sharp-wasm32`, and `@img/sharp-win32-*` packages include an
LGPL-3.0-or-later component. Anyone redistributing a covered binary must
preserve the applicable notices and license text; provide complete
corresponding source, or a valid written or network source offer permitted by
LGPL-3.0-or-later; and preserve recipients' practical right to replace or
relink the covered library, including required installation information. These
are development-only packages and must not be included in Hyphae runtime, SDK,
crate, Python, npm, or native release archive payloads.

`@anthropic-ai/claude-code` and its platform packages are proprietary tooling
whose package metadata says `SEE LICENSE IN README.md` or
`SEE LICENSE IN LICENSE.md`. They are pinned solely for opt-in development and
real-host conformance. They are not a Hyphae runtime dependency, are not
covered by Hyphae's Apache-2.0 license, and must never be included in Hyphae
runtime, SDK, crate, Python, npm, or release archive payloads. Use and
redistribution remain subject to Anthropic's terms.

## Python build boundary

The Python SDK has no runtime dependency. Its exact build dependency is
`setuptools==84.0.0`, licensed MIT. Build dependency identity is recorded in
`sdks/python/pyproject.toml`; release wheels and sdists contain Hyphae's own
license documents and do not bundle setuptools.

## Optional multilingual embedding measurement boundary

The standalone multilingual embedding harness names
`Qwen/Qwen3-Embedding-0.6B`, PyTorch, Transformers, Tokenizers, Safetensors,
CUDA, and NVIDIA tooling only as external measurement subjects. None is
bundled in Hyphae or enters its workspace, product dependency graph, or
runtime. The checked-in model manifest is deliberately unverified and contains
no weight or license digest. A measurement is refused until an operator binds
an asserted full commit through checksummed responses retained from
commit-specific Hugging Face model/tree API URLs and every local file digest.
This operator-retained record preserves the observed response bytes but is not
an independent authenticity anchor for the historical HTTPS exchange. The API's
`Apache-2.0` declaration is recorded separately from whether license text is
bundled, and an independently checksummed legal-review record controls whether
the operator's policy would otherwise allow claims. This harness has no
approved signed legal, acquisition, or hardware trust anchor, so its receipts
remain non-authoritative and cannot unlock product claims. Measurement receipts
inventory files declared by installed Python distributions and native artifacts
observed at declared capture phases; they do not redistribute those files. All
external files retain their own terms as recorded in the populated measurement
manifest and receipt.
