# hyphae-native-manifest

This unpublished crate owns immutable, digest-chained root manifests for native
checkpoints. A manifest binds one committed all-engine `RootSet`, its WAL
anchor, and its predecessor.

Publication uses create-new temporary files and same-directory rename. Unix
also synchronizes the directory. Safe Rust does not expose an equivalent
Windows directory flush, so physical power-loss durability on Windows remains
an explicit gate rather than an inferred guarantee.
