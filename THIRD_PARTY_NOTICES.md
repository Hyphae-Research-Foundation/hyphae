# Third-party notices

This distribution contains third-party components recorded in the exact locked
inventories `Cargo.lock` and the repository `package-lock.json` files. Their
copyrights and licenses remain with their respective authors. The project
Apache-2.0 grant does not relicense those components.

The current reviewed Rust, JavaScript, and Python inventory is aggregated by
exact digest in
`docs/gates/evidence/relicensing-1.2.0-dependency-license-aggregate.json`.

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
`setuptools==80.9.0`, licensed MIT. Build dependency identity is recorded in
`sdks/python/pyproject.toml`; release wheels and sdists contain Hyphae's own
license documents and do not bundle setuptools.
