# G2 TPC-H correctness vertical

Status: bounded implementation evidence; G2 remains open.

`crates/hyphae-native-runtime/tests/tpch_correctness_g2.rs` implements a small,
deterministic TPC-H-derived customer/orders correctness vertical. It uses native
typed tables, secondary indexes, an indexed inner join, filtering, projection,
ordering, and an exact reference result.

The bounded corpus now also covers four admitted query shapes across
`customer`, `orders`, `lineitem`, and `supplier`: secondary-index filtering,
composite-primary-key prefix reads, a materialized CTE, and exact supplier
lookup. Each result is compared with a frozen reference row set.

The test deliberately records current optimizer requirements: the filtered
customer side and right-side join key need admitted indexes. This is useful
architectural evidence but is not TPC-H correctness closure. A versioned matrix
accounts for all 22 canonical query numbers exactly once: Q3 is marked as an
admitted derived vertical and the other 21 retain explicit unsupported-feature
reasons. This prevents a bounded subset from being reported as full TPC-H.
G2 still requires
the canonical schema, deterministic scale-factor generator and digests,
reference outputs for all admitted queries, unsupported-query accounting,
reopen equivalence, and hosted exact-SHA receipts. Performance claims remain
owned by G7.
