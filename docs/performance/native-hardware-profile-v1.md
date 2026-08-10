# Native hardware profile v1

Status: Linux, macOS, and Windows discovery adapters implemented; topology
enrichment remains open

`HardwareProfile` is the read-only discovery surface for hardware-aware Native
execution. The embedded Rust API is
`hyphae_native_runtime::HardwareProfile::discover`; the CLI surface is:

```text
hyphae hardware discover [--data-dir <PATH>]
```

The optional data path selects the filesystem and block device to inspect. It
does not create or open a Hyphae directory. For an absent path, discovery uses
the nearest existing ancestor.

The public shape is versioned by
[`native-hardware-profile-v1.schema.json`](../../contracts/json-schema/native-hardware-profile-v1.schema.json).
Its independent semantic checker is:

```text
python3 tools/check_native_hardware_profile.py --profile <PROFILE.json>
```

Discovery never changes affinity, CPU frequency policy, huge pages, mount
options, device queues, or any other host setting.

## Stable identity and snapshot state

The fingerprint is BLAKE3 over the compact, declaration-ordered JSON encoding
of the schema identity and scheduling-relevant CPU, installed-memory, page,
storage, and operating-system fields. The selected path and available memory
are observations but are excluded from the fingerprint: neither changes the
hardware or policy identity. A filesystem, mount policy, affinity, CPU quota,
kernel, topology, or instruction-set change does change the fingerprint.

The CPU profile distinguishes admitted logical processors, visible physical
cores, uniform SMT width, sockets, affinity, quota, cache-sharing domains, and
instruction sets. Linux additionally records every admitted logical processor
with its physical core, socket, NUMA node, and canonical SMT sibling set. It
also reports each visible NUMA node with its admitted CPU list and installed
memory; transient node availability is excluded from the fingerprint. The
semantic checker rejects duplicate processors, sibling sets that cross cores,
topology outside process affinity, inconsistent core/socket counts, and NUMA
maps that disagree with processor placement. The operating-system profile
lists only local transports compiled for that platform.

Unavailable properties remain JSON `null`, an empty capability list, or the
explicit string `unknown`. They are never encoded as zero and never inferred
from a cloud instance label. Linux discovery reads procfs, sysfs, cgroup v2,
and mountinfo. macOS discovery uses read-only `sysctl` and mount reporting.
Windows executes one fixed, non-interactive, read-only PowerShell/CIM probe;
the selected path is passed only through an environment variable and is never
interpolated into script text. The adapter reports physical/logical processor
and socket counts, installed/available memory, page size, kernel version,
logical-disk filesystem/device, virtualization, and named-pipe transport.
Windows process affinity masks, NUMA nodes, cache domains, and device queue
properties remain explicit unknowns until their safe native adapters exist.
Other platforms return the portable subset.

The profile is input to the separately versioned
[`Native hardware calibration v1`](native-hardware-calibration-v1.md). It does
not itself choose worker counts, enable SIMD kernels, or claim bare-metal
qualification. On Linux, calibration can derive an independently checked,
hard-affinity worker recommendation from this topology; other adapters remain
explicitly unbound. The governor consumes either form only when the profile
fingerprint and recomputed curve match. Cross-platform affinity/NUMA decisions
and performance claims still require the remaining P1 qualification evidence.
