# Native mixed-durability scheduler evidence — 2026-08-02

## Scope

This evidence covers the bounded native commit scheduler described by
[Native mixed-durability scheduler v1](../../native/mixed-durability-scheduler-v1.md).
It demonstrates one FIFO writer-admission policy for detached `strict`,
`group`, and `memory` transactions. It does not close the complete G1 resource
policy or prove a production starvation bound.

The benchmark source is commit
`3ddee943f5b23cdef20d4a50d274fe0e690e6f43`, tree
`7f38147621a4acbed0f8a2006fbf7264c9301df8`, built with Rust
`1.96.0`.

## Implemented behavior

- All durability classes enter one bounded FIFO scheduler.
- Strict and memory requests execute as singletons.
- Only immediately consecutive group requests may share a cohort.
- A non-group command is retained as the next FIFO item instead of being
  skipped during group collection.
- Immediate admission returns typed saturation without mutation.
- Blocking admission never waits on the queue while holding the admission
  mutex.
- Controlled requests expose one atomic `queued → executing` boundary.
- Explicit cancellation or a queue deadline can win only while queued.
- Once executing, cancellation reports `too late` and the waiter receives the
  definite database outcome.
- Scheduler receipts separate admission wait, queue wait, execution, page
  synchronization, WAL synchronization, and end-to-end time.

No durable byte format changed.

## Red and green evidence

The mixed-durability test was committed before implementation and failed
because strict work reached the group-only path with
`GroupCommitRequiresGroupDurability`.

Cancellation tests were then committed before their API and failed with the
expected missing-type, missing-method, and missing-variant compiler errors.

After implementation:

- the native runtime suite passed 166 tests before the FIFO-barrier test was
  added;
- the deterministic `group → strict → group` test passed with CSNs `2, 3, 4`
  and singleton cohorts around the strict barrier;
- exact cancellation tests passed without consuming transaction ID or CSN;
- immediate saturation left the admission mutex acquirable; and
- strict clippy validation passed for the runtime and examples.

## Mixed-load benchmark

The benchmark uses eight synchronized producers for 32 rounds:

- six group producers: 192 commits;
- one strict producer: 32 commits; and
- one memory producer: 32 commits.

Every request writes a disjoint structure key. A final strict commit publishes
CSN 257 and serves as the durable fence. Reopen verification reads every
workload key plus that fence.

| Observation | Windows debug, NTFS | WSL2 release, tmpfs |
|---|---:|---:|
| Total throughput | 365.150 commits/s | 3010.017 commits/s |
| Maximum consecutive group commits | 11 | 8 |
| Group end-to-end p50 | 9,902,700 ns | 1,378,426 ns |
| Strict end-to-end p50 | 8,801,700 ns | 1,177,259 ns |
| Memory end-to-end p50 | 6,143,800 ns | 1,344,500 ns |
| Group queue-wait p50 | 4,920,100 ns | 630,566 ns |
| Strict queue-wait p50 | 5,669,900 ns | 786,147 ns |
| Memory queue-wait p50 | 4,454,000 ns | 974,945 ns |
| Strict execution p50 | 1,986,400 ns | 142,365 ns |
| Memory execution p50 | 721,200 ns | 141,946 ns |

All 32 strict and all 32 memory requests completed under sustained group load.
This is finite no-starvation observation, not a formal or production-duration
fairness proof. The results also show that mixed-load end-to-end latency is
still millisecond-scale: queue wait is now the dominant pain point and the
microsecond target is not met.

## Environment boundaries

Windows benchmark data used `%TEMP%` on healthy NTFS. Windows release execution
was attempted but Application Control blocked the optimized executable with
`os error 4551`; the maintained Windows observation is therefore explicitly a
debug-profile result.

WSL2 ran Linux `6.18.33.1-microsoft-standard-WSL2`. Source lived on the `/mnt/e`
9p mount, while `CARGO_TARGET_DIR` and benchmark data lived on `/tmp` tmpfs.
The WSL2 result is useful for scheduler CPU/queue behavior but its
sub-microsecond synchronization calls are not persistent-filesystem or
power-loss durability evidence.

Raw observations:

- [Windows](native-mixed-scheduler-windows.json)
- [WSL2](native-mixed-scheduler-wsl2.json)

## Remaining work

- a sustained saturation/soak test with an explicit fairness SLO;
- worker-failure injection through the mixed queue;
- abandoned-waiter and saturated-shutdown executable coverage;
- maintenance commands under the same resource policy; and
- persistent Linux filesystem latency and power-loss evidence.

