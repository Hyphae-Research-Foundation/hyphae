// SPDX-License-Identifier: GPL-3.0-only

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _result = hyphae_engine::decode_document(data);
});
