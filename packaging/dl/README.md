<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# dl.hyphae.dev

The distribution front door: a Cloudflare Worker serving the installer
and stable redirects. Every binary byte still comes from GitHub
Releases with its provenance and SHA256SUMS; the worker never hosts
artifacts.

| Path | Answer |
|---|---|
| `/` or `/install` | the installer script (`install.sh`) |
| `/latest` | redirect to the latest GitHub release |
| `/sums` | redirect to the latest `SHA256SUMS` |
| `/aur` | redirect to the `hyphae-bin` AUR packaging repository |
| anything else | redirect to the repository |

## Install one-liner

```bash
curl -fsSL https://dl.hyphae.dev/install | sh
```

The script detects the platform, downloads the official release
artifact, verifies its SHA-256 against the release's `SHA256SUMS`,
installs to `~/.local/bin` without sudo, and suggests
`hyphae agent setup` as the explicit next step. On Arch/Omarchy it
defers to the AUR package unless `HYPHAE_FORCE=1`. `HYPHAE_VERSION`
pins a version; `HYPHAE_BIN_DIR` overrides the destination.

## Updating the worker

Edit `install.sh`, run `./build_worker.sh`, and upload the regenerated
`worker.js` module to the `hyphae-dl` worker in the foundation's
Cloudflare account. The generated `worker.js` is not committed; the
template plus the script are the source of truth.
