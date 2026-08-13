<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->

# Native scheduler authority v1

Status: implementation contract; no P1, P2, or G7 closure claim

Hyphae derives execution decisions through one chain:

`HardwareProfile -> HardwareCalibration -> NativeGovernorPolicy -> NativeExecutionTopology + NUMA steal policy`

Validating those artifacts independently is insufficient. A policy from one
calibration or a topology from another profile can be individually well formed
while authorizing work that was never measured on the current machine. This
contract defines a composite, fail-closed authority check before dedicated
qualification.

## Required bindings

The checker accepts the four immutable JSON artifacts, the selected governor
mode, the exact source commit, and the calibrated executable. It must:

- run every existing semantic checker first;
- require a clean repository at the exact commit and record its canonical tree;
- recompute the executable BLAKE3 and bind it to the calibration receipt;
- require a stable calibration accepted for scheduling;
- bind the profile fingerprint to calibration, policy, and topology;
- bind the calibration cache key to the policy;
- recompute the calibrated worker limit, system reserve, schedulable worker
  count, I/O recommendation, memory headroom, admission queue, and all seven
  class limits;
- recompute physical-core-first, NUMA-grouped, SMT-ranked worker placement;
- require either a complete stable directed NUMA matrix for every used node or
  explicit unsupported coverage; recompute every threshold for the former and
  require disabled cross-node stealing for the latter;
- reject portable placement when the profile contains complete processor
  topology, and reject physical placement when topology is unavailable;
- require hard affinity exactly on Linux physical placement; and
- emit a deterministic audit containing the source commit, source tree,
  executable BLAKE3, and SHA-256 digest of every input artifact.

The audit carries no performance claims. It proves only that scheduler
decisions are the canonical consequence of the supplied measurement authority.
It records the NUMA policy schema, status, and every directed worker/home node
ratio and age threshold so a benchmark receipt cannot hide an uncalibrated or
disabled fallback behind a topology digest.

## Directed single-item execution

A single durable ANN partition may address one existing persistent worker by
the partition identifier modulo the canonical global worker count. The route
uses a capacity-one worker-local slot and the same worker wake authority as the
generic queue; it creates no auxiliary thread and never runs on the submitting
thread. A busy slot falls back atomically to the generic home-pool queue. Closed
admission fails instead of falling back. Workers serve at most one directed job
consecutively while generic work is eligible, so affinity cannot starve the
ordinary priority and calibrated-stealing policy.

Every routed ANN receipt counts `targeted_single_batches` and
`generic_single_fallback_batches`. A one-item parallel routing wave increments
exactly one of them; a multi-item wave increments neither. The counters are
checked additions and never wrap. Single-generation and direct serial execution
report zero for both counters. These counts describe dispatch, not a latency
claim; dedicated qualification remains the only performance authority.

## Failure model

Source/tree substitution, executable/calibration substitution, artifact
swapping, unstable calibration, a forged worker or I/O limit,
incorrect memory headroom, reordered workload classes, skipped physical cores,
premature SMT use, false NUMA grouping, incomplete directed NUMA evidence,
forged steal thresholds, and source-commit mismatch all fail
before a benchmark process starts.

## Local acceptance

The checker must pass a synthetic i7i-shaped fixture with 48 physical cores,
96 logical processors, SMT2, at least two NUMA nodes, and 768 GiB of memory.
Mutation tests must independently alter every cross-artifact identity and each
derived scheduling dimension. The dedicated host later supplies real
measurements; it does not replace these deterministic derivation tests.

The dedicated workflow installs the pinned `blake3==1.0.9` verifier and passes
the exact `target/release/hyphae` binary to the checker. A missing verifier,
dirty worktree, mismatched `HEAD`, or different executable fails before G7.

```sh
python3 -m pip install blake3==1.0.9
PYTHONPATH=. python3 tools/check_native_scheduler_authority.py \
  --profile /tmp/native-hardware-profile.json \
  --calibration /tmp/native-hardware-calibration.json \
  --policy /tmp/native-governor-policy.json \
  --topology /tmp/native-execution-topology.json \
  --mode mixed \
  --expected-commit "$(git rev-parse HEAD)" \
  --executable target/release/hyphae \
  --output /tmp/native-scheduler-authority.json
```
