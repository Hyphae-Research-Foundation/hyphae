# ADR-0034: Bound optional CUDA execution to validated H100 profiles

- Status: Accepted
- Date: 2026-09-22
- Owners: Celiums Solutions LLC

## Context

Hyphae needs an optional accelerator path for catalog-bound Qwen3 embedding
without making CUDA part of the offline product, adding Hyphae-owned unsafe
code, or allowing one durable batch to mix CPU and GPU results. CUDA libraries
use unsafe FFI internally, GPU work is not cooperatively preemptible while a
kernel is running, and device loss or out-of-memory failure can occur after
some in-memory vectors have been calculated.

The existing executor uses Candle 0.9.2 and already separates complete model
execution from the product transaction that publishes vectors. Candle's CUDA
feature builds PTX for the detected compute capability and uses cudarc's
driver, BLAS, and runtime bindings.

## Decision

The existing optional `hyphae-native-embed-cpu` publication owns both the
portable CPU executor and a default-off `cuda` feature. `hyphae-cli` exposes
the same default-off feature so embedded, UDS, and HTTP execution can install
the selected backend. The feature uses Candle 0.9.2 and safe cudarc 0.19
driver and runtime queries. Hyphae source remains under
`unsafe_code = "forbid"`; custom CUDA FFI, custom kernels, and CubeCL are not
accepted by this decision.

`Qwen3AcceleratorExecutor` enumerates visible CUDA devices in ordinal order and
selects only a device whose driver identity is exactly `NVIDIA H100 80GB HBM3`,
whose compute capability is exactly 9.0, and whose memory is at least 79 GiB.
Its execution profile includes the selected ordinal, name, `sm90`, CUDA UUID,
and PCI domain, bus, and device identity. The result profile also records
the observed NVIDIA driver release, CUDA driver/runtime API versions, Candle
version, model revision and manifest, compute dtype, and canonical FP32 output.
No compatible device, driver-query failure, or Candle-device construction
failure selects the complete existing CPU path before any model is loaded.

Backend selection is immutable for the life of a registry. A CUDA-selected
registry loads a CPU/f32 copy from cloned descriptors only after the same
manifest and every artifact have been verified. An `unavailable` CUDA result
is discarded and the executor reruns every input as one complete CPU batch.
Cancellation, deadline, validation, and limit errors are not fallback signals.
The execution profile declares the whole-batch fallback, and no request can
return a mixture of CPU and GPU vectors.

H100 model compute is explicitly BF16 or FP16. Last-token output is converted
on-device to FP32 before transfer, finite-value validation, truncation, and L2
normalization. The product receives only complete FP32 vectors. CUDA kernel,
transfer, out-of-memory, or device-loss failures produce no GPU batch; they
either fail closed or complete the declared whole-batch CPU retry. Cancellation
and deadline errors always fail closed. The product transaction starts only
after one complete batch returns and validates, so failures publish no partial
documents, vectors, or completion marker. The complete execution profile is
stored in the versioned `HYPEMB02` marker with the original transaction ID;
replay returns the original commit and backend identity without an executor.
Cooperative deadlines are checked
between bounded token chunks; they do not claim to preempt a running CUDA
kernel.

## Consequences

- Default builds and CPU-only hosts do not compile or require CUDA.
- Accelerator builds retain the same artifact verification, resource bounds,
  model registry key, product trait, and complete CPU implementation.
- A validated H100 is selected automatically and reported exactly rather than
  represented by a generic `gpu` label.
- Runtime GPU output is never partially accepted. The declared CPU fallback
  restarts the complete batch and returns only CPU vectors.
- Candle and cudarc retain their own audited unsafe FFI boundaries; no unsafe
  block is admitted to a Hyphae crate.

## Alternatives considered

- Hyphae-owned CUDA FFI or kernels were rejected because they would widen the
  unsafe boundary and require a separate kernel audit and support matrix.
- CubeCL was rejected because the CPU executor already uses Candle and Candle
  0.9.2 builds and runs `sm90` PTX on the validated target.
- Per-input or suffix fallback was rejected because it can publish a batch
  produced by two numeric profiles. Whole-batch retry discards every GPU vector
  and is declared in the execution profile.
- Adding another publishable backend crate was rejected because a default-off
  feature preserves one artifact-verification and registry authority.

## Verification

`cargo test -p hyphae-native-embed-cpu --features cuda --locked` covers the
shared CPU and CUDA registry code. The opt-in
`real_h100_bfloat16_and_float16_run_under_lock` test requires a real artifact,
an operator-supplied exclusive lock path, and expected NVML UUID and PCI
identity. Under that lock it requires the validated H100 profile, loads the
real Qwen3-Embedding-0.6B snapshot, and executes both BF16 and FP16 paths to a
finite 384-element FP32 product vector. H100 evidence also records consistent
CUDA-driver and NVML name, compute capability, UUID, and PCI bus identity.
