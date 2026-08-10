// SPDX-License-Identifier: GPL-3.0-only

//! Functional Windows named-pipe security, isolation, and outcome tests.

#![cfg(windows)]

use std::{
    error::Error,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig, connect};
use hyphae_native_product::{
    NativeProduct, ProductDurabilityPolicy, ProductLimits, ProductOperation, ProductResponse,
};
use hyphae_native_protocol::{
    AsyncFrameIo, FrameKind, Hello, ProvisionalStream, WireRequest, decode_end, decode_welcome,
    encode_deadline, encode_frame, encode_hello, encode_product_request,
};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::StreamCommon as _;
use tokio::io::AsyncWriteExt as _;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    root: PathBuf,
    data: PathBuf,
    endpoint: String,
}

impl TestDirectory {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let identity = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hnd-windows-{name}-{}-{identity}",
            std::process::id()
        ));
        let _ignored = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root)?;
        Ok(Self {
            data: root.join("data"),
            endpoint: format!("hyphae-{name}-{}-{identity}", std::process::id()),
            root,
        })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.root);
    }
}

struct Client {
    stream: Stream,
    codec: AsyncFrameIo,
}

impl Client {
    async fn connect(endpoint: &str) -> Result<Self, Box<dyn Error>> {
        let stream = connect(endpoint).await?;
        let mut codec = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
        codec
            .send(
                &mut &stream,
                FrameKind::Hello,
                0,
                1,
                &encode_hello(&Hello::default())?,
            )
            .await?;
        let welcome = codec
            .receive(&mut &stream)
            .await?
            .ok_or("server closed before WELCOME")?;
        assert_eq!(welcome.kind, FrameKind::Welcome);
        let welcome = decode_welcome(&welcome.payload)?;
        codec = AsyncFrameIo::new(usize::try_from(welcome.maximum_frame_payload)?)?;
        Ok(Self { stream, codec })
    }

    async fn send_request(
        &self,
        stream_id: u32,
        request_id: u64,
        operation: ProductOperation,
    ) -> Result<(), Box<dyn Error>> {
        self.codec
            .send(
                &mut &self.stream,
                FrameKind::Execute,
                stream_id,
                request_id,
                &encode_product_request(&request(operation))?,
            )
            .await?;
        Ok(())
    }

    async fn response(
        &self,
        stream_id: u32,
        request_id: u64,
    ) -> Result<ProductResponse, Box<dyn Error>> {
        let mut receive = AsyncFrameIo::new(self.codec.maximum_payload())?;
        let mut provisional = ProvisionalStream::new();
        loop {
            let frame = receive
                .receive(&mut &self.stream)
                .await?
                .ok_or("stream ended before terminal response")?;
            assert_eq!((frame.stream_id, frame.request_id), (stream_id, request_id));
            match frame.kind {
                FrameKind::Data => provisional.push(
                    &frame.payload,
                    hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES,
                )?,
                FrameKind::End => {
                    let encoded = provisional.complete(decode_end(&frame.payload)?)?;
                    return Ok(hyphae_native_protocol::decode_product_response(&encoded)?);
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
async fn protected_pipe_supports_peer_identity_and_multiple_owner_clients()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("acl-peer-multi")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        format!(r"\\.\pipe\{}", test.endpoint),
        NativeDaemonConfig::default(),
    )?;
    assert_eq!(daemon.endpoint(), test.endpoint);

    let first = Client::connect(&test.endpoint).await?;
    let second = Client::connect(&format!(r"\\.\pipe\{}", test.endpoint)).await?;
    let process_id = std::process::id();
    assert_eq!(first.stream.peer_creds()?.pid(), Some(process_id));
    assert_eq!(second.stream.peer_creds()?.pid(), Some(process_id));

    first
        .send_request(
            1,
            2,
            ProductOperation::StructureSet {
                key: b"shared".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            },
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
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
        )
        .await?;
    assert_eq!(
        second.response(2, 3).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    drop(first);
    drop(second);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_pipe_client_is_isolated_and_committed_outcome_remains_queryable()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("malformed-outcome")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        &test.endpoint,
        NativeDaemonConfig::default(),
    )?;

    let malformed = connect(&test.endpoint).await?;
    let mut encoded = encode_frame(
        FrameKind::Hello,
        0,
        1,
        b"malformed",
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    encoded[28] ^= 0xff;
    let mut malformed_writer = &malformed;
    malformed_writer.write_all(&encoded).await?;
    drop(malformed);

    let writer = Client::connect(&test.endpoint).await?;
    writer
        .send_request(
            3,
            20,
            ProductOperation::StructureSet {
                key: b"committed".to_vec(),
                value: b"yes".to_vec(),
                expires_at_micros: None,
            },
        )
        .await?;
    let ProductResponse::StructureSet(hyphae_native_product::ProductCommitOutcome::Committed(
        receipt,
    )) = writer.response(3, 20).await?
    else {
        return Err("structure set did not commit".into());
    };
    let transaction_id = receipt.transaction_id;
    drop(writer);

    let resolver = Client::connect(&test.endpoint).await?;
    resolver
        .send_request(
            4,
            21,
            ProductOperation::TransactionStatus { transaction_id },
        )
        .await?;
    assert!(matches!(
        resolver.response(4, 21).await?,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Committed(_)
        )
    ));
    drop(resolver);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_pipe_enforces_negotiated_frame_and_pending_control_bounds()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("bounds")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        &test.endpoint,
        NativeDaemonConfig::default(),
    )?;

    let stream = connect(&test.endpoint).await?;
    let mut handshake = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
    let hello = Hello {
        maximum_frame_payload: 256,
        maximum_in_flight: 1,
        ..Hello::default()
    };
    handshake
        .send(&mut &stream, FrameKind::Hello, 0, 1, &encode_hello(&hello)?)
        .await?;
    let welcome = handshake
        .receive(&mut &stream)
        .await?
        .ok_or("server closed before WELCOME")?;
    assert_eq!(decode_welcome(&welcome.payload)?.maximum_frame_payload, 256);
    let codec = AsyncFrameIo::new(256)?;
    codec
        .send(
            &mut &stream,
            FrameKind::Deadline,
            1,
            10,
            &encode_deadline(1)?,
        )
        .await?;
    codec
        .send(
            &mut &stream,
            FrameKind::Deadline,
            2,
            11,
            &encode_deadline(1)?,
        )
        .await?;
    let mut receive = AsyncFrameIo::new(256)?;
    let failure = receive
        .receive(&mut &stream)
        .await?
        .ok_or("missing pending-control bound failure")?;
    assert_eq!((failure.kind, failure.request_id), (FrameKind::Failure, 11));
    drop(stream);

    let oversized_stream = connect(&test.endpoint).await?;
    let mut oversized_handshake = AsyncFrameIo::new(16 * 1024 * 1024)?;
    let oversized_hello = Hello {
        maximum_frame_payload: 256,
        ..Hello::default()
    };
    oversized_handshake
        .send(
            &mut &oversized_stream,
            FrameKind::Hello,
            0,
            1,
            &encode_hello(&oversized_hello)?,
        )
        .await?;
    let welcome = oversized_handshake
        .receive(&mut &oversized_stream)
        .await?
        .ok_or("server closed before WELCOME")?;
    assert_eq!(decode_welcome(&welcome.payload)?.maximum_frame_payload, 256);
    let encoded = encode_frame(FrameKind::Ping, 3, 12, &[0; 257], 16 * 1024 * 1024)?;
    let mut writer = &oversized_stream;
    writer.write_all(&encoded).await?;
    drop(oversized_stream);

    let healthy = Client::connect(&test.endpoint).await?;
    healthy
        .send_request(4, 13, ProductOperation::Capabilities)
        .await?;
    assert!(matches!(
        healthy.response(4, 13).await?,
        ProductResponse::Capabilities(_)
    ));
    drop(healthy);
    daemon.shutdown().await?;
    Ok(())
}
