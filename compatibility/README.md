# On-disk compatibility fixtures

Each versioned fixture is a byte-for-byte historical Hyphae data directory.
The engine test reconstructs it without generated indexes, opens it, verifies
the expected records, and proves that durable idempotency receipts survive.

Fixtures are immutable once their disk format ships. A new disk format adds a
new directory and test case; it never rewrites an older fixture.

The format-1 fixture is frozen release history. Its pre-release generator
remains available only as a reproducibility check:

```sh
python3 tools/generate_compatibility_fixture.py \
  --binary target/debug/hyphae \
  --check compatibility/v1/data-directory.json
```

The generator deliberately omits the materialized Redb index so the test also
proves that recovery reconstructs disposable indexes from authoritative data.

The immutable format-2 fixture includes KV records, a vector-space definition,
durable signed-Q15 vectors, a lexical-index definition, and all idempotency
receipts. Reproduce it with:

```sh
cargo test -p hyphae-engine --example generate_disk_format_2_fixture
cargo run -q -p hyphae-engine --example generate_disk_format_2_fixture -- \
  /tmp/hyphae-format-2-fixture
```

The generator's test compares the semantic JSON against
`v2/data-directory.json`; the checked-in fixture omits Redb and therefore also
proves snapshot-driven reconstruction of every materialized retrieval table.

## Native protocol SDK fixture

`native-protocol-v1-structure-get.bin` is one complete canonical `HYPHLCL1`
frame containing a `HYPREQ01` structure-get request. Rust, Python, and
TypeScript tests independently encode it and decode/re-encode it byte for byte.
Regenerate it only when intentionally changing the append-only native protocol:

```sh
cargo run -p hyphae-native-protocol --example generate_sdk_fixture
```

## Valkey/Redis RDB migration fixture

`valkey/rdb-v11.json` is one immutable RDB version-11 source payload for the
external migration path. It exercises every decoded encoding — raw and
integer strings, a hashtable hash, a listpack set, an intset set, a
quicklist2 list, a binary-double sorted set, and a stream with one consumer
group — across two databases with one absolute expiry and a valid CRC-64
trailer. The generator is self-contained so the fixture bytes never depend on
the parser under test, and its embedded test proves the checked-in JSON is
byte-exact. Reproduce it with:

```sh
cargo test -p hyphae-cli --example generate_valkey_rdb_fixture
cargo run -q -p hyphae-cli --example generate_valkey_rdb_fixture -- \
  /tmp/hyphae-valkey-rdb-v11.json
```

The packaged mirror `crates/hyphae-cli/tests/fixtures/valkey-rdb-v11.json`
feeds the black-box test that runs the complete
inspect → waive → run → verify → promote cycle against these exact bytes.
