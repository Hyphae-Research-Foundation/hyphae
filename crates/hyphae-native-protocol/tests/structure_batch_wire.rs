// SPDX-License-Identifier: Apache-2.0

//! Current-minor structure batch receipt and legacy down-conversion coverage.

use hyphae_native_product::{
    ProductCommitOutcome, ProductCommitReceipt, ProductDurability, ProductResponse,
    ProductStructureMutationBatchReceipt, ProductStructureMutationOutcome,
    ProductStructureMutationResult, ProductTransactionId,
};
use hyphae_native_protocol::{
    ProductCodecError, decode_product_response_for_minor, encode_product_response,
    encode_product_response_for_minor,
};

#[test]
fn no_op_batch_round_trips_only_at_minor_seven() -> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::StructureMutationBatch(ProductStructureMutationBatchReceipt {
        read_csn: Some(7),
        commit: None,
        results: vec![ProductStructureMutationOutcome {
            changed: false,
            result: ProductStructureMutationResult::Boolean(false),
        }],
    });

    let encoded = encode_product_response_for_minor(&response, 7)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 46);
    assert_eq!(decode_product_response_for_minor(&encoded, 7)?, response);
    assert!(matches!(
        encode_product_response_for_minor(&response, 6),
        Err(ProductCodecError::Unsupported)
    ));
    for end in 0..encoded.len() {
        assert!(decode_product_response_for_minor(&encoded[..end], 7).is_err());
    }

    let mut reserved = encoded.clone();
    reserved[25] = 1;
    assert!(matches!(
        decode_product_response_for_minor(&reserved, 7),
        Err(ProductCodecError::Malformed)
    ));

    let mut zero = encoded.clone();
    zero[32..36].copy_from_slice(&0_u32.to_le_bytes());
    assert!(matches!(
        decode_product_response_for_minor(&zero, 7),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut excessive = encoded.clone();
    excessive[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode_product_response_for_minor(&excessive, 7),
        Err(ProductCodecError::LimitExceeded)
    ));

    let mut impossible = encoded.clone();
    impossible[32..36].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_product_response_for_minor(&impossible, 7),
        Err(ProductCodecError::Truncated)
    ));

    let mut inconsistent = encoded;
    inconsistent[40] = 1;
    assert!(matches!(
        decode_product_response_for_minor(&inconsistent, 7),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

#[test]
fn committed_batch_down_converts_to_the_frozen_legacy_response()
-> Result<(), Box<dyn std::error::Error>> {
    let commit = receipt()?;
    let response = ProductResponse::StructureMutationBatch(ProductStructureMutationBatchReceipt {
        read_csn: Some(7),
        commit: Some(commit),
        results: vec![
            ProductStructureMutationOutcome {
                changed: false,
                result: ProductStructureMutationResult::Boolean(false),
            },
            ProductStructureMutationOutcome {
                changed: true,
                result: ProductStructureMutationResult::Count(5),
            },
        ],
    });
    let current = encode_product_response_for_minor(&response, 7)?;
    assert_eq!(decode_product_response_for_minor(&current, 7)?, response);

    let legacy = ProductResponse::StructureMutated(ProductCommitOutcome::Committed(commit));
    assert_eq!(
        encode_product_response_for_minor(&response, 6)?,
        encode_product_response_for_minor(&legacy, 6)?
    );
    Ok(())
}

#[test]
fn inconsistent_batch_receipts_never_encode() -> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::StructureMutationBatch(ProductStructureMutationBatchReceipt {
        read_csn: None,
        commit: Some(receipt()?),
        results: vec![ProductStructureMutationOutcome {
            changed: false,
            result: ProductStructureMutationResult::Boolean(false),
        }],
    });
    assert!(matches!(
        encode_product_response(&response),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

fn receipt() -> Result<ProductCommitReceipt, Box<dyn std::error::Error>> {
    Ok(ProductCommitReceipt {
        transaction_id: ProductTransactionId::new(9).ok_or("transaction id")?,
        commit_csn: 8,
        catalog_version: 3,
        commit_lsn: 11,
        wal_block_digest: [4; 32],
        durability: ProductDurability::Strict,
        durability_cohort_size: 1,
        durability_cohort_position: 0,
    })
}
