# hyphae-native-records

This unpublished crate owns the canonical binary encodings for committed MVCC
rows and immutable blob references. Rows carry no per-column type tags; the
catalog fixes column order and logical types.

It is a substrate crate, not a SQL executor or heap/index implementation.
