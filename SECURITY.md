# Security policy

Do not disclose suspected vulnerabilities in public issues, discussions, pull
requests, or chat logs.

Report a vulnerability through GitHub private vulnerability reporting for
`Hyphae-Research-Foundation/hyphae`, or contact `hello@celiums.ai` if that
channel is not available. Include the affected revision, platform,
reproduction steps, impact, and any proposed mitigation.

## Supported versions

| Version | Supported |
|---|---|
| `4.0.0` | Yes, current source release line; publication remains gated |
| `3.0.0` | Security fixes only; current published release |
| `2.2.0` and older | No |

## Baseline security guarantees

- The server binds to loopback by default.
- Remote binding requires explicit configuration and authentication.
- Inputs have body, depth, batch, result, timeout, and concurrency limits.
- Corrupt or future on-disk formats fail closed.
- Result and retrieval proofs are verifiable offline under explicit resource
  limits.
- External providers are optional and cannot enter the core dependency path.

These guarantees are release requirements. The `4.0.0` source candidate
integrates Lane14 `f2336d8664b32d812cea3291d942e286a89eea9d` and requires a new
exact-SHA hosted security matrix and G8 closure before release. It inherits no `3.0.0` release evidence.
The signed `3.0.0` archives, SBOMs, provenance, and G8 closure remain bound to
their historical release commit; see `docs/release/verification.md`.
