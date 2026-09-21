// SPDX-License-Identifier: Apache-2.0

//! Generates the shared cross-language Native SDK request fixture.

use hyphae_native_product::{
    ObjectId, ProductDocValue, ProductDocument, ProductDurabilityPolicy, ProductLimits,
    ProductOperation, ProductTransactionHandle, ProductTransactionSearchMutation, ProductVector,
};
use hyphae_native_protocol::{
    FrameKind, WireRequest, encode_frame, encode_product_request, encode_product_request_for_minor,
};

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

    let payload = encode_product_request_for_minor(
        &WireRequest {
            operation: ProductOperation::TransactionStageSearch {
                handle: ProductTransactionHandle::new(7).ok_or("nonzero transaction handle")?,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: ObjectId::new(13)?,
                    document: ProductDocument {
                        object_id: ObjectId::new(201)?,
                        text: "rust database".to_owned(),
                        doc_values: [
                            ("blob".to_owned(), ProductDocValue::Bytes(vec![7])),
                            ("flag".to_owned(), ProductDocValue::Boolean(true)),
                            ("name".to_owned(), ProductDocValue::String("a".to_owned())),
                            ("rank".to_owned(), ProductDocValue::Integer(3)),
                            (
                                "rating".to_owned(),
                                ProductDocValue::Float(hyphae_native_product::CanonicalF64::new(
                                    4.5,
                                )),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                        vectors: [("embedding".to_owned(), ProductVector::new([1.0, 0.0])?)]
                            .into_iter()
                            .collect(),
                    },
                },
            },
            logical_time_micros: 10,
            deadline_micros: None,
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::MEMORY,
        },
        7,
    )?;
    let fixture = encode_frame(
        FrameKind::Execute,
        7,
        43,
        &payload,
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native-protocol-v1-transaction-document.bin");
    std::fs::write(path, fixture)?;
    Ok(())
}
