// SPDX-License-Identifier: AGPL-3.0-only

//! Multi-client daemon handshake, completion, flow control, and disconnect tests.

#![cfg(unix)]

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    NativeProduct, ProductDurabilityPolicy, ProductLimits, ProductOperation, ProductResponse,
};
use hyphae_native_protocol::{
    AsyncFrameIo, FrameKind, Hello, ProvisionalStream, WireRequest, decode_end, decode_welcome,
    encode_cancel, encode_deadline, encode_frame, encode_hello, encode_product_request,
    encode_window_update,
};
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::{Stream, prelude::*};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    root: PathBuf,
    data: PathBuf,
    socket: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hnd-{name}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir(&root)?;
        Ok(Self {
            data: root.join("data"),
            socket: root.join("native.sock"),
            root,
        })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.root);
    }
}

struct Client {
    stream: Stream,
    codec: AsyncFrameIo,
}

impl Client {
    async fn connect(path: &Path, initial_window: u32) -> Result<Self, Box<dyn Error>> {
        Self::connect_with_hello(
            path,
            Hello {
                initial_window,
                ..Hello::default()
            },
        )
        .await
    }

    async fn connect_with_hello(path: &Path, hello: Hello) -> Result<Self, Box<dyn Error>> {
        let path = path.to_string_lossy();
        let name = path.to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        let mut codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
        codec
            .send(&mut &stream, FrameKind::Hello, 0, 1, &encode_hello(&hello)?)
            .await?;
        let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
        let welcome = receive
            .receive(&mut &stream)
            .await?
            .ok_or("server closed before WELCOME")?;
        assert_eq!(welcome.kind, FrameKind::Welcome);
        let welcome = decode_welcome(&welcome.payload)?;
        assert_eq!(welcome.initial_window, hello.initial_window);
        codec = AsyncFrameIo::new(usize::try_from(welcome.maximum_frame_payload)?)?;
        Ok(Self { stream, codec })
    }

    async fn send_request(
        &self,
        stream_id: u32,
        request_id: u64,
        request: &WireRequest,
    ) -> Result<(), Box<dyn Error>> {
        self.codec
            .send(
                &mut &self.stream,
                FrameKind::Execute,
                stream_id,
                request_id,
                &encode_product_request(request)?,
            )
            .await?;
        Ok(())
    }

    async fn send_kind(
        &self,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        request: &WireRequest,
    ) -> Result<(), Box<dyn Error>> {
        self.codec
            .send(
                &mut &self.stream,
                kind,
                stream_id,
                request_id,
                &encode_product_request(request)?,
            )
            .await?;
        Ok(())
    }

    async fn response(
        &self,
        stream_id: u32,
        request_id: u64,
    ) -> Result<ProductResponse, Box<dyn Error>> {
        let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
        let mut provisional = ProvisionalStream::new();
        loop {
            let frame = receive
                .receive(&mut &self.stream)
                .await?
                .ok_or("stream ended before END")?;
            assert_eq!((frame.stream_id, frame.request_id), (stream_id, request_id));
            match frame.kind {
                FrameKind::Data => provisional.push(&frame.payload, 16 * 1024 * 1024)?,
                FrameKind::End => {
                    let bytes = provisional.complete(decode_end(&frame.payload)?)?;
                    return Ok(hyphae_native_protocol::decode_product_response(&bytes)?);
                }
                FrameKind::Failure => {
                    return Err(hyphae_native_protocol::decode_failure(&frame.payload)?.into());
                }
                _ => return Err("unexpected response frame".into()),
            }
        }
    }
}

fn request(operation: ProductOperation) -> WireRequest {
    WireRequest {
        operation,
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::MEMORY,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_clients_handshake_share_one_product_and_endpoint_is_private()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("multi-client")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    assert_eq!(
        fs::metadata(&test.socket)?.permissions().mode() & 0o777,
        0o600
    );
    let first = Client::connect(&test.socket, 64 * 1024).await?;
    let second = Client::connect(&test.socket, 64 * 1024).await?;

    first
        .send_request(
            1,
            2,
            &request(ProductOperation::StructureSet {
                key: b"shared".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            }),
        )
        .await?;
    assert!(matches!(
        first.response(1, 2).await?,
        ProductResponse::StructureSet(_)
    ));
    second
        .send_request(
            2,
            3,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    assert_eq!(
        second.response(2, 3).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    drop(first);
    drop(second);
    daemon.shutdown().await?;
    assert!(!test.socket.exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_protocol_exposes_the_shared_telemetry_snapshot() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("telemetry")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect(&test.socket, 64 * 1024).await?;
    client
        .send_request(1, 2, &request(ProductOperation::Capabilities))
        .await?;
    client.response(1, 2).await?;
    client
        .send_request(2, 3, &request(ProductOperation::Telemetry))
        .await?;
    let ProductResponse::Telemetry(snapshot) = client.response(2, 3).await? else {
        return Err("local telemetry returned the wrong response".into());
    };
    assert_ne!(snapshot.process_start_identity, 0);
    assert_ne!(snapshot.session_start_identity, 0);
    assert!(
        snapshot
            .metrics
            .iter()
            .all(|row| row.descriptor.labels.is_empty())
    );
    drop(client);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_frames_release_the_stream_before_client_reuse() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("stream-reuse")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect(&test.socket, 64 * 1024).await?;

    for request_id in 2..=129 {
        client
            .send_request(1, request_id, &request(ProductOperation::Capabilities))
            .await?;
        assert!(matches!(
            client.response(1, request_id).await?,
            ProductResponse::Capabilities(_)
        ));
    }

    drop(client);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_handles_are_session_local_and_disconnect_does_not_stop_daemon()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("prepared-disconnect")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let first = Client::connect(&test.socket, 64 * 1024).await?;
    first
        .send_request(
            1,
            2,
            &request(ProductOperation::ExecuteSql {
                statement: "CREATE TABLE items (id BIGINT PRIMARY KEY)".to_owned(),
                parameters: vec![],
            }),
        )
        .await?;
    first.response(1, 2).await?;
    first
        .send_kind(
            FrameKind::Prepare,
            1,
            3,
            &request(ProductOperation::PrepareSql {
                statement: "SELECT id FROM items WHERE id = ?".to_owned(),
            }),
        )
        .await?;
    let ProductResponse::PreparedSql { handle, .. } = first.response(1, 3).await? else {
        return Err("prepare did not return a handle".into());
    };
    drop(first);

    let second = Client::connect(&test.socket, 64 * 1024).await?;
    second
        .send_request(
            2,
            4,
            &request(ProductOperation::ExecutePrepared {
                handle,
                parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
            }),
        )
        .await?;
    assert!(second.response(2, 4).await.is_err());
    drop(second);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_outcome_can_be_resolved_after_disconnect() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("disconnect-outcome")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let first = Client::connect(&test.socket, 64 * 1024).await?;
    first
        .send_request(
            1,
            20,
            &request(ProductOperation::StructureSet {
                key: b"committed".to_vec(),
                value: b"yes".to_vec(),
                expires_at_micros: None,
            }),
        )
        .await?;
    let ProductResponse::StructureSet(hyphae_native_product::ProductCommitOutcome::Committed(
        receipt,
    )) = first.response(1, 20).await?
    else {
        return Err("structure set did not commit".into());
    };
    drop(first);

    let second = Client::connect(&test.socket, 64 * 1024).await?;
    second
        .send_request(
            2,
            21,
            &request(ProductOperation::TransactionStatus {
                transaction_id: receipt.transaction_id,
            }),
        )
        .await?;
    assert!(matches!(
        second.response(2, 21).await?,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Committed(_)
        )
    ));
    drop(second);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_handle_can_be_deallocated() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("deallocate")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect(&test.socket, 64 * 1024).await?;
    client
        .send_request(
            1,
            2,
            &request(ProductOperation::ExecuteSql {
                statement: "CREATE TABLE items (id BIGINT PRIMARY KEY)".to_owned(),
                parameters: vec![],
            }),
        )
        .await?;
    client.response(1, 2).await?;
    client
        .send_kind(
            FrameKind::Prepare,
            1,
            3,
            &request(ProductOperation::PrepareSql {
                statement: "SELECT id FROM items WHERE id = ?".to_owned(),
            }),
        )
        .await?;
    let ProductResponse::PreparedSql { handle, .. } = client.response(1, 3).await? else {
        return Err("prepare did not return a handle".into());
    };
    client
        .send_kind(
            FrameKind::Deallocate,
            1,
            4,
            &request(ProductOperation::DeallocatePrepared { handle }),
        )
        .await?;
    assert_eq!(client.response(1, 4).await?, ProductResponse::Deallocated);
    let missing = Client::connect(&test.socket, 64 * 1024).await?;
    missing
        .send_request(
            1,
            5,
            &request(ProductOperation::ExecutePrepared {
                handle,
                parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
            }),
        )
        .await?;
    assert!(missing.response(1, 5).await.is_err());
    drop(missing);
    drop(client);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flow_control_stalls_data_until_window_update() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("flow-control")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect(&test.socket, 8).await?;
    client
        .send_request(7, 2, &request(ProductOperation::Capabilities))
        .await?;
    let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
    let first = receive
        .receive(&mut &client.stream)
        .await?
        .ok_or("missing first DATA")?;
    assert_eq!(first.kind, FrameKind::Data);
    assert_eq!(first.payload.len(), 8);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(25),
            receive.receive(&mut &client.stream)
        )
        .await
        .is_err()
    );
    client
        .codec
        .send(
            &mut &client.stream,
            FrameKind::WindowUpdate,
            7,
            2,
            &encode_window_update(4096)?,
        )
        .await?;
    let next = receive
        .receive(&mut &client.stream)
        .await?
        .ok_or("window update did not resume stream")?;
    assert_eq!(next.kind, FrameKind::Data);
    client
        .codec
        .send(
            &mut &client.stream,
            FrameKind::WindowUpdate,
            7,
            2,
            &encode_window_update(4096)?,
        )
        .await?;
    loop {
        let frame = receive
            .receive(&mut &client.stream)
            .await?
            .ok_or("flow-controlled response ended before completion")?;
        if frame.kind == FrameKind::End {
            break;
        }
    }
    drop(client);
    let _product = daemon.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn missing_completion_is_rejected() -> Result<(), Box<dyn Error>> {
    let mut provisional = ProvisionalStream::new();
    provisional.push(b"rows", 64)?;
    assert!(provisional.reject_incomplete().is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_and_deadline_frames_reach_product_errors() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("cancel-deadline")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect(&test.socket, 64 * 1024).await?;

    client
        .codec
        .send(
            &mut &client.stream,
            FrameKind::Cancel,
            3,
            10,
            &encode_cancel(1),
        )
        .await?;
    client
        .send_request(3, 10, &request(ProductOperation::Capabilities))
        .await?;
    let Err(cancelled) = client.response(3, 10).await else {
        return Err("cancelled request completed".into());
    };
    assert!(cancelled.to_string().contains("cancelled"));

    client
        .codec
        .send(
            &mut &client.stream,
            FrameKind::Deadline,
            4,
            11,
            &encode_deadline(1)?,
        )
        .await?;
    client
        .send_request(4, 11, &request(ProductOperation::Capabilities))
        .await?;
    let Err(deadline) = client.response(4, 11).await else {
        return Err("expired request completed".into());
    };
    assert!(deadline.to_string().contains("deadline"));

    drop(client);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_clients_are_connection_local() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("malformed-isolation")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;

    let path = test.socket.to_string_lossy();
    let name = path.to_fs_name::<GenericFilePath>()?;
    let malformed = Stream::connect(name).await?;
    let mut encoded = encode_frame(FrameKind::Hello, 0, 1, b"malformed", 16 * 1024 * 1024)?;
    encoded[28] ^= 0xff;
    tokio::io::AsyncWriteExt::write_all(&mut &malformed, &encoded).await?;
    drop(malformed);

    let wrong_handshake = Stream::connect(path.to_fs_name::<GenericFilePath>()?).await?;
    let codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
    codec
        .send(&mut &wrong_handshake, FrameKind::Ping, 0, 2, b"not-hello")
        .await?;
    drop(wrong_handshake);

    let malformed_hello = Stream::connect(path.to_fs_name::<GenericFilePath>()?).await?;
    codec
        .send(
            &mut &malformed_hello,
            FrameKind::Hello,
            0,
            3,
            b"malformed-hello",
        )
        .await?;
    drop(malformed_hello);

    let malformed_control = Client::connect(&test.socket, 64 * 1024).await?;
    malformed_control
        .codec
        .send(
            &mut &malformed_control.stream,
            FrameKind::WindowUpdate,
            99,
            99,
            b"malformed",
        )
        .await?;
    drop(malformed_control);

    let malformed_request = Client::connect(&test.socket, 64 * 1024).await?;
    malformed_request
        .codec
        .send(
            &mut &malformed_request.stream,
            FrameKind::Execute,
            98,
            98,
            b"malformed-request",
        )
        .await?;
    let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
    let failure = receive
        .receive(&mut &malformed_request.stream)
        .await?
        .ok_or("missing malformed-request failure")?;
    assert_eq!(failure.kind, FrameKind::Failure);
    drop(malformed_request);

    let disconnected = Client::connect(&test.socket, 64 * 1024).await?;
    disconnected
        .send_request(3, 30, &request(ProductOperation::Capabilities))
        .await?;
    drop(disconnected);

    let healthy = Client::connect(&test.socket, 64 * 1024).await?;
    healthy
        .send_request(4, 31, &request(ProductOperation::Capabilities))
        .await?;
    assert!(matches!(
        healthy.response(4, 31).await?,
        ProductResponse::Capabilities(_)
    ));
    drop(healthy);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_controls_are_bounded_and_negotiated_frame_limit_is_enforced()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("connection-bounds")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect_with_hello(
        &test.socket,
        Hello {
            maximum_frame_payload: 256,
            maximum_in_flight: 2,
            ..Hello::default()
        },
    )
    .await?;

    for request_id in 40..43 {
        client
            .codec
            .send(
                &mut &client.stream,
                FrameKind::Deadline,
                u32::try_from(request_id)?,
                request_id,
                &encode_deadline(1)?,
            )
            .await?;
    }
    let mut receive = AsyncFrameIo::new(256)?;
    let bounded =
        tokio::time::timeout(Duration::from_secs(1), receive.receive(&mut &client.stream))
            .await??
            .ok_or("missing bounded-control failure")?;
    assert_eq!(bounded.kind, FrameKind::Failure);
    assert_eq!(bounded.request_id, 42);
    drop(client);

    let oversized = Client::connect_with_hello(
        &test.socket,
        Hello {
            maximum_frame_payload: 256,
            ..Hello::default()
        },
    )
    .await?;
    let encoded = encode_frame(FrameKind::Ping, 1, 50, &[0; 257], 16 * 1024 * 1024)?;
    tokio::io::AsyncWriteExt::write_all(&mut &oversized.stream, &encoded).await?;
    let mut oversized_receive = AsyncFrameIo::new(256)?;
    let closed = tokio::time::timeout(
        Duration::from_secs(1),
        oversized_receive.receive(&mut &oversized.stream),
    )
    .await;
    assert!(matches!(closed, Ok(Ok(None) | Err(_))));
    drop(oversized);

    let healthy = Client::connect(&test.socket, 64 * 1024).await?;
    healthy
        .send_request(5, 51, &request(ProductOperation::Capabilities))
        .await?;
    assert!(matches!(
        healthy.response(5, 51).await?,
        ProductResponse::Capabilities(_)
    ));
    drop(healthy);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_active_request_or_stream_closes_only_that_connection()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("duplicate-active")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let duplicate = Client::connect(&test.socket, 1).await?;
    duplicate
        .send_request(7, 60, &request(ProductOperation::Capabilities))
        .await?;
    duplicate
        .send_request(8, 60, &request(ProductOperation::Capabilities))
        .await?;
    drop(duplicate);

    let duplicate_stream = Client::connect(&test.socket, 1).await?;
    duplicate_stream
        .send_request(10, 62, &request(ProductOperation::Capabilities))
        .await?;
    duplicate_stream
        .send_request(10, 63, &request(ProductOperation::Capabilities))
        .await?;
    drop(duplicate_stream);

    let healthy = Client::connect(&test.socket, 64 * 1024).await?;
    healthy
        .send_request(9, 61, &request(ProductOperation::Capabilities))
        .await?;
    assert!(matches!(
        healthy.response(9, 61).await?,
        ProductResponse::Capabilities(_)
    ));
    drop(healthy);
    daemon.shutdown().await?;
    Ok(())
}
