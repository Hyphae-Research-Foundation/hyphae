#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Sync the working tree to the DigitalOcean devbox and run a command there.
# Usage: tools/devbox.sh [command...]   (default: full check suite)
# The devbox is a c-16 droplet; keep it deleted when idle (doctl compute
# droplet delete hyphae-devbox).
set -euo pipefail
DEVBOX_IP="${HYPHAE_DEVBOX_IP:-198.199.77.236}"
SSH_KEY="${HYPHAE_DEVBOX_KEY:-$HOME/.ssh/celiums-workers}"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

rsync -az --delete -e "ssh -i $SSH_KEY" \
  --exclude target --exclude .git \
  --exclude 'sdks/typescript/node_modules' --exclude 'sdks/typescript/dist' \
  "$REPO_DIR/" "root@$DEVBOX_IP:/workspace/hyphae/"

if [ $# -eq 0 ]; then
  set -- bash -c 'cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings && cargo test --workspace --all-features --locked'
fi
ssh -i "$SSH_KEY" "root@$DEVBOX_IP" "cd /workspace/hyphae && source ~/.cargo/env && $*"
