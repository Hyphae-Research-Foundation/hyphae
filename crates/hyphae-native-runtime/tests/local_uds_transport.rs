// SPDX-License-Identifier: Apache-2.0

//! Direct Unix-domain transport integration coverage.

#![cfg(unix)]

use std::{
    fs::{self, File},
    io::{Cursor, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, LOCAL_FRAME_HEADER_SIZE, LocalFrameIo,
    LocalProtocolError, LocalTransportError, UdsFrameConnection, UdsFrameListener, encode_frame,
};

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        // Hosted macOS exposes a TMPDIR long enough to exceed sockaddr_un's
        // pathname limit once the socket name is appended.
        let path = Path::new("/tmp").join(format!(
            "hyphae-native-local-uds-test-{}-{timestamp}",
            std::process::id()
        ));
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

struct FragmentedReader<R> {
    inner: R,
    maximum_read: usize,
}

impl<R: Read> Read for FragmentedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let limit = output.len().min(self.maximum_read);
        self.inner.read(&mut output[..limit])
    }
}

#[test]
fn framed_io_accepts_fragmented_reads_and_distinguishes_clean_eof()
-> Result<(), Box<dyn std::error::Error>> {
    let encoded = encode_frame(FrameKind::Ping, 7, 41, b"fragmented", 16)?;
    let mut reader = FragmentedReader {
        inner: Cursor::new(encoded),
        maximum_read: 3,
    };
    let mut io = LocalFrameIo::new(16)?;
    let decoded = io
        .receive_from(&mut reader)?
        .ok_or("fragmented frame was treated as EOF")?;
    assert_eq!(decoded.kind, FrameKind::Ping);
    assert_eq!(decoded.stream_id, 7);
    assert_eq!(decoded.request_id, 41);
    assert_eq!(decoded.payload, b"fragmented");
    assert!(io.receive_from(&mut reader)?.is_none());
    Ok(())
}

#[test]
fn framed_io_rejects_invalid_maximum_truncation_and_oversized_header()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(matches!(
        LocalFrameIo::new(DEFAULT_MAX_FRAME_PAYLOAD + 1),
        Err(LocalTransportError::MaximumPayloadTooLarge)
    ));

    let encoded = encode_frame(FrameKind::Ping, 0, 1, b"x", 1)?;
    let mut truncated_header = Cursor::new(&encoded[..LOCAL_FRAME_HEADER_SIZE - 1]);
    let mut io = LocalFrameIo::new(1)?;
    assert!(matches!(
        io.receive_from(&mut truncated_header),
        Err(LocalTransportError::Truncated)
    ));

    let mut truncated_payload = Cursor::new(&encoded[..encoded.len() - 1]);
    assert!(matches!(
        io.receive_from(&mut truncated_payload),
        Err(LocalTransportError::Truncated)
    ));

    let mut oversized = Cursor::new(encoded);
    let mut zero_payload_io = LocalFrameIo::new(0)?;
    assert!(matches!(
        zero_payload_io.receive_from(&mut oversized),
        Err(LocalTransportError::Protocol(
            LocalProtocolError::PayloadTooLarge
        ))
    ));
    assert_eq!(
        oversized.position(),
        u64::try_from(LOCAL_FRAME_HEADER_SIZE)?
    );
    Ok(())
}

#[test]
fn uds_round_trip_preserves_order_identity_payload_and_permissions()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let socket = temporary.path().join("session.sock");
    let listener = UdsFrameListener::bind(&socket, 64)?;
    assert_eq!(
        fs::symlink_metadata(&socket)?.permissions().mode() & 0o777,
        0o600
    );

    let server = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let mut connection = listener.accept()?;
            let (hello_stream_id, hello_request_id) = {
                let hello = connection
                    .receive()?
                    .ok_or(LocalTransportError::Truncated)?;
                if hello.kind != FrameKind::Hello
                    || hello.request_id != 1
                    || !hello.payload.is_empty()
                {
                    return Err(std::io::Error::other("unexpected HELLO frame").into());
                }
                (hello.stream_id, hello.request_id)
            };
            connection.send(FrameKind::Welcome, hello_stream_id, hello_request_id, b"")?;

            for expected in 2_u64..=4 {
                let (stream_id, request_id, payload) = {
                    let ping = connection
                        .receive()?
                        .ok_or(LocalTransportError::Truncated)?;
                    if ping.kind != FrameKind::Ping
                        || ping.stream_id != 9
                        || ping.request_id != expected
                    {
                        return Err(std::io::Error::other("unexpected PING frame").into());
                    }
                    (ping.stream_id, ping.request_id, ping.payload.to_vec())
                };
                connection.send(FrameKind::Ping, stream_id, request_id, &payload)?;
            }

            let (close_stream_id, close_request_id) = {
                let close = connection
                    .receive()?
                    .ok_or(LocalTransportError::Truncated)?;
                if close.kind != FrameKind::Close
                    || close.request_id != 5
                    || !close.payload.is_empty()
                {
                    return Err(std::io::Error::other("unexpected CLOSE frame").into());
                }
                (close.stream_id, close.request_id)
            };
            connection.send(FrameKind::Close, close_stream_id, close_request_id, b"")?;
            listener.close()?;
            Ok(())
        },
    );

    let mut client = UdsFrameConnection::connect(&socket, 64)?;
    client.send(FrameKind::Hello, 0, 1, b"")?;
    let welcome = client.receive()?.ok_or("server closed before WELCOME")?;
    assert_eq!(welcome.kind, FrameKind::Welcome);
    assert_eq!(welcome.request_id, 1);

    for request_id in 2_u64..=4 {
        let payload = request_id.to_be_bytes();
        client.send(FrameKind::Ping, 9, request_id, &payload)?;
        let ping = client.receive()?.ok_or("server closed before PING")?;
        assert_eq!(ping.kind, FrameKind::Ping);
        assert_eq!(ping.stream_id, 9);
        assert_eq!(ping.request_id, request_id);
        assert_eq!(ping.payload, payload);
    }

    client.send(FrameKind::Close, 0, 5, b"")?;
    let close = client.receive()?.ok_or("server closed before CLOSE")?;
    assert_eq!(close.kind, FrameKind::Close);
    assert_eq!(close.request_id, 5);
    server
        .join()
        .map_err(|_| std::io::Error::other("UDS server panicked"))?
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    assert!(!socket.exists());
    Ok(())
}

#[test]
fn uds_listener_never_replaces_or_removes_an_unowned_path() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TemporaryDirectory::create()?;
    let socket = temporary.path().join("owned.sock");
    File::create(&socket)?;
    assert!(matches!(
        UdsFrameListener::bind(&socket, 0),
        Err(LocalTransportError::EndpointExists)
    ));
    assert!(socket.is_file());

    let replacement = temporary.path().join("replacement.sock");
    let listener = UdsFrameListener::bind(&replacement, 0)?;
    fs::remove_file(&replacement)?;
    File::create(&replacement)?;
    assert!(matches!(
        listener.close(),
        Err(LocalTransportError::EndpointReplaced)
    ));
    assert!(replacement.is_file());
    Ok(())
}
