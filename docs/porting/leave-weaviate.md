<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Leave Weaviate with a receipt

No system should hold your data hostage — least of all one that cannot
prove what it returned. This guide moves one Weaviate class into a Native
search collection through public APIs only, and hands you a sealed
accounting of exactly what transferred, what changed, and what was lost.
No migration in the other direction produces one.

## Protocol

The importer (`tools/weaviate_import.py`) exports through Weaviate's
public REST cursor — its storage format is not a stable contract, so the
API is the honest consistency point — and ingests into a provisioned
collection:

```bash
hyphae init --data-dir ./migrated
hyphae catalog --data-dir ./migrated create-search-collection \
  --database 10 --schema 11 --collection 13 --analyzer 12 \
  --name main.public.migrated --dimension 384
hyphae search --data-dir ./migrated provision --collection 13

python3 tools/weaviate_import.py \
  --endpoint http://127.0.0.1:8080 --class-name Documents \
  --text-property text --vector-target exact \
  --binary hyphae --data-dir ./migrated \
  --output import-receipt.json
```

## The receipt

`hyphae-weaviate-import-receipt-v1` borrows the G10 external-migration
fidelity-class pattern:

- **Source identity**: the server version, the class, and the SHA-256 of
  the canonical sorted export — rerun the export and the digest either
  reproduces or the source changed.
- **Consistency point**, stated honestly: a live cursor export can miss
  writes concurrent with it; quiesce writes for a point-in-time claim.
- **Fidelity classes** per construct:
  - `objects` are **exact** — source UUIDs map one-to-one onto 128-bit
    document identities and the text ingests byte-exactly;
  - `vectors` are **equivalent** — the floats transfer exactly, and the
    ANN graph is rebuilt deterministically rather than copied, so recall
    is re-measured, not assumed;
  - quantized vector configurations (PQ/BQ/SQ/RQ) are
    **declared-degraded** and abort the import unless the operator
    passes an explicit `--waive-degraded`;
  - multi-tenant classes are **rejected** per run — each tenant maps to
    its own target directory.
- **Verification**: the target collection's document count is re-read
  through the shipped binary and compared against the export.

## After the move

Re-measure relevance instead of trusting it: point `tools/rag_eval.py`
at a pinned dataset in the migrated directory, or replay your own query
mix, and compare against the numbers the source produced. The
[claim protocol](../retrieval/claims-protocol.md) is the standard the
migrated collection is now held to — including byte-identical committed
state across hosts and offline-verifiable retrieval, properties the
source never offered.
