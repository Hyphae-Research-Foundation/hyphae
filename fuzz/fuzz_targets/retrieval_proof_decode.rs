// SPDX-License-Identifier: Apache-2.0

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _exact = hyphae_engine::ExactRetrievalProof::from_bytes(data);
    let _lexical = hyphae_engine::LexicalRetrievalProof::from_bytes(data);
    let _hybrid = hyphae_engine::HybridRetrievalProof::from_bytes(data);
});
