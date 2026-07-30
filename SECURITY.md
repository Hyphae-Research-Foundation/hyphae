# Security policy

Do not disclose suspected vulnerabilities in public issues, discussions, pull
requests, or chat logs.

Report a vulnerability through GitHub private vulnerability reporting for
`celiumsai/hyphae`, or contact `security@celiums.ai` if that channel is not
available. Include the affected revision, platform, reproduction steps,
impact, and any proposed mitigation.

## Supported versions

| Version | Supported |
|---|---|
| `0.2.0` | Yes |
| `0.1.0` | Yes |
| `0.2.1` source candidate | Not released |
| `< 0.1.0` | No |

## Baseline security guarantees

- The server binds to loopback by default.
- Remote binding requires explicit configuration and authentication.
- Inputs have body, depth, batch, result, timeout, and concurrency limits.
- Corrupt or future on-disk formats fail closed.
- Result and retrieval proofs are verifiable offline under explicit resource
  limits.
- External providers are optional and cannot enter the core dependency path.

These guarantees are release requirements. The historical `0.2.0` gate
records complete local evidence, but this repository does not retain enough
commit-bound hosted receipts to close its two hosted items. The published tag,
GitHub release, and registry entries do not substitute for that missing
evidence. Any source change, including the `0.2.1` candidate, requires the
complete gate matrix to pass again on one exact commit before it becomes a
supported release.
