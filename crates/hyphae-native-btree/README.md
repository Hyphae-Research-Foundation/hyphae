# hyphae-native-btree

```toml
[dependencies]
hyphae-native-btree = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

This unpublished crate owns Hyphae's first immutable copy-on-write B+tree.
Keys and values are canonical binary bytes. Every update appends new native
leaf/internal pages and returns a new root; historical roots remain readable.
Point reads can use the Hyphae partitioned buffer pool without changing page
ownership or immutability.

The first implementation favors fail-closed validation and correctness. Prefix
compression, bulk loading, fill-factor tuning, latching, range cursors, and
buffer-pool integration remain later work.
