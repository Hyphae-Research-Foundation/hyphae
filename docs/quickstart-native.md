# Native quickstart

Status: current for `3.0.0` and later published Native releases; the
closed G0-G8 profiles and their receipts are indexed by the
[native gate status](gates/native-gate-status.md).

This guide exercises the Native SQL, structure, and integrated search engines
through one binary and one owned data directory. It starts no listener and
requires no database, cache, cloud service, embedding provider, or LLM.

## Install or build the binary

```bash
cargo install hyphae-cli --version 3.0.0 --locked
hyphae version --json
```

Building from source instead:

## Build and initialize

```bash
cargo build --release --locked -p hyphae-cli
export HYPHAE_DATA_DIR="$PWD/hyphae-native-data"
./target/release/hyphae init --data-dir "$HYPHAE_DATA_DIR"
./target/release/hyphae capabilities --data-dir "$HYPHAE_DATA_DIR"
```

The directory must not already exist. Later Native commands reopen the same
directory and fail rather than silently creating or converting state.

PowerShell uses `hyphae.exe` and `$env:HYPHAE_DATA_DIR`.

## Use Native SQL

```bash
./target/release/hyphae sql --data-dir "$HYPHAE_DATA_DIR" execute \
  --statement 'CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT NOT NULL)'
./target/release/hyphae sql --data-dir "$HYPHAE_DATA_DIR" execute \
  --statement 'INSERT INTO notes (id, body) VALUES (?, ?)' \
  --parameter 1 --parameter '"offline first"'
./target/release/hyphae sql --data-dir "$HYPHAE_DATA_DIR" prepared \
  --statement 'SELECT id, body FROM notes WHERE id = ?' --parameter 1
```

Parameters are canonical JSON scalars. Results disclose the catalog/root
identity and visible commit sequence needed to correlate cross-engine state.

## Use Native structures

```bash
./target/release/hyphae structure --data-dir "$HYPHAE_DATA_DIR" set \
  --key active-note --value 1 --expires-at-micros 4102444800000000
./target/release/hyphae structure --data-dir "$HYPHAE_DATA_DIR" get \
  --key active-note
./target/release/hyphae structure --data-dir "$HYPHAE_DATA_DIR" ttl \
  --key active-note
```

The typed `batch` and `read` forms cover strings, counters, hashes, lists,
sets, sorted sets, streams, scans, algebra, and atomic multi-operation writes.
Use `hyphae structure <subcommand> --help` for their bounded JSON envelopes.

## Use integrated lexical and vector search

```bash
./target/release/hyphae catalog --data-dir "$HYPHAE_DATA_DIR" \
  create-search-collection --database 10 --schema 11 --collection 13 \
  --analyzer 12 --name main.public.note_search --dimension 2
./target/release/hyphae search --data-dir "$HYPHAE_DATA_DIR" \
  provision --collection 13
./target/release/hyphae search --data-dir "$HYPHAE_DATA_DIR" ingest \
  --collection 13 --idempotency-id 1 \
  --documents-json '[{"id":1001,"text":"offline native search","doc_values":{"category":"note","price":1},"vectors":{"exact":[1.0,0.0],"ann":[1.0,0.0]}}]'
./target/release/hyphae search --data-dir "$HYPHAE_DATA_DIR" integrated \
  --collection 13 --lexical offline --vector-target exact \
  --vector 1 --vector 0 --vector-strategy exact --limit 10
```

Document, vector, and idempotency IDs are stable unsigned integers. The
integrated command uses one catalog snapshot and labels approximate execution
when ANN is selected.

## Checkpoint, back up, restore, and diagnose

```bash
./target/release/hyphae checkpoint --data-dir "$HYPHAE_DATA_DIR"
./target/release/hyphae doctor --data-dir "$HYPHAE_DATA_DIR"
./target/release/hyphae backup create --data-dir "$HYPHAE_DATA_DIR" \
  --out "$PWD/hyphae-native-backup"
./target/release/hyphae backup verify --backup "$PWD/hyphae-native-backup"
./target/release/hyphae restore --backup "$PWD/hyphae-native-backup" \
  --data-dir "$PWD/hyphae-native-restored"
./target/release/hyphae doctor --data-dir "$PWD/hyphae-native-restored"
```

Backup and restore destinations must be new. Restore verifies and stages the
complete Native directory before atomic activation; it never merges with an
existing destination.

## Start an explicit local service

Native state can expose the binary local protocol and optional loopback HTTP
`/v2` edge:

```bash
./target/release/hyphae serve --data-dir "$HYPHAE_DATA_DIR" \
  --endpoint "$PWD/hyphae.sock" --http-bind 127.0.0.1:8787
```

On Windows, `--endpoint` is a named-pipe identity. No listener starts unless
`serve` is selected. The canonical HTTP contract is
[`contracts/openapi/hyphae-v2.yaml`](../contracts/openapi/hyphae-v2.yaml); the
complete command inventory is in the [CLI reference](cli/reference.md).
