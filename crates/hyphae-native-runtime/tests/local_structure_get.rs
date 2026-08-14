// SPDX-License-Identifier: AGPL-3.0-only

//! Native local structure operation codec and UDS session coverage.

use hyphae_native_runtime::{
    LOCAL_OPERATION_HEADER_SIZE, LocalFailureCode, LocalOperationCodecError, LocalValue,
    MAX_LOCAL_STRUCTURE_KEY_BYTES, decode_local_failure, decode_local_structure_get,
    decode_local_value, encode_local_failure, encode_local_structure_get, encode_local_value,
};

#[test]
fn local_operation_codecs_have_stable_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    assert_eq!(
        encode_local_structure_get(&mut buffer, b"\0k")?,
        [1, 1, 0, 0, 2, 0, 0, 0, 0, b'k']
    );
    assert_eq!(decode_local_structure_get(&buffer)?, b"\0k");

    assert_eq!(
        encode_local_value(&mut buffer, None, 8)?,
        [1, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(decode_local_value(&buffer)?, LocalValue::Missing);

    assert_eq!(
        encode_local_value(&mut buffer, Some(b""), 8)?,
        [1, 1, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(decode_local_value(&buffer)?, LocalValue::Present(b""));

    assert_eq!(
        encode_local_value(&mut buffer, Some(b"v"), 9)?,
        [1, 1, 0, 0, 1, 0, 0, 0, b'v']
    );
    assert_eq!(decode_local_value(&buffer)?, LocalValue::Present(b"v"));

    assert_eq!(
        encode_local_failure(&mut buffer, LocalFailureCode::InvalidRequest),
        [1, 1, 0, 0]
    );
    assert_eq!(
        decode_local_failure(&buffer)?,
        LocalFailureCode::InvalidRequest
    );
    Ok(())
}

#[test]
fn local_operation_codecs_reject_every_noncanonical_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let exact_key = vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES];
    assert_eq!(
        decode_local_structure_get(encode_local_structure_get(&mut buffer, &exact_key)?)?,
        exact_key
    );
    assert!(matches!(
        encode_local_structure_get(&mut buffer, &vec![b'k'; MAX_LOCAL_STRUCTURE_KEY_BYTES + 1]),
        Err(LocalOperationCodecError::KeyTooLarge)
    ));

    for length in 0..LOCAL_OPERATION_HEADER_SIZE {
        assert!(matches!(
            decode_local_structure_get(&[0; LOCAL_OPERATION_HEADER_SIZE][..length]),
            Err(LocalOperationCodecError::Truncated)
        ));
    }

    let mut request = [1_u8, 1, 0, 0, 1, 0, 0, 0].to_vec();
    assert!(matches!(
        decode_local_structure_get(&request),
        Err(LocalOperationCodecError::LengthMismatch)
    ));
    request[0] = 2;
    assert!(matches!(
        decode_local_structure_get(&request),
        Err(LocalOperationCodecError::UnsupportedVersion(2))
    ));
    request[0] = 1;
    request[2] = 1;
    assert!(matches!(
        decode_local_structure_get(&request),
        Err(LocalOperationCodecError::ReservedBytes)
    ));
    request[2] = 0;
    request[1] = 2;
    assert!(matches!(
        decode_local_structure_get(&request),
        Err(LocalOperationCodecError::UnknownStructureOpcode(2))
    ));

    assert!(matches!(
        decode_local_value(&[1, 2, 0, 0, 0, 0, 0, 0]),
        Err(LocalOperationCodecError::UnknownValueTag(2))
    ));
    assert!(matches!(
        decode_local_value(&[1, 0, 0, 0, 1, 0, 0, 0, b'x']),
        Err(LocalOperationCodecError::NoncanonicalMissing)
    ));
    assert!(matches!(
        encode_local_value(&mut buffer, Some(&[0; 57]), 64),
        Err(LocalOperationCodecError::PayloadTooLarge)
    ));
    assert!(matches!(
        decode_local_failure(&[1, 19, 0, 0]),
        Err(LocalOperationCodecError::UnknownFailureCode(19))
    ));
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
        thread,
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, LocalSessionError, NativeDatabase, NativeSchedulerClock,
        UdsFrameConnection, UdsFrameListener,
    };
    use hyphae_native_types::DurabilityClass;

    use super::{
        LocalFailureCode, LocalValue, MAX_LOCAL_STRUCTURE_KEY_BYTES, decode_local_failure,
        decode_local_value, encode_local_structure_get,
    };

    const MAXIMUM_PAYLOAD: usize = 8_192;

    struct TemporaryDirectory(PathBuf);

    static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let ordinal = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hyphae-local-get-{}-{ordinal}", std::process::id()));
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

    struct CountingClock {
        logical_time_micros: i64,
        samples: AtomicUsize,
    }

    impl CountingClock {
        fn new(logical_time_micros: i64) -> Self {
            Self {
                logical_time_micros,
                samples: AtomicUsize::new(0),
            }
        }
    }

    impl NativeSchedulerClock for CountingClock {
        fn logical_time_micros(&self) -> i64 {
            self.samples.fetch_add(1, Ordering::Relaxed);
            self.logical_time_micros
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
        request_buffer: &mut Vec<u8>,
        stream_id: u32,
        request_id: u64,
        key: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let payload = encode_local_structure_get(request_buffer, key)?;
        connection.send(FrameKind::Structure, stream_id, request_id, payload)?;
        receive_response(connection, FrameKind::Value, stream_id, request_id)
    }

    #[test]
    fn uds_session_reads_physical_values_ttl_and_request_local_failures()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(90, DurabilityClass::Memory)?;
        seed.set(b"live".to_vec(), b"value".to_vec(), None)?;
        seed.set(b"empty".to_vec(), Vec::new(), None)?;
        seed.set(b"expired".to_vec(), b"stale".to_vec(), Some(100))?;
        seed.set(b"large".to_vec(), vec![b'x'; MAXIMUM_PAYLOAD], None)?;
        seed.commit()?;

        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(
            move || -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
                let mut connection = listener.accept()?;
                let clock = CountingClock::new(100);
                let mut session = LocalDataSession::new(&mut database, &clock);
                session.serve(&mut connection)?;
                listener.close()?;
                Ok(clock.samples.load(Ordering::Relaxed))
            },
        );

        let mut client = UdsFrameConnection::connect(&socket, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Hello, 0, 1, b"")?;
        assert_eq!(
            receive_response(&mut client, FrameKind::Welcome, 0, 1)?,
            b""
        );

        let mut request_buffer = Vec::new();
        let live = request_get(&mut client, &mut request_buffer, 7, 2, b"live")?;
        assert_eq!(decode_local_value(&live)?, LocalValue::Present(b"value"));

        let missing = request_get(&mut client, &mut request_buffer, 7, 3, b"missing")?;
        assert_eq!(decode_local_value(&missing)?, LocalValue::Missing);

        let empty = request_get(&mut client, &mut request_buffer, 7, 4, b"empty")?;
        assert_eq!(decode_local_value(&empty)?, LocalValue::Present(b""));

        let expired = request_get(&mut client, &mut request_buffer, 7, 5, b"expired")?;
        assert_eq!(decode_local_value(&expired)?, LocalValue::Missing);

        client.send(FrameKind::Structure, 8, 6, &[1])?;
        let malformed = receive_response(&mut client, FrameKind::Failure, 8, 6)?;
        assert_eq!(
            decode_local_failure(&malformed)?,
            LocalFailureCode::InvalidRequest
        );

        let mut oversized_key = vec![1, 1, 0, 0];
        oversized_key
            .extend_from_slice(&u32::try_from(MAX_LOCAL_STRUCTURE_KEY_BYTES + 1)?.to_le_bytes());
        oversized_key.resize(8 + MAX_LOCAL_STRUCTURE_KEY_BYTES + 1, b'k');
        client.send(FrameKind::Structure, 8, 7, &oversized_key)?;
        let oversized = receive_response(&mut client, FrameKind::Failure, 8, 7)?;
        assert_eq!(
            decode_local_failure(&oversized)?,
            LocalFailureCode::KeyTooLarge
        );

        let payload = encode_local_structure_get(&mut request_buffer, b"large")?;
        client.send(FrameKind::Structure, 9, 8, payload)?;
        let too_large = receive_response(&mut client, FrameKind::Failure, 9, 8)?;
        assert_eq!(
            decode_local_failure(&too_large)?,
            LocalFailureCode::ResponseTooLarge
        );

        client.send(FrameKind::Cancel, 10, 9, b"")?;
        let unexpected = receive_response(&mut client, FrameKind::Failure, 10, 9)?;
        assert_eq!(
            decode_local_failure(&unexpected)?,
            LocalFailureCode::UnexpectedFrame
        );

        let recovered = request_get(&mut client, &mut request_buffer, 11, 10, b"live")?;
        assert_eq!(
            decode_local_value(&recovered)?,
            LocalValue::Present(b"value")
        );

        client.send(FrameKind::Close, 0, 11, b"")?;
        assert_eq!(receive_response(&mut client, FrameKind::Close, 0, 11)?, b"");
        let clock_samples = server
            .join()
            .map_err(|_| std::io::Error::other("local GET server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(clock_samples, 6);
        Ok(())
    }

    #[test]
    fn session_requires_room_for_operation_headers() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("small-data");
        let socket = temporary.path().join("small.sock");
        let mut database = NativeDatabase::create(data)?;
        let listener = UdsFrameListener::bind(&socket, 7)?;
        let server = thread::spawn(move || {
            let result = (|| -> Result<(), LocalSessionError> {
                let mut connection = listener.accept()?;
                let clock = CountingClock::new(0);
                LocalDataSession::new(&mut database, &clock).serve(&mut connection)
            })();
            let _ignored = listener.close();
            matches!(result, Err(LocalSessionError::PayloadBoundTooSmall))
        });
        let _client = UdsFrameConnection::connect(&socket, 7)?;
        assert!(
            server
                .join()
                .map_err(|_| std::io::Error::other("small session server panicked"))?
        );
        Ok(())
    }

    #[test]
    fn session_rejects_a_non_hello_first_frame_without_engine_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("handshake-data");
        let socket = temporary.path().join("handshake.sock");
        let mut database = NativeDatabase::create(data)?;
        let listener = UdsFrameListener::bind(&socket, 64)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            let clock = CountingClock::new(0);
            let result = LocalDataSession::new(&mut database, &clock).serve(&mut connection);
            listener.close()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
                matches!(result, Err(LocalSessionError::InvalidHandshake)),
                clock.samples.load(Ordering::Relaxed),
            ))
        });

        let mut client = UdsFrameConnection::connect(&socket, 64)?;
        client.send(FrameKind::Ping, 3, 41, b"")?;
        let failure = receive_response(&mut client, FrameKind::Failure, 3, 41)?;
        assert_eq!(
            decode_local_failure(&failure)?,
            LocalFailureCode::UnexpectedFrame
        );
        let (invalid_handshake, clock_samples) = server
            .join()
            .map_err(|_| std::io::Error::other("handshake server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert!(invalid_handshake);
        assert_eq!(clock_samples, 0);
        Ok(())
    }
}
