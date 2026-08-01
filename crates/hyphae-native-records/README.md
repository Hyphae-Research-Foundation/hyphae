# hyphae-native-records

This unpublished crate owns the canonical binary encodings for committed MVCC
rows, catalog-ordered typed tuples, row-version pointers, and immutable blob
references. Rows and `HYTUPL01` tuples carry no per-column type tags; the
catalog fixes column order and logical types. Both owned and borrowed tuple
decoders validate exact length, null bits, offsets, and trailing-byte
invariants.

It is a substrate crate, not a SQL executor or heap/index implementation.
