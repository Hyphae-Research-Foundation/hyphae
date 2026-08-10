// SPDX-License-Identifier: GPL-3.0-only

//! Cross-language Native v2 SDK protocol fixtures.

use hyphae_client::v2::{ProductDurabilityPolicy, ProductLimits, ProductOperation};
use hyphae_native_protocol::{
    WireRequest, decode_frame, decode_product_request, encode_frame, encode_product_request,
};

#[test]
fn shared_binary_fixture_decodes_and_reencodes_exactly() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture()?;
    let frame = decode_frame(&fixture, hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
    assert_eq!(frame.stream_id, 7);
    assert_eq!(frame.request_id, 42);
    let request = decode_product_request(frame.payload)?;
    assert!(matches!(
        request.operation,
        ProductOperation::StructureGet { ref key } if key == b"shared-key"
    ));
    let encoded_payload = encode_product_request(&request)?;
    let encoded = encode_frame(
        frame.kind,
        frame.stream_id,
        frame.request_id,
        &encoded_payload,
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    assert_eq!(encoded, fixture);
    Ok(())
}

#[test]
fn independent_structure_get_encoder_matches_shared_fixture()
-> Result<(), Box<dyn std::error::Error>> {
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
    let frame = encode_frame(
        hyphae_native_protocol::FrameKind::Execute,
        7,
        42,
        &payload,
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    assert_eq!(frame, fixture()?);
    Ok(())
}

fn fixture() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let encoded = include_str!("fixtures/native-protocol-v1-structure-get.hex").trim();
    if !encoded.len().is_multiple_of(2) {
        return Err("fixture has an odd number of hexadecimal digits".into());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)?;
            Ok(u8::from_str_radix(text, 16)?)
        })
        .collect()
}
