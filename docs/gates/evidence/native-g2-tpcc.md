# G2 TPC-C ACID vertical

Status: bounded implementation evidence; not promoted into the G2 closure at
`a839037`, which claims no universal SQL or official benchmark result.

`crates/hyphae-native-runtime/tests/tpcc_acid_g2.rs` implements a deterministic
New-Order-derived transaction over district and order rows. The tests prove:

- district sequence update and order creation publish atomically;
- a Payment-derived warehouse/district/customer update publishes atomically;
- a Delivery-derived carrier/customer update publishes atomically;
- Order-Status/Stock-Level-derived reads remain snapshot-consistent;
- overlapping New-Order attempts are resolved by first-committer-wins;
- the losing transaction cannot publish its alternate order total;
- strict-durability results survive reopen; and
- explicit rollback publishes neither the sequence update nor order row.

This is not complete TPC-C ACID evidence. A deterministic bounded fixture now
loads and reopens all nine core table families (`warehouse`, `district`,
`customer`, `orders`, `new_order`, `order_line`, `item`, `stock`, `history`)
from a versioned seeded profile and verifies exact row counts. G2 still requires
the canonical full-column TPC-C schema and loader,
seeded workload receipts, and hosted exact-SHA execution. Throughput and tail
latency remain G7 responsibilities.
