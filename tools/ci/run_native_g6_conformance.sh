#!/usr/bin/env sh
# SPDX-License-Identifier: AGPL-3.0-only
set -eu

platform=${G6_PLATFORM:?G6_PLATFORM must be linux, macos, or windows}
output=${G6_RECEIPT:-target/g6/native-g6-conformance-${platform}.json}
python3 tools/run_native_g6_conformance.py --platform "$platform" --output "$output"
