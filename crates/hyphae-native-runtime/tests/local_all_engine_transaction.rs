// SPDX-License-Identifier: Apache-2.0

//! Contract and integration tests for the explicit local all-engine transaction.

use std::num::NonZeroU64;

use hyphae_native_runtime::{
    LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE, LOCAL_TRANSACTION_BEGIN_SIZE,
    LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE, LOCAL_TRANSACTION_COMMIT_SIZE,
    LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE, LOCAL_TRANSACTION_ROLLBACK_SIZE,
    LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE, LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE,
    LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE, LOCAL_TRANSACTION_STRUCTURE_SET_HEADER_SIZE,
    LocalFailureCode, LocalOperationCodecError, LocalSearchCodecError, LocalSqlCodecError,
    LocalTransactionBeginReceipt, LocalTransactionCodecError, LocalTransactionCommitReceipt,
    LocalTransactionEngine, LocalTransactionIndexDocumentRequest, LocalTransactionRollbackReceipt,
    LocalTransactionStageReceipt, LocalTransactionStructureSetRequest,
    MAX_LOCAL_SEARCH_DOCUMENT_BYTES, MAX_LOCAL_SEARCH_DOCUMENT_ID_BYTES, MAX_LOCAL_SQL_PARAMETERS,
    MAX_LOCAL_SQL_STATEMENT_BYTES, MAX_LOCAL_STRUCTURE_KEY_BYTES, MAX_LOCAL_TRANSACTION_OPERATIONS,
    decode_local_failure, decode_local_transaction_begin, decode_local_transaction_begin_receipt,
    decode_local_transaction_commit, decode_local_transaction_commit_receipt,
    decode_local_transaction_index_document, decode_local_transaction_rollback,
    decode_local_transaction_rollback_receipt, decode_local_transaction_sql_dml,
    decode_local_transaction_stage_receipt, decode_local_transaction_structure_set,
    encode_local_failure, encode_local_transaction_begin, encode_local_transaction_begin_receipt,
    encode_local_transaction_commit, encode_local_transaction_commit_receipt,
    encode_local_transaction_index_document, encode_local_transaction_rollback,
    encode_local_transaction_rollback_receipt, encode_local_transaction_sql_dml,
    encode_local_transaction_stage_receipt, encode_local_transaction_structure_set,
};
use hyphae_native_types::{Csn, DurabilityClass, ObjectId, ScalarValue, TransactionId};

const _: () = {
    assert!(LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE == LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE);
    assert!(LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE > LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE);
    assert!(LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE > LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE);
};

#[test]
fn transaction_control_payloads_match_frozen_goldens() -> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(7).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();

    assert_eq!(
        encode_local_transaction_begin(&mut buffer, DurabilityClass::Strict)?,
        &[1, 1, 1, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        decode_local_transaction_begin(&buffer)?,
        DurabilityClass::Strict
    );

    let begun = LocalTransactionBeginReceipt {
        durability: DurabilityClass::Strict,
        handle,
        read_csn: Some(Csn::new(5)?),
        logical_time_micros: 11,
    };
    let mut expected_begun = [0_u8; 32];
    expected_begun[0] = 1;
    expected_begun[1] = 1;
    expected_begun[2] = 1;
    expected_begun[4..12].copy_from_slice(&7_u64.to_le_bytes());
    expected_begun[12..20].copy_from_slice(&5_u64.to_le_bytes());
    expected_begun[20..28].copy_from_slice(&11_i64.to_le_bytes());
    assert_eq!(
        encode_local_transaction_begin_receipt(&mut buffer, begun)?,
        expected_begun
    );
    assert_eq!(decode_local_transaction_begin_receipt(&buffer)?, begun);

    let commit = encode_local_transaction_commit(&mut buffer, handle, 3)?.to_vec();
    assert_eq!(decode_local_transaction_commit(&commit)?, (handle, 3));
    let committed = LocalTransactionCommitReceipt {
        durability: DurabilityClass::Strict,
        handle,
        transaction_id: TransactionId::new(23)?,
        commit_csn: Csn::new(17)?,
        staged_operations: 3,
    };
    assert_eq!(
        decode_local_transaction_commit_receipt(encode_local_transaction_commit_receipt(
            &mut buffer,
            committed
        )?)?,
        committed
    );

    let rollback = encode_local_transaction_rollback(&mut buffer, handle).to_vec();
    assert_eq!(decode_local_transaction_rollback(&rollback)?, handle);
    let rolled_back = LocalTransactionRollbackReceipt {
        handle,
        discarded_operations: 3,
    };
    assert_eq!(
        decode_local_transaction_rollback_receipt(encode_local_transaction_rollback_receipt(
            &mut buffer,
            rolled_back
        )?)?,
        rolled_back
    );
    Ok(())
}

#[test]
fn transaction_engine_payloads_round_trip_canonical_values()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(7).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let structure = LocalTransactionStructureSetRequest {
        handle,
        key: b"k",
        value: b"value",
        relative_ttl_micros: Some(9),
    };
    let encoded_structure =
        encode_local_transaction_structure_set(&mut buffer, structure, 128)?.to_vec();
    assert_eq!(
        decode_local_transaction_structure_set(&encoded_structure)?,
        structure
    );

    let sql = encode_local_transaction_sql_dml(
        &mut buffer,
        handle,
        "INSERT INTO events (id, body) VALUES (?, ?)",
        &[ScalarValue::Signed(1), ScalarValue::Text("one".to_owned())],
        256,
    )?
    .to_vec();
    let decoded_sql = decode_local_transaction_sql_dml(&sql)?;
    assert_eq!(decoded_sql.handle, handle);
    assert_eq!(
        decoded_sql.statement,
        "INSERT INTO events (id, body) VALUES (?, ?)"
    );
    assert_eq!(
        decoded_sql.parameters,
        vec![ScalarValue::Signed(1), ScalarValue::Text("one".to_owned())]
    );

    let index = ObjectId::new(19)?;
    let document = LocalTransactionIndexDocumentRequest {
        handle,
        index,
        document_id: b"doc-1",
        text: "native transaction",
    };
    let encoded_document =
        encode_local_transaction_index_document(&mut buffer, document, 128)?.to_vec();
    assert_eq!(
        decode_local_transaction_index_document(&encoded_document)?,
        document
    );

    let staged = LocalTransactionStageReceipt {
        engine: LocalTransactionEngine::Search,
        handle,
        operation_ordinal: 3,
        rows_affected: 1,
    };
    assert_eq!(
        decode_local_transaction_stage_receipt(encode_local_transaction_stage_receipt(
            &mut buffer,
            staged
        )?)?,
        staged
    );
    Ok(())
}

#[test]
fn transaction_commit_and_rollback_payloads_match_frozen_goldens()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(7).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let mut expected_commit = [0_u8; LOCAL_TRANSACTION_COMMIT_SIZE];
    expected_commit[0] = 1;
    expected_commit[1] = 1;
    expected_commit[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_commit[12..20].copy_from_slice(&3_u64.to_le_bytes());
    assert_eq!(
        encode_local_transaction_commit(&mut buffer, handle, 3)?,
        expected_commit
    );

    let committed = LocalTransactionCommitReceipt {
        durability: DurabilityClass::Strict,
        handle,
        transaction_id: TransactionId::new(23)?,
        commit_csn: Csn::new(17)?,
        staged_operations: 3,
    };
    let mut expected_committed = [0_u8; LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE];
    expected_committed[0] = 1;
    expected_committed[1] = 3;
    expected_committed[2] = 1;
    expected_committed[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_committed[12..28].copy_from_slice(&23_u128.to_le_bytes());
    expected_committed[28..36].copy_from_slice(&17_u64.to_le_bytes());
    expected_committed[36..40].copy_from_slice(&3_u32.to_le_bytes());
    assert_eq!(
        encode_local_transaction_commit_receipt(&mut buffer, committed)?,
        expected_committed
    );

    let mut expected_rollback = [0_u8; LOCAL_TRANSACTION_ROLLBACK_SIZE];
    expected_rollback[0] = 1;
    expected_rollback[1] = 1;
    expected_rollback[4..12].copy_from_slice(&handle.get().to_le_bytes());
    assert_eq!(
        encode_local_transaction_rollback(&mut buffer, handle),
        expected_rollback
    );
    let rolled_back = LocalTransactionRollbackReceipt {
        handle,
        discarded_operations: 3,
    };
    let mut expected_rolled_back = [0_u8; LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE];
    expected_rolled_back[0] = 1;
    expected_rolled_back[1] = 4;
    expected_rolled_back[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_rolled_back[12..20].copy_from_slice(&3_u64.to_le_bytes());
    assert_eq!(
        encode_local_transaction_rollback_receipt(&mut buffer, rolled_back)?,
        expected_rolled_back
    );
    Ok(())
}

#[test]
fn transaction_engine_payloads_match_frozen_goldens() -> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(7).ok_or("nonzero handle")?;
    let index = ObjectId::new(19)?;
    let mut buffer = Vec::new();
    let structure = LocalTransactionStructureSetRequest {
        handle,
        key: b"k",
        value: b"v",
        relative_ttl_micros: Some(9),
    };
    let mut expected_structure = vec![0_u8; LOCAL_TRANSACTION_STRUCTURE_SET_HEADER_SIZE + 2];
    expected_structure[0] = 1;
    expected_structure[1] = 4;
    expected_structure[2] = 1;
    expected_structure[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_structure[12..16].copy_from_slice(&1_u32.to_le_bytes());
    expected_structure[16..20].copy_from_slice(&1_u32.to_le_bytes());
    expected_structure[20..28].copy_from_slice(&9_i64.to_le_bytes());
    expected_structure[32..].copy_from_slice(b"kv");
    assert_eq!(
        encode_local_transaction_structure_set(&mut buffer, structure, usize::MAX)?,
        expected_structure
    );

    let statement = "DELETE FROM events WHERE id = 1";
    let mut expected_sql = vec![0_u8; LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE + statement.len()];
    expected_sql[0] = 1;
    expected_sql[1] = 2;
    expected_sql[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_sql[12..16].copy_from_slice(&u32::try_from(statement.len())?.to_le_bytes());
    expected_sql[24..].copy_from_slice(statement.as_bytes());
    assert_eq!(
        encode_local_transaction_sql_dml(&mut buffer, handle, statement, &[], usize::MAX)?,
        expected_sql
    );

    let document = LocalTransactionIndexDocumentRequest {
        handle,
        index,
        document_id: b"d",
        text: "x",
    };
    let mut expected_document = vec![0_u8; LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE + 2];
    expected_document[0] = 1;
    expected_document[1] = 2;
    expected_document[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_document[12..28].copy_from_slice(&index.get().to_le_bytes());
    expected_document[28..32].copy_from_slice(&1_u32.to_le_bytes());
    expected_document[32..36].copy_from_slice(&1_u32.to_le_bytes());
    expected_document[40..].copy_from_slice(b"dx");
    assert_eq!(
        encode_local_transaction_index_document(&mut buffer, document, usize::MAX)?,
        expected_document
    );

    let staged = LocalTransactionStageReceipt {
        engine: LocalTransactionEngine::Search,
        handle,
        operation_ordinal: 3,
        rows_affected: 1,
    };
    let mut expected_staged = [0_u8; LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE];
    expected_staged[0] = 1;
    expected_staged[1] = 2;
    expected_staged[2] = 3;
    expected_staged[4..12].copy_from_slice(&handle.get().to_le_bytes());
    expected_staged[12..20].copy_from_slice(&3_u64.to_le_bytes());
    expected_staged[20..28].copy_from_slice(&1_u64.to_le_bytes());
    assert_eq!(
        encode_local_transaction_stage_receipt(&mut buffer, staged)?,
        expected_staged
    );
    Ok(())
}

#[test]
fn transaction_control_codecs_reject_noncanonical_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let begin_payload =
        encode_local_transaction_begin(&mut buffer, DurabilityClass::Memory)?.to_vec();
    for length in 0..LOCAL_TRANSACTION_BEGIN_SIZE {
        assert!(matches!(
            decode_local_transaction_begin(&begin_payload[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    let mut invalid = begin_payload.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_begin(&invalid),
        Err(LocalTransactionCodecError::LengthMismatch)
    ));
    for (offset, value, expected) in [
        (0, 2, LocalTransactionCodecError::UnsupportedVersion(2)),
        (1, 2, LocalTransactionCodecError::UnknownOpcode(2)),
        (3, 1, LocalTransactionCodecError::ReservedBytes),
        (2, 2, LocalTransactionCodecError::UnsupportedDurability(2)),
        (2, 9, LocalTransactionCodecError::UnknownDurability(9)),
    ] {
        invalid = begin_payload.clone();
        invalid[offset] = value;
        assert_eq!(decode_local_transaction_begin(&invalid), Err(expected));
    }
    let begin_receipt = LocalTransactionBeginReceipt {
        durability: DurabilityClass::Memory,
        handle,
        read_csn: None,
        logical_time_micros: 10,
    };
    let encoded_receipt =
        encode_local_transaction_begin_receipt(&mut buffer, begin_receipt)?.to_vec();
    for length in 0..LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_transaction_begin_receipt(&encoded_receipt[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }

    assert_eq!(
        decode_local_transaction_commit(encode_local_transaction_commit(
            &mut buffer,
            handle,
            u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS)?,
        )?)?
        .1,
        u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS)?
    );
    assert!(matches!(
        encode_local_transaction_commit(
            &mut buffer,
            handle,
            u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS + 1)?,
        ),
        Err(LocalTransactionCodecError::InvalidOperationCount)
    ));
    let mut empty_commit = vec![0_u8; LOCAL_TRANSACTION_COMMIT_SIZE];
    empty_commit[0] = 1;
    empty_commit[1] = 1;
    empty_commit[4..12].copy_from_slice(&handle.get().to_le_bytes());
    assert_eq!(decode_local_transaction_commit(&empty_commit)?, (handle, 0));
    let rollback = encode_local_transaction_rollback(&mut buffer, handle).to_vec();
    for length in 0..LOCAL_TRANSACTION_ROLLBACK_SIZE {
        assert!(matches!(
            decode_local_transaction_rollback(&rollback[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }

    for (code, byte) in [
        (LocalFailureCode::TransactionActive, 13),
        (LocalFailureCode::TransactionInactive, 14),
        (LocalFailureCode::TransactionMismatch, 15),
        (LocalFailureCode::TransactionEmpty, 16),
        (LocalFailureCode::TransactionResourceLimit, 17),
        (LocalFailureCode::TransactionConflict, 18),
    ] {
        assert_eq!(encode_local_failure(&mut buffer, code), [1, byte, 0, 0]);
        assert_eq!(decode_local_failure(&buffer)?, code);
    }
    Ok(())
}

#[test]
fn transaction_commit_and_rollback_codecs_reject_noncanonical_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let commit = encode_local_transaction_commit(&mut buffer, handle, 3)?.to_vec();
    for length in 0..LOCAL_TRANSACTION_COMMIT_SIZE {
        assert!(matches!(
            decode_local_transaction_commit(&commit[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    let mut invalid = commit.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_commit(&invalid),
        Err(LocalTransactionCodecError::LengthMismatch)
    ));
    for range in [2..4, 4..12, 20..24] {
        invalid = commit.clone();
        if range == (4..12) {
            invalid[range].fill(0);
            assert!(matches!(
                decode_local_transaction_commit(&invalid),
                Err(LocalTransactionCodecError::InvalidIdentity)
            ));
        } else {
            invalid[range.start] = 1;
            assert!(matches!(
                decode_local_transaction_commit(&invalid),
                Err(LocalTransactionCodecError::ReservedBytes)
            ));
        }
    }

    let rollback = encode_local_transaction_rollback(&mut buffer, handle).to_vec();
    for length in 0..LOCAL_TRANSACTION_ROLLBACK_SIZE {
        assert!(matches!(
            decode_local_transaction_rollback(&rollback[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    invalid = rollback.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_rollback(&invalid),
        Err(LocalTransactionCodecError::LengthMismatch)
    ));
    invalid = rollback.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_transaction_rollback(&invalid),
        Err(LocalTransactionCodecError::InvalidIdentity)
    ));
    for offset in [2, 12] {
        invalid = rollback.clone();
        invalid[offset] = 1;
        assert!(matches!(
            decode_local_transaction_rollback(&invalid),
            Err(LocalTransactionCodecError::ReservedBytes)
        ));
    }
    Ok(())
}

#[test]
fn transaction_begin_and_stage_receipts_reject_noncanonical_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let begun = encode_local_transaction_begin_receipt(
        &mut buffer,
        LocalTransactionBeginReceipt {
            durability: DurabilityClass::Memory,
            handle,
            read_csn: Some(Csn::new(1)?),
            logical_time_micros: 10,
        },
    )?
    .to_vec();
    for length in 0..LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_transaction_begin_receipt(&begun[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    let mut invalid = begun.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_begin_receipt(&invalid),
        Err(LocalTransactionCodecError::LengthMismatch)
    ));
    invalid = begun.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_transaction_begin_receipt(&invalid),
        Err(LocalTransactionCodecError::InvalidIdentity)
    ));
    invalid = begun.clone();
    invalid[12..20].fill(0);
    assert_eq!(
        decode_local_transaction_begin_receipt(&invalid)?.read_csn,
        None
    );

    let staged = encode_local_transaction_stage_receipt(
        &mut buffer,
        LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Search,
            handle,
            operation_ordinal: 1,
            rows_affected: 1,
        },
    )?
    .to_vec();
    for length in 0..LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_transaction_stage_receipt(&staged[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    invalid = staged.clone();
    invalid[2] = 9;
    assert!(matches!(
        decode_local_transaction_stage_receipt(&invalid),
        Err(LocalTransactionCodecError::UnknownEngine(9))
    ));
    for range in [4..12, 12..20] {
        invalid = staged.clone();
        invalid[range].fill(0);
        assert!(matches!(
            decode_local_transaction_stage_receipt(&invalid),
            Err(LocalTransactionCodecError::InvalidIdentity
                | LocalTransactionCodecError::InvalidOperationCount)
        ));
    }
    Ok(())
}

#[test]
fn transaction_commit_and_rollback_receipts_reject_noncanonical_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let committed = encode_local_transaction_commit_receipt(
        &mut buffer,
        LocalTransactionCommitReceipt {
            durability: DurabilityClass::Strict,
            handle,
            transaction_id: TransactionId::new(1)?,
            commit_csn: Csn::new(1)?,
            staged_operations: 1,
        },
    )?
    .to_vec();
    for length in 0..LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_transaction_commit_receipt(&committed[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    let mut invalid = committed.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_commit_receipt(&invalid),
        Err(LocalTransactionCodecError::LengthMismatch)
    ));
    for range in [4..12, 12..28, 28..36, 36..40] {
        invalid = committed.clone();
        invalid[range].fill(0);
        assert!(matches!(
            decode_local_transaction_commit_receipt(&invalid),
            Err(LocalTransactionCodecError::InvalidIdentity
                | LocalTransactionCodecError::InvalidOperationCount)
        ));
    }

    let rolled_back = encode_local_transaction_rollback_receipt(
        &mut buffer,
        LocalTransactionRollbackReceipt {
            handle,
            discarded_operations: 1,
        },
    )?
    .to_vec();
    for length in 0..LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_transaction_rollback_receipt(&rolled_back[..length]),
            Err(LocalTransactionCodecError::Truncated)
        ));
    }
    invalid = rolled_back.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_transaction_rollback_receipt(&invalid),
        Err(LocalTransactionCodecError::InvalidIdentity)
    ));
    invalid = rolled_back;
    invalid[12..20]
        .copy_from_slice(&u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_transaction_rollback_receipt(&invalid),
        Err(LocalTransactionCodecError::InvalidOperationCount)
    ));
    Ok(())
}

#[test]
fn transaction_structure_and_search_codecs_enforce_physical_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let index = ObjectId::new(1)?;
    let mut buffer = Vec::new();
    let exact_key = vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES];
    let structure = LocalTransactionStructureSetRequest {
        handle,
        key: &exact_key,
        value: b"",
        relative_ttl_micros: None,
    };
    assert_eq!(
        decode_local_transaction_structure_set(encode_local_transaction_structure_set(
            &mut buffer,
            structure,
            usize::MAX,
        )?)?
        .key,
        exact_key
    );
    let too_large_key = vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES + 1];
    assert!(matches!(
        encode_local_transaction_structure_set(
            &mut buffer,
            LocalTransactionStructureSetRequest {
                handle,
                key: &too_large_key,
                value: b"",
                relative_ttl_micros: None,
            },
            usize::MAX,
        ),
        Err(LocalOperationCodecError::KeyTooLarge)
    ));

    let exact_document_id = vec![b'd'; MAX_LOCAL_SEARCH_DOCUMENT_ID_BYTES];
    let exact_text = "t".repeat(MAX_LOCAL_SEARCH_DOCUMENT_BYTES);
    let document = LocalTransactionIndexDocumentRequest {
        handle,
        index,
        document_id: &exact_document_id,
        text: &exact_text,
    };
    assert_eq!(
        decode_local_transaction_index_document(encode_local_transaction_index_document(
            &mut buffer,
            document,
            usize::MAX,
        )?)?,
        document
    );
    assert!(matches!(
        encode_local_transaction_index_document(
            &mut buffer,
            LocalTransactionIndexDocumentRequest {
                handle,
                index,
                document_id: &vec![b'd'; MAX_LOCAL_SEARCH_DOCUMENT_ID_BYTES + 1],
                text: "",
            },
            usize::MAX,
        ),
        Err(LocalSearchCodecError::DocumentIdTooLarge)
    ));
    assert!(matches!(
        encode_local_transaction_index_document(
            &mut buffer,
            LocalTransactionIndexDocumentRequest {
                handle,
                index,
                document_id: b"d",
                text: &"t".repeat(MAX_LOCAL_SEARCH_DOCUMENT_BYTES + 1),
            },
            usize::MAX,
        ),
        Err(LocalSearchCodecError::DocumentTooLarge)
    ));

    Ok(())
}

#[test]
fn transaction_structure_codec_rejects_noncanonical_payloads()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let canonical = encode_local_transaction_structure_set(
        &mut buffer,
        LocalTransactionStructureSetRequest {
            handle,
            key: b"k",
            value: b"v",
            relative_ttl_micros: Some(9),
        },
        usize::MAX,
    )?
    .to_vec();
    for length in 0..canonical.len() {
        assert!(matches!(
            decode_local_transaction_structure_set(&canonical[..length]),
            Err(LocalOperationCodecError::Truncated | LocalOperationCodecError::LengthMismatch)
        ));
    }
    let mut invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_structure_set(&invalid),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    for (offset, value) in [(0, 2), (1, 5)] {
        invalid = canonical.clone();
        invalid[offset] = value;
        assert!(matches!(
            decode_local_transaction_structure_set(&invalid),
            Err(LocalOperationCodecError::UnsupportedVersion(2)
                | LocalOperationCodecError::UnknownStructureOpcode(5))
        ));
    }
    for offset in [3, 28] {
        invalid = canonical.clone();
        invalid[offset] = 1;
        assert!(matches!(
            decode_local_transaction_structure_set(&invalid),
            Err(LocalOperationCodecError::ReservedBytes)
        ));
    }
    invalid = canonical.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_transaction_structure_set(&invalid),
        Err(LocalOperationCodecError::InvalidIdentity)
    ));
    invalid = canonical.clone();
    invalid[2] = 2;
    assert!(matches!(
        decode_local_transaction_structure_set(&invalid),
        Err(LocalOperationCodecError::UnknownExpiryMode(2))
    ));
    invalid = canonical;
    invalid[12..16].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_local_transaction_structure_set(&invalid),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn transaction_search_codec_rejects_noncanonical_payloads() -> Result<(), Box<dyn std::error::Error>>
{
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let index = ObjectId::new(1)?;
    let mut buffer = Vec::new();
    let canonical = encode_local_transaction_index_document(
        &mut buffer,
        LocalTransactionIndexDocumentRequest {
            handle,
            index,
            document_id: b"d",
            text: "λ",
        },
        usize::MAX,
    )?
    .to_vec();
    for length in 0..canonical.len() {
        assert!(matches!(
            decode_local_transaction_index_document(&canonical[..length]),
            Err(LocalSearchCodecError::Truncated | LocalSearchCodecError::LengthMismatch)
        ));
    }
    let mut invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_index_document(&invalid),
        Err(LocalSearchCodecError::LengthMismatch)
    ));
    for offset in [2, 36] {
        invalid = canonical.clone();
        invalid[offset] = 1;
        assert!(matches!(
            decode_local_transaction_index_document(&invalid),
            Err(LocalSearchCodecError::ReservedBytes)
        ));
    }
    for range in [4..12, 12..28] {
        invalid = canonical.clone();
        invalid[range].fill(0);
        assert!(matches!(
            decode_local_transaction_index_document(&invalid),
            Err(LocalSearchCodecError::InvalidTransactionHandle
                | LocalSearchCodecError::InvalidObjectId)
        ));
    }
    invalid = canonical.clone();
    invalid[32..36].copy_from_slice(&1_u32.to_le_bytes());
    invalid.truncate(LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE + 2);
    invalid[LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE + 1] = 0xff;
    assert!(matches!(
        decode_local_transaction_index_document(&invalid),
        Err(LocalSearchCodecError::InvalidUtf8)
    ));
    invalid = canonical;
    invalid[28..32].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_local_transaction_index_document(&invalid),
        Err(LocalSearchCodecError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn transaction_sql_codec_enforces_statement_and_parameter_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let mut buffer = Vec::new();
    let exact_statement = "x".repeat(MAX_LOCAL_SQL_STATEMENT_BYTES);
    let exact_parameters = vec![ScalarValue::Null; MAX_LOCAL_SQL_PARAMETERS];
    let encoded = encode_local_transaction_sql_dml(
        &mut buffer,
        handle,
        &exact_statement,
        &exact_parameters,
        usize::MAX,
    )?;
    let decoded = decode_local_transaction_sql_dml(encoded)?;
    assert_eq!(decoded.statement.len(), MAX_LOCAL_SQL_STATEMENT_BYTES);
    assert_eq!(decoded.parameters.len(), MAX_LOCAL_SQL_PARAMETERS);
    assert!(matches!(
        encode_local_transaction_sql_dml(
            &mut buffer,
            handle,
            &"x".repeat(MAX_LOCAL_SQL_STATEMENT_BYTES + 1),
            &[],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::StatementTooLarge)
    ));
    assert!(matches!(
        encode_local_transaction_sql_dml(
            &mut buffer,
            handle,
            "INSERT INTO t (id) VALUES (?)",
            &vec![ScalarValue::Null; MAX_LOCAL_SQL_PARAMETERS + 1],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::ParameterCountExceeded)
    ));
    Ok(())
}

#[test]
fn transaction_sql_codec_rejects_noncanonical_payloads() -> Result<(), Box<dyn std::error::Error>> {
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let statement = "DELETE FROM events WHERE id = ?";
    let mut buffer = Vec::new();
    let canonical = encode_local_transaction_sql_dml(
        &mut buffer,
        handle,
        statement,
        &[ScalarValue::Signed(1)],
        usize::MAX,
    )?
    .to_vec();
    for length in 0..canonical.len() {
        assert!(matches!(
            decode_local_transaction_sql_dml(&canonical[..length]),
            Err(LocalSqlCodecError::Truncated
                | LocalSqlCodecError::LengthMismatch
                | LocalSqlCodecError::InvalidScalar)
        ));
    }
    let mut invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_transaction_sql_dml(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));
    for offset in [2, 20] {
        invalid = canonical.clone();
        invalid[offset] = 1;
        assert!(matches!(
            decode_local_transaction_sql_dml(&invalid),
            Err(LocalSqlCodecError::ReservedBytes)
        ));
    }
    invalid = canonical.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_transaction_sql_dml(&invalid),
        Err(LocalSqlCodecError::InvalidIdentity)
    ));
    invalid = canonical.clone();
    invalid[12..16].fill(0);
    assert!(matches!(
        decode_local_transaction_sql_dml(&invalid),
        Err(LocalSqlCodecError::EmptyStatement)
    ));
    invalid = canonical.clone();
    invalid[LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE] = 0xff;
    assert!(matches!(
        decode_local_transaction_sql_dml(&invalid),
        Err(LocalSqlCodecError::InvalidUtf8)
    ));
    invalid = canonical;
    let scalar_offset = LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE + statement.len();
    invalid[scalar_offset] = 0xff;
    assert!(matches!(
        decode_local_transaction_sql_dml(&invalid),
        Err(LocalSqlCodecError::UnknownScalarTag(0xff))
    ));
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        num::NonZeroU64,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        CommitBoundary, FrameKind, LocalDataSession, LocalFailureCode, LocalSessionError,
        LocalTransactionBeginReceipt, LocalTransactionCommitReceipt,
        LocalTransactionDeleteDocumentRequest, LocalTransactionEngine,
        LocalTransactionIndexDocumentRequest, LocalTransactionReplaceDocumentRequest,
        LocalTransactionRollbackReceipt, LocalTransactionStageReceipt,
        LocalTransactionStructureSetRequest, MAX_LOCAL_TRANSACTION_OPERATIONS, NativeDatabase,
        NativeRuntimeError, NativeSchedulerClock, NativeSnapshot, NativeWriteBatch, SqlResult, Ttl,
        UdsFrameConnection, UdsFrameListener, decode_local_failure,
        decode_local_transaction_begin_receipt, decode_local_transaction_commit_receipt,
        decode_local_transaction_rollback_receipt, decode_local_transaction_stage_receipt,
        encode_local_search_match, encode_local_sql_prepare, encode_local_structure_get,
        encode_local_transaction_begin, encode_local_transaction_commit,
        encode_local_transaction_delete_document, encode_local_transaction_index_document,
        encode_local_transaction_replace_document, encode_local_transaction_rollback,
        encode_local_transaction_sql_dml, encode_local_transaction_structure_set,
    };
    use hyphae_native_types::{Csn, DurabilityClass, ObjectId, ScalarValue, TransactionId};

    const MAXIMUM_PAYLOAD: usize = 512;
    const STREAM_ID: u32 = 17;
    const INDEX_ID: u128 = 100;
    const SELECT_EVENT: &str = "SELECT id, body FROM events WHERE id = ?";

    type TestError = Box<dyn std::error::Error>;
    type ServerError = Box<dyn std::error::Error + Send + Sync>;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, TestError> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = Path::new("/tmp")
                .join(format!("hy-transaction-{}-{timestamp}", std::process::id()));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    struct CountingClock(AtomicUsize);

    impl CountingClock {
        fn samples(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    impl NativeSchedulerClock for CountingClock {
        fn logical_time_micros(&self) -> i64 {
            self.0.fetch_add(1, Ordering::Relaxed);
            100
        }
    }

    struct SeededDatabase {
        database: NativeDatabase,
        prior: NativeSnapshot,
        index: ObjectId,
        seed_transaction_id: TransactionId,
        seed_csn: Csn,
    }

    fn seed_database(path: &Path) -> Result<SeededDatabase, TestError> {
        let mut database = NativeDatabase::create(path)?;
        let index = ObjectId::new(INDEX_ID)?;
        let mut seed = database.begin(90, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE events (
                id BIGINT PRIMARY KEY,
                body TEXT NOT NULL
            )",
            &[],
        )?;
        seed.create_search_index(index, "documents")?;
        let receipt = seed.commit()?;
        let prior = database.snapshot(100)?;
        Ok(SeededDatabase {
            database,
            prior,
            index,
            seed_transaction_id: receipt.transaction_id,
            seed_csn: receipt.commit_csn,
        })
    }

    #[derive(Debug, Eq, PartialEq)]
    struct EngineObservation {
        rows: Vec<Vec<ScalarValue>>,
        structure: Option<Vec<u8>>,
        documents: Vec<Vec<u8>>,
    }

    fn observe(
        snapshot: &NativeSnapshot,
        index: ObjectId,
        id: i64,
        key: &[u8],
        query: &str,
    ) -> Result<EngineObservation, TestError> {
        let prepared = snapshot.prepare_sql(SELECT_EVENT)?;
        let SqlResult::Rows { rows, .. } =
            snapshot.execute_prepared(&prepared, &[ScalarValue::Signed(id)])?
        else {
            return Err("SELECT did not return rows".into());
        };
        let documents = snapshot
            .match_text(index, query, 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect();
        Ok(EngineObservation {
            rows,
            structure: snapshot.get(key).map(<[u8]>::to_vec),
            documents,
        })
    }

    fn expected_present(id: i64, body: &str, value: &[u8], document: &[u8]) -> EngineObservation {
        EngineObservation {
            rows: vec![vec![
                ScalarValue::Signed(id),
                ScalarValue::Text(body.to_owned()),
            ]],
            structure: Some(value.to_vec()),
            documents: vec![document.to_vec()],
        }
    }

    fn expected_absent() -> EngineObservation {
        EngineObservation {
            rows: Vec::new(),
            structure: None,
            documents: Vec::new(),
        }
    }

    fn stage_all_engines(
        batch: &mut NativeWriteBatch,
        index: ObjectId,
        id: i64,
        body: &str,
        key: &[u8],
        value: &[u8],
        document: &[u8],
    ) -> Result<(), TestError> {
        assert_eq!(
            batch.execute_sql_dml(
                "INSERT INTO events (id, body) VALUES (?, ?)",
                &[ScalarValue::Signed(id), ScalarValue::Text(body.to_owned()),],
            )?,
            SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            }
        );
        batch.set(key.to_vec(), value.to_vec(), None)?;
        batch.index_document(index, document.to_vec(), body.to_owned())?;
        Ok(())
    }

    fn spawn_server(
        database: NativeDatabase,
        socket: &Path,
        clock: Arc<CountingClock>,
    ) -> Result<thread::JoinHandle<Result<(), ServerError>>, TestError> {
        spawn_server_with_payload(database, socket, clock, MAXIMUM_PAYLOAD)
    }

    fn spawn_server_with_payload(
        database: NativeDatabase,
        socket: &Path,
        clock: Arc<CountingClock>,
        maximum_payload: usize,
    ) -> Result<thread::JoinHandle<Result<(), ServerError>>, TestError> {
        let listener = UdsFrameListener::bind(socket, maximum_payload)?;
        Ok(thread::spawn(move || {
            let mut database = database;
            let mut connection = listener.accept()?;
            LocalDataSession::new(&mut database, clock.as_ref()).serve(&mut connection)?;
            listener.close()?;
            Ok(())
        }))
    }

    fn join_server(server: thread::JoinHandle<Result<(), ServerError>>) -> Result<(), TestError> {
        server
            .join()
            .map_err(|_| std::io::Error::other("transaction server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(())
    }

    struct SessionClient {
        connection: UdsFrameConnection,
        buffer: Vec<u8>,
        next_request_id: u64,
    }

    impl SessionClient {
        fn connect(socket: &Path) -> Result<Self, TestError> {
            Self::connect_with_payload(socket, MAXIMUM_PAYLOAD)
        }

        fn connect_with_payload(socket: &Path, maximum_payload: usize) -> Result<Self, TestError> {
            let mut connection = UdsFrameConnection::connect(socket, maximum_payload)?;
            connection.send(FrameKind::Hello, 0, 1, b"")?;
            let welcome = connection
                .receive()?
                .ok_or("server closed during handshake")?;
            if welcome.kind != FrameKind::Welcome
                || welcome.stream_id != 0
                || welcome.request_id != 1
                || !welcome.payload.is_empty()
            {
                return Err("transaction handshake diverged".into());
            }
            Ok(Self {
                connection,
                buffer: Vec::new(),
                next_request_id: 2,
            })
        }

        fn exchange(
            &mut self,
            request_kind: FrameKind,
            response_kind: FrameKind,
            payload: &[u8],
        ) -> Result<Vec<u8>, TestError> {
            let request_id = self.next_request_id;
            self.next_request_id = request_id.checked_add(1).ok_or("request ID exhausted")?;
            self.connection
                .send(request_kind, STREAM_ID, request_id, payload)?;
            let response = self.connection.receive()?.ok_or("server closed early")?;
            if response.kind != response_kind
                || response.stream_id != STREAM_ID
                || response.request_id != request_id
            {
                return Err("transaction response identity diverged".into());
            }
            Ok(response.payload.to_vec())
        }

        fn expect_failure(
            &mut self,
            kind: FrameKind,
            payload: &[u8],
            expected: LocalFailureCode,
        ) -> Result<(), TestError> {
            let response = self.exchange(kind, FrameKind::Failure, payload)?;
            assert_eq!(decode_local_failure(&response)?, expected);
            Ok(())
        }

        fn begin(
            &mut self,
            durability: DurabilityClass,
        ) -> Result<LocalTransactionBeginReceipt, TestError> {
            let payload = encode_local_transaction_begin(&mut self.buffer, durability)?.to_vec();
            let response = self.exchange(FrameKind::Begin, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_begin_receipt(&response)?)
        }

        fn stage_sql(
            &mut self,
            handle: NonZeroU64,
            id: i64,
            body: &str,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let payload = encode_local_transaction_sql_dml(
                &mut self.buffer,
                handle,
                "INSERT INTO events (id, body) VALUES (?, ?)",
                &[ScalarValue::Signed(id), ScalarValue::Text(body.to_owned())],
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Execute, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_structure(
            &mut self,
            handle: NonZeroU64,
            key: &[u8],
            value: &[u8],
            ttl: Option<i64>,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let payload = encode_local_transaction_structure_set(
                &mut self.buffer,
                LocalTransactionStructureSetRequest {
                    handle,
                    key,
                    value,
                    relative_ttl_micros: ttl,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Structure, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_search(
            &mut self,
            handle: NonZeroU64,
            index: ObjectId,
            document_id: &[u8],
            text: &str,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let payload = encode_local_transaction_index_document(
                &mut self.buffer,
                LocalTransactionIndexDocumentRequest {
                    handle,
                    index,
                    document_id,
                    text,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Search, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_search_replace(
            &mut self,
            handle: NonZeroU64,
            index: ObjectId,
            document_id: &[u8],
            text: &str,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let payload = encode_local_transaction_replace_document(
                &mut self.buffer,
                LocalTransactionReplaceDocumentRequest {
                    handle,
                    index,
                    document_id,
                    text,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Search, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_search_delete(
            &mut self,
            handle: NonZeroU64,
            index: ObjectId,
            document_id: &[u8],
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let payload = encode_local_transaction_delete_document(
                &mut self.buffer,
                LocalTransactionDeleteDocumentRequest {
                    handle,
                    index,
                    document_id,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Search, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn commit(
            &mut self,
            handle: NonZeroU64,
            expected_operations: u64,
        ) -> Result<LocalTransactionCommitReceipt, TestError> {
            let payload =
                encode_local_transaction_commit(&mut self.buffer, handle, expected_operations)?
                    .to_vec();
            let response = self.exchange(FrameKind::Commit, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_commit_receipt(&response)?)
        }

        fn rollback(
            &mut self,
            handle: NonZeroU64,
        ) -> Result<LocalTransactionRollbackReceipt, TestError> {
            let payload = encode_local_transaction_rollback(&mut self.buffer, handle).to_vec();
            let response = self.exchange(FrameKind::Rollback, FrameKind::Receipt, &payload)?;
            Ok(decode_local_transaction_rollback_receipt(&response)?)
        }

        fn close(mut self) -> Result<(), TestError> {
            let response = self.exchange(FrameKind::Close, FrameKind::Close, b"")?;
            assert!(response.is_empty());
            Ok(())
        }
    }

    fn assert_inactive_and_begin_failures(client: &mut SessionClient) -> Result<(), TestError> {
        let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
        let commit = encode_local_transaction_commit(&mut client.buffer, handle, 1)?.to_vec();
        client.expect_failure(
            FrameKind::Commit,
            &commit,
            LocalFailureCode::TransactionInactive,
        )?;
        let rollback = encode_local_transaction_rollback(&mut client.buffer, handle).to_vec();
        client.expect_failure(
            FrameKind::Rollback,
            &rollback,
            LocalFailureCode::TransactionInactive,
        )?;
        let unsupported_group = [1, 1, DurabilityClass::Group as u8, 0, 0, 0, 0, 0];
        client.expect_failure(
            FrameKind::Begin,
            &unsupported_group,
            LocalFailureCode::UnsupportedDurability,
        )
    }

    fn assert_active_session_guards(
        client: &mut SessionClient,
        handle: NonZeroU64,
        index: ObjectId,
    ) -> Result<(), TestError> {
        let duplicate =
            encode_local_transaction_begin(&mut client.buffer, DurabilityClass::Memory)?.to_vec();
        client.expect_failure(
            FrameKind::Begin,
            &duplicate,
            LocalFailureCode::TransactionActive,
        )?;
        let get = encode_local_structure_get(&mut client.buffer, b"joint-key")?.to_vec();
        client.expect_failure(
            FrameKind::Structure,
            &get,
            LocalFailureCode::TransactionActive,
        )?;
        let search =
            encode_local_search_match(&mut client.buffer, index, "needle", 10, MAXIMUM_PAYLOAD)?
                .to_vec();
        client.expect_failure(
            FrameKind::Search,
            &search,
            LocalFailureCode::TransactionActive,
        )?;
        let prepare =
            encode_local_sql_prepare(&mut client.buffer, SELECT_EVENT, MAXIMUM_PAYLOAD)?.to_vec();
        client.expect_failure(
            FrameKind::Prepare,
            &prepare,
            LocalFailureCode::TransactionActive,
        )?;
        client.expect_failure(
            FrameKind::Structure,
            &[1, 4],
            LocalFailureCode::InvalidRequest,
        )?;
        let wrong_handle = NonZeroU64::new(handle.get().checked_add(1).ok_or("handle overflow")?)
            .ok_or("nonzero handle")?;
        let wrong = encode_local_transaction_structure_set(
            &mut client.buffer,
            LocalTransactionStructureSetRequest {
                handle: wrong_handle,
                key: b"joint-key",
                value: b"value",
                relative_ttl_micros: None,
            },
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        client.expect_failure(
            FrameKind::Structure,
            &wrong,
            LocalFailureCode::TransactionMismatch,
        )
    }

    fn assert_empty_commit_and_read_only_sql_fail(
        client: &mut SessionClient,
        handle: NonZeroU64,
    ) -> Result<(), TestError> {
        let empty_commit = encode_local_transaction_commit(&mut client.buffer, handle, 1)?.to_vec();
        client.expect_failure(
            FrameKind::Commit,
            &empty_commit,
            LocalFailureCode::TransactionEmpty,
        )?;
        let select = encode_local_transaction_sql_dml(
            &mut client.buffer,
            handle,
            SELECT_EVENT,
            &[ScalarValue::Signed(1)],
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        client.expect_failure(FrameKind::Execute, &select, LocalFailureCode::SqlInvalid)
    }

    fn stage_joint_transaction(
        client: &mut SessionClient,
        handle: NonZeroU64,
        index: ObjectId,
        id: i64,
        body: &str,
    ) -> Result<(), TestError> {
        assert_eq!(
            client.stage_sql(handle, id, body)?,
            LocalTransactionStageReceipt {
                engine: LocalTransactionEngine::Relational,
                handle,
                operation_ordinal: 1,
                rows_affected: 1,
            }
        );
        assert_eq!(
            client.stage_structure(handle, b"joint-key", b"joint-value", Some(50))?,
            LocalTransactionStageReceipt {
                engine: LocalTransactionEngine::Structure,
                handle,
                operation_ordinal: 2,
                rows_affected: 1,
            }
        );
        assert_eq!(
            client.stage_search(handle, index, b"joint-doc", body)?,
            LocalTransactionStageReceipt {
                engine: LocalTransactionEngine::Search,
                handle,
                operation_ordinal: 3,
                rows_affected: 1,
            }
        );
        Ok(())
    }

    #[test]
    fn uds_transaction_commits_three_engines_under_one_csn_and_reopens() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let seeded = seed_database(&data)?;
        let SeededDatabase {
            database,
            prior,
            index,
            seed_transaction_id,
            seed_csn,
        } = seeded;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server(database, &socket, Arc::clone(&clock))?;
        let mut client = SessionClient::connect(&socket)?;

        assert_inactive_and_begin_failures(&mut client)?;
        let begun = client.begin(DurabilityClass::Strict)?;
        assert_eq!(begun.read_csn, Some(seed_csn));
        assert_eq!(begun.logical_time_micros, 100);
        assert_active_session_guards(&mut client, begun.handle, index)?;
        assert_empty_commit_and_read_only_sql_fail(&mut client, begun.handle)?;
        stage_joint_transaction(&mut client, begun.handle, index, 1, "needle native")?;
        let mismatch =
            encode_local_transaction_commit(&mut client.buffer, begun.handle, 2)?.to_vec();
        client.expect_failure(
            FrameKind::Commit,
            &mismatch,
            LocalFailureCode::TransactionMismatch,
        )?;
        let committed = client.commit(begun.handle, 3)?;
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 1);
        assert_eq!(committed.handle, begun.handle);
        assert_eq!(committed.durability, DurabilityClass::Strict);
        assert_eq!(committed.staged_operations, 3);
        assert_eq!(committed.commit_csn.get(), seed_csn.get() + 1);
        assert_eq!(
            committed.transaction_id.get(),
            seed_transaction_id.get() + 1
        );
        assert_eq!(
            observe(&prior, index, 1, b"joint-key", "needle")?,
            expected_absent()
        );

        let reopened = NativeDatabase::open(&data)?;
        let current = reopened.snapshot(100)?;
        assert_eq!(current.visible_csn(), Some(committed.commit_csn));
        assert_eq!(current.ttl(b"joint-key"), Ttl::RemainingMicros(50));
        assert_eq!(
            observe(&current, index, 1, b"joint-key", "needle")?,
            expected_present(1, "needle native", b"joint-value", b"joint-doc")
        );
        Ok(())
    }

    fn stage_discarded_transaction(
        client: &mut SessionClient,
        handle: NonZeroU64,
        index: ObjectId,
        id: i64,
        text: &str,
    ) -> Result<(), TestError> {
        client.stage_sql(handle, id, text)?;
        client.stage_structure(handle, b"joint-key", b"discarded", None)?;
        client.stage_search(handle, index, b"joint-doc", text)?;
        Ok(())
    }

    #[test]
    fn local_search_lifecycle_preserves_ordinal_after_missing_document() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let mut seeded = seed_database(&data)?;
        let mut documents = seeded.database.begin(91, DurabilityClass::Strict)?;
        documents.index_document(seeded.index, b"replace".to_vec(), "old token")?;
        documents.index_document(seeded.index, b"delete".to_vec(), "retired token")?;
        documents.commit()?;

        let index = seeded.index;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server(seeded.database, &socket, Arc::clone(&clock))?;
        let mut client = SessionClient::connect(&socket)?;
        let begun = client.begin(DurabilityClass::Memory)?;
        assert_eq!(
            client
                .stage_search_replace(begun.handle, index, b"replace", "current token")?
                .operation_ordinal,
            1
        );
        let missing = encode_local_transaction_delete_document(
            &mut client.buffer,
            LocalTransactionDeleteDocumentRequest {
                handle: begun.handle,
                index,
                document_id: b"missing",
            },
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        client.expect_failure(FrameKind::Search, &missing, LocalFailureCode::EngineFailure)?;
        assert_eq!(
            client
                .stage_search_delete(begun.handle, index, b"delete")?
                .operation_ordinal,
            2
        );
        client.commit(begun.handle, 2)?;
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 1);
        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(
            reopened
                .match_latest_text(index, "current", 10)?
                .into_iter()
                .map(|hit| hit.document_id)
                .collect::<Vec<_>>(),
            [b"replace".to_vec()]
        );
        assert!(
            reopened
                .match_latest_text(index, "old retired", 10)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn peer_loss_discards_the_complete_private_batch() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let seeded = seed_database(&data)?;
        let index = seeded.index;
        let seed_csn = seeded.seed_csn;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server_clock = Arc::clone(&clock);
        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut database = seeded.database;
            let mut connection = listener.accept()?;
            let result =
                LocalDataSession::new(&mut database, server_clock.as_ref()).serve(&mut connection);
            if !matches!(result, Err(LocalSessionError::PeerClosed)) {
                return Err::<(), ServerError>("peer loss did not terminate fail-closed".into());
            }
            listener.close()?;
            Ok(())
        });
        let mut client = SessionClient::connect(&socket)?;
        let begun = client.begin(DurabilityClass::Strict)?;
        stage_discarded_transaction(&mut client, begun.handle, index, 6, "peer-loss-token")?;
        drop(client);
        join_server(server)?;

        assert_eq!(clock.samples(), 1);
        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(100)?;
        assert_eq!(snapshot.visible_csn(), Some(seed_csn));
        assert_eq!(
            observe(&snapshot, index, 6, b"joint-key", "peer-loss-token")?,
            expected_absent()
        );
        Ok(())
    }

    #[test]
    fn rollback_and_close_discard_complete_private_batches() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let seeded = seed_database(&data)?;
        let index = seeded.index;
        let seed_csn = seeded.seed_csn;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server(seeded.database, &socket, Arc::clone(&clock))?;
        let mut client = SessionClient::connect(&socket)?;

        let rolled_back = client.begin(DurabilityClass::Memory)?;
        stage_discarded_transaction(&mut client, rolled_back.handle, index, 2, "rollback-token")?;
        assert_eq!(
            client.rollback(rolled_back.handle)?,
            LocalTransactionRollbackReceipt {
                handle: rolled_back.handle,
                discarded_operations: 3,
            }
        );
        let closed = client.begin(DurabilityClass::Strict)?;
        assert_eq!(closed.handle.get(), rolled_back.handle.get() + 1);
        stage_discarded_transaction(&mut client, closed.handle, index, 3, "close-token")?;
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 2);
        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(100)?;
        assert_eq!(snapshot.visible_csn(), Some(seed_csn));
        assert_eq!(
            observe(&snapshot, index, 2, b"joint-key", "rollback-token")?,
            expected_absent()
        );
        assert_eq!(
            observe(&snapshot, index, 3, b"joint-key", "close-token")?,
            expected_absent()
        );
        Ok(())
    }

    #[test]
    fn semantic_failure_preserves_prior_operation_and_next_ordinal() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let seeded = seed_database(&data)?;
        let index = seeded.index;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server(seeded.database, &socket, Arc::clone(&clock))?;
        let mut client = SessionClient::connect(&socket)?;
        let begun = client.begin(DurabilityClass::Memory)?;

        assert_eq!(
            client
                .stage_structure(begun.handle, b"retained-key", b"retained-value", None)?
                .operation_ordinal,
            1
        );
        let invalid_sql = encode_local_transaction_sql_dml(
            &mut client.buffer,
            begun.handle,
            SELECT_EVENT,
            &[ScalarValue::Signed(1)],
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        client.expect_failure(
            FrameKind::Execute,
            &invalid_sql,
            LocalFailureCode::SqlInvalid,
        )?;
        assert_eq!(
            client
                .stage_search(begun.handle, index, b"retained-doc", "retained token",)?
                .operation_ordinal,
            2
        );
        client.commit(begun.handle, 2)?;
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 1);
        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(100)?;
        assert_eq!(
            snapshot.get(b"retained-key"),
            Some(b"retained-value".as_slice())
        );
        assert_eq!(
            snapshot
                .match_text(index, "retained", 10)?
                .into_iter()
                .map(|hit| hit.document_id)
                .collect::<Vec<_>>(),
            [b"retained-doc".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn begin_receipt_preflight_precedes_clock_and_batch_creation() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("small.sock");
        let seeded = seed_database(&data)?;
        let seed_csn = seeded.seed_csn;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server_with_payload(seeded.database, &socket, Arc::clone(&clock), 31)?;
        let mut client = SessionClient::connect_with_payload(&socket, 31)?;
        let mut buffer = Vec::new();
        let begin = encode_local_transaction_begin(&mut buffer, DurabilityClass::Strict)?.to_vec();
        client.expect_failure(FrameKind::Begin, &begin, LocalFailureCode::ResponseTooLarge)?;
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 0);
        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(reopened.snapshot(100)?.visible_csn(), Some(seed_csn));
        Ok(())
    }

    #[test]
    fn commit_receipt_preflight_preserves_the_active_batch_for_rollback() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("small.sock");
        let seeded = seed_database(&data)?;
        let seed_csn = seeded.seed_csn;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server_with_payload(seeded.database, &socket, Arc::clone(&clock), 39)?;
        let mut client = SessionClient::connect_with_payload(&socket, 39)?;
        let begun = client.begin(DurabilityClass::Memory)?;
        assert_eq!(
            client
                .stage_structure(begun.handle, b"", b"", None)?
                .operation_ordinal,
            1
        );
        let commit = encode_local_transaction_commit(&mut client.buffer, begun.handle, 1)?.to_vec();
        client.expect_failure(
            FrameKind::Commit,
            &commit,
            LocalFailureCode::ResponseTooLarge,
        )?;
        assert_eq!(client.rollback(begun.handle)?.discarded_operations, 1);
        client.close()?;
        join_server(server)?;

        assert_eq!(clock.samples(), 1);
        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(100)?;
        assert_eq!(snapshot.visible_csn(), Some(seed_csn));
        assert_eq!(snapshot.get(b""), None);
        Ok(())
    }

    #[test]
    fn transaction_operation_limit_fails_without_mutating_the_batch() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let seeded = seed_database(&data)?;
        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server = spawn_server(seeded.database, &socket, Arc::clone(&clock))?;
        let mut client = SessionClient::connect(&socket)?;
        let begun = client.begin(DurabilityClass::Memory)?;

        for ordinal in 1..=MAX_LOCAL_TRANSACTION_OPERATIONS {
            let key = format!("resource-{ordinal}");
            let receipt = client.stage_structure(begun.handle, key.as_bytes(), b"", None)?;
            assert_eq!(receipt.operation_ordinal, u64::try_from(ordinal)?);
        }
        let overflow = encode_local_transaction_structure_set(
            &mut client.buffer,
            LocalTransactionStructureSetRequest {
                handle: begun.handle,
                key: b"resource-overflow",
                value: b"",
                relative_ttl_micros: None,
            },
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        client.expect_failure(
            FrameKind::Structure,
            &overflow,
            LocalFailureCode::TransactionResourceLimit,
        )?;
        assert_eq!(
            client.rollback(begun.handle)?.discarded_operations,
            u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS)?
        );
        client.close()?;
        join_server(server)?;
        assert_eq!(clock.samples(), 1);
        Ok(())
    }

    fn commit_boundaries() -> [CommitBoundary; 7] {
        [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ]
    }

    fn boundary_recovers_commit(boundary: CommitBoundary) -> bool {
        matches!(
            boundary,
            CommitBoundary::WalAppended
                | CommitBoundary::WalSynchronized
                | CommitBoundary::RootPublished
        )
    }

    #[test]
    fn every_crash_boundary_recovers_none_or_the_complete_three_engine_commit()
    -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        for (ordinal, boundary) in commit_boundaries().into_iter().enumerate() {
            let data = temporary.path().join(format!("boundary-{ordinal}"));
            let seeded = seed_database(&data)?;
            let mut database = seeded.database;
            let mut batch = database.begin_optimistic(100, DurabilityClass::Strict)?;
            stage_all_engines(
                &mut batch,
                seeded.index,
                4,
                "crash-token",
                b"crash-key",
                b"crash-value",
                b"crash-doc",
            )?;
            assert!(matches!(
                database.commit_optimistic_with_interruption(batch, boundary),
                Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(&data)?;
            let snapshot = reopened.snapshot(100)?;
            let expected = if boundary_recovers_commit(boundary) {
                expected_present(4, "crash-token", b"crash-value", b"crash-doc")
            } else {
                expected_absent()
            };
            assert_eq!(
                observe(&snapshot, seeded.index, 4, b"crash-key", "crash-token")?,
                expected,
                "mixed recovery at {boundary:?}"
            );
            let expected_csn =
                seeded.seed_csn.get() + u64::from(boundary_recovers_commit(boundary));
            assert_eq!(
                snapshot.visible_csn().map(Csn::get),
                Some(expected_csn),
                "wrong recovery CSN at {boundary:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn conflicting_three_engine_batch_publishes_none_of_the_loser() -> Result<(), TestError> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let seeded = seed_database(&data)?;
        let mut database = seeded.database;
        let mut winner = database.begin_optimistic(100, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(100, DurabilityClass::Strict)?;
        stage_all_engines(
            &mut winner,
            seeded.index,
            5,
            "winnerexclusive",
            b"conflict-key",
            b"winner-value",
            b"conflict-doc",
        )?;
        stage_all_engines(
            &mut loser,
            seeded.index,
            5,
            "loserexclusive",
            b"conflict-key",
            b"loser-value",
            b"conflict-doc",
        )?;
        let committed = database.commit_optimistic(winner)?;
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::WriteConflict(_))
        ));

        let snapshot = database.snapshot(100)?;
        assert_eq!(snapshot.visible_csn(), Some(committed.commit_csn));
        assert_eq!(
            observe(
                &snapshot,
                seeded.index,
                5,
                b"conflict-key",
                "winnerexclusive",
            )?,
            expected_present(5, "winnerexclusive", b"winner-value", b"conflict-doc")
        );
        assert_eq!(
            observe(
                &snapshot,
                seeded.index,
                5,
                b"conflict-key",
                "loserexclusive",
            )?
            .documents,
            Vec::<Vec<u8>>::new()
        );
        Ok(())
    }
}
