#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Regenerates worker.js from install.sh (base64-embedded) for the
# hyphae-dl Cloudflare Worker behind https://dl.hyphae.dev.
# Deploy: PUT the module to
#   accounts/<ACCOUNT>/workers/scripts/hyphae-dl
# with main_module=worker.js; the custom domain dl.hyphae.dev is bound
# to the service. Credentials come from the operator's Cloudflare
# account and are never stored in this repository.
set -eu
cd "$(dirname "$0")"
B64=$(base64 -w0 install.sh)
sed "s#^const INSTALL_B64 = .*#const INSTALL_B64 = \"$B64\";#" worker.template.js > worker.js
echo "worker.js regenerated"
