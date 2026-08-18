// SPDX-License-Identifier: Apache-2.0

//! Generates the shared cross-language Native SDK request fixture.

use hyphae_native_product::{ProductDurabilityPolicy, ProductLimits, ProductOperation};
use hyphae_native_protocol::{FrameKind, WireRequest, encode_frame, encode_product_request};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let payload = encode_product_request(&WireRequest {
        operation: ProductOperation::StructureGet {
            key: b"shared-key".to_vec(),
        },
        logical_time_micros: 1_700_000_000_000_000,
        deadline_micros: Some(1_700_000_000_500_000),
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    })?;
    let fixture = encode_frame(
        FrameKind::Execute,
        7,
        42,
        &payload,
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../compatibility/native-protocol-v1-structure-get.bin");
    std::fs::write(path, fixture)?;
    Ok(())
}
