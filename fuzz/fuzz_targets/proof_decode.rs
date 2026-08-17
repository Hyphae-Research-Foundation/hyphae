// SPDX-License-Identifier: Apache-2.0

#![no_main]

use std::{path::PathBuf, sync::LazyLock};

use libfuzzer_sys::fuzz_target;

static INPUT: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::temp_dir().join(format!("hyphae-fuzz-proof-{}", std::process::id()))
});

fuzz_target!(|data: &[u8]| {
    if std::fs::write(INPUT.as_path(), data).is_ok() {
        let _result = hyphae_engine::read_result_proof(INPUT.as_path(), 1024 * 1024);
    }
});
