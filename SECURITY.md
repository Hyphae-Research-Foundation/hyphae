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
composes selected integration `d9af7f4393b1ef17536e3ee20d911e5f3c8f0976`
but must complete a new exact-SHA hosted security matrix and G8 closure before
publication. It does not inherit the `3.0.0` release evidence. The signed
`3.0.0` archives, SBOMs, provenance, and G8 closure remain bound to their exact
historical release commit; see `docs/release/verification.md` for how to verify
them.
