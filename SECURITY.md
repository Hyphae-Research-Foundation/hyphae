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
| `4.0.0` | Yes, current published release |
| `3.0.0` | Security fixes only |
| `2.2.0` and older | No |

## Baseline security guarantees

- The server binds to loopback by default.
- Remote binding requires explicit configuration and authentication.
- Inputs have body, depth, batch, result, timeout, and concurrency limits.
- Corrupt or future on-disk formats fail closed.
- Result and retrieval proofs are verifiable offline under explicit resource
  limits.
- External providers are optional and cannot enter the core dependency path.

These guarantees are release requirements. The `4.0.0` release source
`7cbcf97d165beeb08aba27ff21203fa468f45bec` passed its hosted security
matrix and exact-SHA G8 closure. Its signed archives, SBOMs, and provenance
are bound to that source and the annotated tag; see `docs/release/verification.md`. The `3.0.0` evidence remains
bound to its historical source and is not inherited by 4.0.0.
