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
| `3.0.0` | Yes, current release line |
| `2.2.0` | Security fixes only, until the next 3.x minor is released |
| Older `2.x`, `1.x`, `0.x` | No |

## Baseline security guarantees

- The server binds to loopback by default.
- Remote binding requires explicit configuration and authentication.
- Inputs have body, depth, batch, result, timeout, and concurrency limits.
- Corrupt or future on-disk formats fail closed.
- Result and retrieval proofs are verifiable offline under explicit resource
  limits.
- External providers are optional and cannot enter the core dependency path.

These guarantees are release requirements. The `3.0.0` release evidence
(signed archives, SBOMs, provenance, and G8 closure) is bound to its exact
release commit; see `docs/release/verification.md` for how to verify it.
