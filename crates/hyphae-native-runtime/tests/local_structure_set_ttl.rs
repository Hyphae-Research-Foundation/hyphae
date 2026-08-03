// SPDX-License-Identifier: Apache-2.0

//! Native local structure SET, TTL, receipt, and mutation-session coverage.

use hyphae_native_runtime::{
    LOCAL_COMMIT_RECEIPT_SIZE, LOCAL_STRUCTURE_SET_HEADER_SIZE, LOCAL_TTL_PAYLOAD_SIZE,
    LocalFailureCode, LocalOperationCodecError, LocalStructureCommitReceipt, LocalStructureRequest,
    LocalStructureSetRequest, LocalTtlValue, MAX_LOCAL_STRUCTURE_KEY_BYTES, decode_local_failure,
    decode_local_structure_commit_receipt, decode_local_structure_request, decode_local_ttl,
    encode_local_failure, encode_local_structure_commit_receipt, encode_local_structure_set,
    encode_local_structure_ttl, encode_local_ttl,
};
use hyphae_native_types::{Csn, DurabilityClass, TransactionId};

#[test]
fn local_set_ttl_codecs_have_stable_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    assert_eq!(
        encode_local_structure_set(&mut buffer, b"\0k", b"v", None, DurabilityClass::Strict, 23,)?,
        [
            1, 2, 1, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'k', b'v',
        ]
    );
    assert_eq!(
        decode_local_structure_request(&buffer)?,
        LocalStructureRequest::Set(LocalStructureSetRequest {
            key: b"\0k",
            value: b"v",
            relative_ttl_micros: None,
            durability: DurabilityClass::Strict,
        })
    );

    assert_eq!(
        encode_local_structure_set(
            &mut buffer,
            b"k",
            b"",
            Some(25),
            DurabilityClass::Memory,
            21,
        )?,
        [
            1, 2, 3, 1, 1, 0, 0, 0, 0, 0, 0, 0, 25, 0, 0, 0, 0, 0, 0, 0, b'k',
        ]
    );
    assert_eq!(
        decode_local_structure_request(&buffer)?,
        LocalStructureRequest::Set(LocalStructureSetRequest {
            key: b"k",
            value: b"",
            relative_ttl_micros: Some(25),
            durability: DurabilityClass::Memory,
        })
    );

    assert_eq!(
        encode_local_structure_ttl(&mut buffer, b"\0k")?,
        [1, 3, 0, 0, 2, 0, 0, 0, 0, b'k']
    );
    assert_eq!(
        decode_local_structure_request(&buffer)?,
        LocalStructureRequest::Ttl(b"\0k")
    );

    assert_eq!(
        encode_local_ttl(&mut buffer, LocalTtlValue::Missing)?,
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(decode_local_ttl(&buffer)?, LocalTtlValue::Missing);
    assert_eq!(
        encode_local_ttl(&mut buffer, LocalTtlValue::Persistent)?,
        [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(decode_local_ttl(&buffer)?, LocalTtlValue::Persistent);
    assert_eq!(
        encode_local_ttl(&mut buffer, LocalTtlValue::RemainingMicros(25))?,
        [1, 2, 0, 0, 25, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        decode_local_ttl(&buffer)?,
        LocalTtlValue::RemainingMicros(25)
    );

    let receipt = LocalStructureCommitReceipt {
        transaction_id: TransactionId::new(2)?,
        commit_csn: Csn::new(3)?,
        durability: DurabilityClass::Memory,
    };
    assert_eq!(
        encode_local_structure_commit_receipt(&mut buffer, receipt)?,
        [
            1, 1, 3, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0,
        ]
    );
    assert_eq!(decode_local_structure_commit_receipt(&buffer)?, receipt);

    assert_eq!(
        encode_local_failure(&mut buffer, LocalFailureCode::UnsupportedDurability),
        [1, 6, 0, 0]
    );
    assert_eq!(
        decode_local_failure(&buffer)?,
        LocalFailureCode::UnsupportedDurability
    );
    assert_eq!(
        encode_local_failure(&mut buffer, LocalFailureCode::ExpiryOverflow),
        [1, 7, 0, 0]
    );
    assert_eq!(
        decode_local_failure(&buffer)?,
        LocalFailureCode::ExpiryOverflow
    );
    Ok(())
}

#[test]
fn local_set_codec_enforces_key_payload_and_durability_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let exact_key = vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES];
    let encoded = encode_local_structure_set(
        &mut buffer,
        &exact_key,
        b"",
        None,
        DurabilityClass::Memory,
        LOCAL_STRUCTURE_SET_HEADER_SIZE + exact_key.len(),
    )?;
    assert!(matches!(
        decode_local_structure_request(encoded)?,
        LocalStructureRequest::Set(LocalStructureSetRequest { key, .. }) if key == exact_key
    ));
    assert!(matches!(
        encode_local_structure_set(
            &mut buffer,
            &vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES + 1],
            b"",
            None,
            DurabilityClass::Memory,
            usize::MAX,
        ),
        Err(LocalOperationCodecError::KeyTooLarge)
    ));
    assert!(matches!(
        encode_local_structure_set(
            &mut buffer,
            b"k",
            b"v",
            None,
            DurabilityClass::Memory,
            LOCAL_STRUCTURE_SET_HEADER_SIZE + 1,
        ),
        Err(LocalOperationCodecError::PayloadTooLarge)
    ));
    assert!(matches!(
        encode_local_structure_set(
            &mut buffer,
            b"k",
            b"v",
            None,
            DurabilityClass::Group,
            usize::MAX,
        ),
        Err(LocalOperationCodecError::UnsupportedDurability(2))
    ));
    Ok(())
}

#[test]
fn local_set_codec_rejects_noncanonical_headers_and_ttl() {
    let mut request = [
        1_u8, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    for length in 0..LOCAL_STRUCTURE_SET_HEADER_SIZE {
        assert!(matches!(
            decode_local_structure_request(&request[..length]),
            Err(LocalOperationCodecError::Truncated)
        ));
    }
    request[0] = 2;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::UnsupportedVersion(2))
    ));
    request[0] = 1;
    request[1] = 5;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::UnknownStructureOpcode(5))
    ));
    request[1] = 2;
    request[2] = 2;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::UnsupportedDurability(2))
    ));
    request[2] = 4;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::UnknownDurability(4))
    ));
    request[2] = 1;
    request[3] = 2;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::UnknownExpiryMode(2))
    ));
    request[3] = 0;
    request[12] = 1;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::NoncanonicalRelativeTtl)
    ));
    request[3] = 1;
    request[12..20].fill(0);
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::NoncanonicalRelativeTtl)
    ));
    request[12..20].copy_from_slice(&(-1_i64).to_le_bytes());
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::NoncanonicalRelativeTtl)
    ));
    request[12..20].copy_from_slice(&1_i64.to_le_bytes());
    request[4] = 1;
    assert!(matches!(
        decode_local_structure_request(&request),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    request[4] = 0;
    let mut trailing = request.to_vec();
    trailing.push(0);
    assert!(matches!(
        decode_local_structure_request(&trailing),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
}

#[test]
fn local_ttl_and_receipt_codecs_reject_every_noncanonical_boundary() {
    let mut ttl = [1_u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    for length in 0..LOCAL_TTL_PAYLOAD_SIZE {
        assert!(matches!(
            decode_local_ttl(&ttl[..length]),
            Err(LocalOperationCodecError::Truncated)
        ));
    }
    assert!(matches!(
        decode_local_ttl(&[0; LOCAL_TTL_PAYLOAD_SIZE + 1]),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    ttl[0] = 2;
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::UnsupportedVersion(2))
    ));
    ttl[0] = 1;
    ttl[2] = 1;
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::ReservedBytes)
    ));
    ttl[2] = 0;
    ttl[1] = 3;
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::UnknownTtlTag(3))
    ));
    ttl[1] = 0;
    ttl[4] = 1;
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::NoncanonicalTtl)
    ));
    ttl[1] = 2;
    ttl[4..12].fill(0);
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::NoncanonicalTtl)
    ));
    ttl[4..12].copy_from_slice(&(-1_i64).to_le_bytes());
    assert!(matches!(
        decode_local_ttl(&ttl),
        Err(LocalOperationCodecError::NoncanonicalTtl)
    ));

    let mut receipt = [
        1_u8, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
    ];
    for length in 0..LOCAL_COMMIT_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_structure_commit_receipt(&receipt[..length]),
            Err(LocalOperationCodecError::Truncated)
        ));
    }
    assert!(matches!(
        decode_local_structure_commit_receipt(&[0; LOCAL_COMMIT_RECEIPT_SIZE + 1]),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    receipt[1] = 2;
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::UnknownReceiptTag(2))
    ));
    receipt[1] = 1;
    receipt[2] = 2;
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::UnsupportedDurability(2))
    ));
    receipt[2] = 4;
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::UnknownDurability(4))
    ));
    receipt[2] = 1;
    receipt[3] = 1;
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::ReservedBytes)
    ));
    receipt[3] = 0;
    receipt[4..20].fill(0);
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::InvalidIdentity)
    ));
    receipt[4] = 1;
    receipt[20..28].fill(0);
    assert!(matches!(
        decode_local_structure_commit_receipt(&receipt),
        Err(LocalOperationCodecError::InvalidIdentity)
    ));
    assert!(matches!(
        decode_local_failure(&[1, 19, 0, 0]),
        Err(LocalOperationCodecError::UnknownFailureCode(19))
    ));
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicI64, AtomicUsize, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, LocalValue, NativeDatabase, NativeSchedulerClock, Ttl,
        UdsFrameConnection, UdsFrameListener, decode_local_failure,
        decode_local_structure_commit_receipt, decode_local_ttl, decode_local_value,
        encode_local_structure_get, encode_local_structure_set, encode_local_structure_ttl,
    };

    use super::{
        Csn, DurabilityClass, LocalFailureCode, LocalStructureCommitReceipt, LocalTtlValue,
        TransactionId,
    };

    const MAXIMUM_PAYLOAD: usize = 8_192;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = Path::new("/tmp").join(format!("hy-set-{}-{timestamp}", std::process::id()));
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

    struct AdjustableClock {
        logical_time_micros: AtomicI64,
        samples: AtomicUsize,
    }

    impl AdjustableClock {
        fn new(logical_time_micros: i64) -> Self {
            Self {
                logical_time_micros: AtomicI64::new(logical_time_micros),
                samples: AtomicUsize::new(0),
            }
        }

        fn set(&self, logical_time_micros: i64) {
            self.logical_time_micros
                .store(logical_time_micros, Ordering::Relaxed);
        }
    }

    impl NativeSchedulerClock for AdjustableClock {
        fn logical_time_micros(&self) -> i64 {
            self.samples.fetch_add(1, Ordering::Relaxed);
            self.logical_time_micros.load(Ordering::Relaxed)
        }
    }

    fn receive_response(
        connection: &mut UdsFrameConnection,
        expected_kind: FrameKind,
        stream_id: u32,
        request_id: u64,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let frame = connection.receive()?.ok_or("server closed early")?;
        if frame.kind != expected_kind
            || frame.stream_id != stream_id
            || frame.request_id != request_id
        {
            return Err("response identity diverged".into());
        }
        Ok(frame.payload.to_vec())
    }

    fn request_get(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        stream_id: u32,
        request_id: u64,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
        let payload = encode_local_structure_get(buffer, key)?;
        connection.send(FrameKind::Structure, stream_id, request_id, payload)?;
        let response = receive_response(connection, FrameKind::Value, stream_id, request_id)?;
        Ok(match decode_local_value(&response)? {
            LocalValue::Missing => None,
            LocalValue::Present(value) => Some(value.to_vec()),
        })
    }

    fn request_ttl(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        stream_id: u32,
        request_id: u64,
        key: &[u8],
    ) -> Result<LocalTtlValue, Box<dyn std::error::Error>> {
        let payload = encode_local_structure_ttl(buffer, key)?;
        connection.send(FrameKind::Structure, stream_id, request_id, payload)?;
        let response = receive_response(connection, FrameKind::Value, stream_id, request_id)?;
        Ok(decode_local_ttl(&response)?)
    }

    #[derive(Clone, Copy)]
    struct SetCall<'value> {
        stream_id: u32,
        request_id: u64,
        key: &'value [u8],
        value: &'value [u8],
        relative_ttl_micros: Option<i64>,
        durability: DurabilityClass,
    }

    fn request_set(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        call: SetCall<'_>,
    ) -> Result<LocalStructureCommitReceipt, Box<dyn std::error::Error>> {
        let payload = encode_local_structure_set(
            buffer,
            call.key,
            call.value,
            call.relative_ttl_micros,
            call.durability,
            MAXIMUM_PAYLOAD,
        )?;
        connection.send(
            FrameKind::Structure,
            call.stream_id,
            call.request_id,
            payload,
        )?;
        let response = receive_response(
            connection,
            FrameKind::Receipt,
            call.stream_id,
            call.request_id,
        )?;
        Ok(decode_local_structure_commit_receipt(&response)?)
    }

    fn exercise_request_local_failures(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        client.send(FrameKind::Structure, 5, 2, &[1])?;
        assert_eq!(
            decode_local_failure(&receive_response(client, FrameKind::Failure, 5, 2)?)?,
            LocalFailureCode::InvalidRequest
        );

        let mut group = encode_local_structure_set(
            buffer,
            b"group",
            b"value",
            None,
            DurabilityClass::Strict,
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        group[2] = DurabilityClass::Group as u8;
        client.send(FrameKind::Structure, 5, 3, &group)?;
        assert_eq!(
            decode_local_failure(&receive_response(client, FrameKind::Failure, 5, 3)?)?,
            LocalFailureCode::UnsupportedDurability
        );

        let overflow = encode_local_structure_set(
            buffer,
            b"overflow",
            b"value",
            Some(i64::MAX),
            DurabilityClass::Memory,
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Structure, 5, 4, overflow)?;
        assert_eq!(
            decode_local_failure(&receive_response(client, FrameKind::Failure, 5, 4)?)?,
            LocalFailureCode::ExpiryOverflow
        );

        let mismatch = encode_local_structure_set(
            buffer,
            b"hash",
            b"value",
            None,
            DurabilityClass::Memory,
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Structure, 5, 5, mismatch)?;
        assert_eq!(
            decode_local_failure(&receive_response(client, FrameKind::Failure, 5, 5)?)?,
            LocalFailureCode::EngineFailure
        );
        Ok(())
    }

    fn exercise_successful_mutations(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        clock: &AdjustableClock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            request_set(
                client,
                buffer,
                SetCall {
                    stream_id: 6,
                    request_id: 6,
                    key: b"memory",
                    value: b"warm",
                    relative_ttl_micros: None,
                    durability: DurabilityClass::Memory,
                },
            )?,
            LocalStructureCommitReceipt {
                transaction_id: TransactionId::new(2)?,
                commit_csn: Csn::new(2)?,
                durability: DurabilityClass::Memory,
            }
        );
        assert_eq!(
            request_ttl(client, buffer, 6, 7, b"memory")?,
            LocalTtlValue::Persistent
        );

        assert_eq!(
            request_set(
                client,
                buffer,
                SetCall {
                    stream_id: 7,
                    request_id: 8,
                    key: b"strict",
                    value: b"durable",
                    relative_ttl_micros: Some(25),
                    durability: DurabilityClass::Strict,
                },
            )?,
            LocalStructureCommitReceipt {
                transaction_id: TransactionId::new(3)?,
                commit_csn: Csn::new(3)?,
                durability: DurabilityClass::Strict,
            }
        );
        assert_eq!(
            request_ttl(client, buffer, 7, 9, b"strict")?,
            LocalTtlValue::RemainingMicros(25)
        );
        assert_eq!(
            request_get(client, buffer, 7, 10, b"strict")?,
            Some(b"durable".to_vec())
        );

        clock.set(125);
        assert_eq!(
            request_ttl(client, buffer, 8, 11, b"strict")?,
            LocalTtlValue::Missing
        );
        assert_eq!(request_get(client, buffer, 8, 12, b"strict")?, None);
        Ok(())
    }

    #[test]
    fn uds_session_commits_set_reports_ttl_and_reopens_strict_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(90, DurabilityClass::Memory)?;
        seed.create_hash(b"hash".to_vec())?;
        seed.commit()?;

        let clock = Arc::new(AdjustableClock::new(100));
        let server_clock = Arc::clone(&clock);
        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            let mut session = LocalDataSession::new(&mut database, server_clock.as_ref());
            session.serve(&mut connection)?;
            listener.close()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let mut client = UdsFrameConnection::connect(&socket, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Hello, 0, 1, b"")?;
        assert_eq!(
            receive_response(&mut client, FrameKind::Welcome, 0, 1)?,
            b""
        );
        let mut buffer = Vec::new();

        exercise_request_local_failures(&mut client, &mut buffer)?;

        exercise_successful_mutations(&mut client, &mut buffer, clock.as_ref())?;

        client.send(FrameKind::Close, 0, 13, b"")?;
        assert_eq!(receive_response(&mut client, FrameKind::Close, 0, 13)?, b"");
        server
            .join()
            .map_err(|_| std::io::Error::other("local SET server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(clock.samples.load(Ordering::Relaxed), 9);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(
            reopened.get_latest_structure(b"memory", 100)?,
            Some(b"warm".to_vec())
        );
        assert_eq!(
            reopened.get_latest_structure(b"strict", 100)?,
            Some(b"durable".to_vec())
        );
        assert_eq!(
            reopened.ttl_latest_structure(b"strict", 100)?,
            Ttl::RemainingMicros(25)
        );
        Ok(())
    }

    #[test]
    fn set_preflights_receipt_capacity_before_clock_or_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("small-data");
        let socket = temporary.path().join("small.sock");
        let mut database = NativeDatabase::create(&data)?;
        let clock = Arc::new(AdjustableClock::new(100));
        let server_clock = Arc::clone(&clock);
        let listener = UdsFrameListener::bind(&socket, 20)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            LocalDataSession::new(&mut database, server_clock.as_ref()).serve(&mut connection)?;
            listener.close()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let mut client = UdsFrameConnection::connect(&socket, 20)?;
        client.send(FrameKind::Hello, 0, 1, b"")?;
        assert_eq!(
            receive_response(&mut client, FrameKind::Welcome, 0, 1)?,
            b""
        );
        let mut buffer = Vec::new();
        let request =
            encode_local_structure_set(&mut buffer, b"", b"", None, DurabilityClass::Memory, 20)?;
        client.send(FrameKind::Structure, 1, 2, request)?;
        assert_eq!(
            decode_local_failure(&receive_response(&mut client, FrameKind::Failure, 1, 2)?)?,
            LocalFailureCode::ResponseTooLarge
        );
        client.send(FrameKind::Close, 0, 3, b"")?;
        assert_eq!(receive_response(&mut client, FrameKind::Close, 0, 3)?, b"");
        server
            .join()
            .map_err(|_| std::io::Error::other("small SET server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(clock.samples.load(Ordering::Relaxed), 0);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(reopened.get_latest_structure(b"", 100)?, None);
        Ok(())
    }
}
