#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run `npm audit --audit-level=moderate`, retrying only when the registry's
# advisories endpoint itself failed. A real finding, a lockfile problem, or
# any other npm error exits with npm's status on the first attempt; the
# endpoint's intermittent 503s to hosted runners are the only thing retried,
# and the gate still fails closed when the endpoint never answers.
#
# Usage: tools/npm_audit.sh [npm options such as --prefix DIR]
set -uo pipefail

attempts=6
for attempt in $(seq 1 "$attempts"); do
  output=$(npm "$@" audit --audit-level=moderate 2>&1)
  status=$?
  printf '%s\n' "$output"
  if [ "$status" -eq 0 ]; then
    exit 0
  fi
  if ! grep -q "audit endpoint returned an error" <<<"$output"; then
    exit "$status"
  fi
  if [ "$attempt" -lt "$attempts" ]; then
    echo "npm audit endpoint unavailable (attempt $attempt of $attempts); retrying" >&2
    sleep $((attempt * 30))
  fi
done
echo "npm audit endpoint unavailable after $attempts attempts" >&2
exit 1
