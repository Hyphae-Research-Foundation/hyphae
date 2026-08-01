# hyphae-native-catalog

Internal, unpublished immutable catalog model shared by Hyphae's native
relational, structure, and search engines. It owns the bounded canonical
`HYCOBJ01` definition codec used by runtime catalog roots and WAL mutations.

The normative target contract is
[`docs/native/catalog-v1.md`](../../docs/native/catalog-v1.md). The runtime now
persists complete definitions in `HYCAT002` and reads legacy `HYCAT001`.
Catalog B+tree scaling, definition history, dependencies, indexes, constraints,
and schema evolution remain partial G1/G2 work.
