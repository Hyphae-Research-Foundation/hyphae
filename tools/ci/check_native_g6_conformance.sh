#!/usr/bin/env sh
set -eu

python3 tools/check_native_g6_conformance.py aggregate \
  --receipt "${G6_LINUX_RECEIPT:?missing G6_LINUX_RECEIPT}" \
  --receipt "${G6_MACOS_RECEIPT:?missing G6_MACOS_RECEIPT}" \
  --receipt "${G6_WINDOWS_RECEIPT:?missing G6_WINDOWS_RECEIPT}" \
  --output "${G6_AGGREGATE:-target/g6/native-g6-conformance-aggregate.json}"
