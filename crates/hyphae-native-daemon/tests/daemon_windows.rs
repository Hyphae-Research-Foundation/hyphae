// SPDX-License-Identifier: AGPL-3.0-only

//! Functional Windows named-pipe security, isolation, and outcome tests.

#![cfg(windows)]

use std::{
    error::Error,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig, connect};
use hyphae_native_product::{
    ApiKeyId, AuthenticatedAuthority, BuiltInRole, MetricId, MetricValue, NativeProduct,
    NativeProductService, NativeProductServiceConfig, ProductAuthorization,
    ProductDurabilityPolicy, ProductErrorCode, ProductLimits, ProductOperation, ProductResponse,
    ProductScope, TelemetryRegistry,
};
use hyphae_native_protocol::{
    AsyncFrameIo, FrameKind, Hello, OwnedFrame, ProtocolCapabilities, ProvisionalStream,
    WireRequest, decode_end, decode_failure, decode_welcome, encode_authenticated_hello,
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
    negotiated_minor: u16,
}

impl Client {
    async fn connect(endpoint: &str) -> Result<Self, Box<dyn Error>> {
        let hello = Hello::default();
        let payload = encode_hello(&hello)?;
        Self::connect_with_payload(endpoint, &hello, &payload).await
    }

    async fn connect_authenticated(endpoint: &str, api_key: &str) -> Result<Self, Box<dyn Error>> {
        let hello = Hello {
            capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            ..Hello::default()
        };
        let payload = encode_authenticated_hello(&hello, api_key)?;
        Self::connect_with_payload(endpoint, &hello, &payload).await
    }

    async fn connect_with_payload(
        endpoint: &str,
        hello: &Hello,
        payload: &[u8],
    ) -> Result<Self, Box<dyn Error>> {
        let stream = connect(endpoint).await?;
        let mut codec = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
        codec
            .send(&mut &stream, FrameKind::Hello, 0, 1, payload)
            .await?;
        let welcome = codec
            .receive(&mut &stream)
            .await?
            .ok_or("server closed before WELCOME")?;
        assert_eq!(welcome.kind, FrameKind::Welcome);
        let welcome = decode_welcome(&welcome.payload)?;
        assert_eq!(welcome.initial_window, hello.initial_window);
        if hello
            .required_capabilities
            .contains(ProtocolCapabilities::API_KEY_AUTH)
        {
            assert!(
                welcome
                    .capabilities
                    .contains(ProtocolCapabilities::API_KEY_AUTH)
            );
            assert_eq!(welcome.catalog_version, 0);
        }
        codec = AsyncFrameIo::new(usize::try_from(welcome.maximum_frame_payload)?)?;
        Ok(Self {
            stream,
            codec,
            negotiated_minor: welcome.minor,
        })
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

    async fn failure_code(
        &self,
        stream_id: u32,
        request_id: u64,
    ) -> Result<ProductErrorCode, Box<dyn Error>> {
        let mut receive = AsyncFrameIo::new(self.codec.maximum_payload())?;
        let frame = receive
            .receive(&mut &self.stream)
            .await?
            .ok_or("stream ended before FAILURE")?;
        assert_eq!(
            (frame.kind, frame.stream_id, frame.request_id),
            (FrameKind::Failure, stream_id, request_id)
        );
        Ok(decode_failure(&frame.payload)?.code())
    }
}

async fn handshake_response(endpoint: &str, payload: &[u8]) -> Result<OwnedFrame, Box<dyn Error>> {
    let stream = connect(endpoint).await?;
    let codec = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
    codec
        .send(&mut &stream, FrameKind::Hello, 0, 1, payload)
        .await?;
    let mut receive = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
    receive
        .receive(&mut &stream)
        .await?
        .ok_or_else(|| "server closed before handshake response".into())
}

struct ManagedReaderFixture {
    product: NativeProduct,
    owner: AuthenticatedAuthority,
    reader_secret: String,
    reader_key_id: ApiKeyId,
}

fn managed_reader_product(test: &TestDirectory) -> Result<ManagedReaderFixture, Box<dyn Error>> {
    let owner_path = test.root.join("owner.key");
    let reader_path = test.root.join("reader.key");
    let mut product = NativeProduct::create(&test.data)?;
    product.migration_store_public_entries(&[(b"shared".to_vec(), b"value".to_vec())])?;
    product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
    let owner_secret = std::fs::read_to_string(&owner_path)?;
    let owner = product.authenticate_api_key(&owner_secret, 2)?;
    let reader = product.create_security_principal(&owner, "Reader", 2)?;
    let owner = product.authenticate_api_key(&owner_secret, 3)?;
    product.assign_built_in_role(
        &owner,
        reader.principal_id,
        BuiltInRole::Reader,
        ProductScope::Instance,
        3,
    )?;
    let owner = product.authenticate_api_key(&owner_secret, 4)?;
    product.set_security_principal_enabled(&owner, reader.principal_id, true, 4)?;
    let owner = product.authenticate_api_key(&owner_secret, 5)?;
    let issued = product.issue_api_key_to_file(
        &owner,
        reader.principal_id,
        "reader",
        [BuiltInRole::Reader],
        ProductAuthorization::from_permissions([
            hyphae_native_product::ProductPermission::DataRead,
        ]),
        None,
        &reader_path,
        5,
    )?;
    Ok(ManagedReaderFixture {
        product,
        owner,
        reader_secret: std::fs::read_to_string(reader_path)?,
        reader_key_id: issued.key_id,
    })
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

fn request_count(telemetry: &TelemetryRegistry) -> Result<u64, Box<dyn Error>> {
    let snapshot = telemetry.snapshot(0, None);
    let row = snapshot
        .metrics
        .into_iter()
        .find(|row| row.descriptor.id == MetricId::Requests)
        .ok_or("request metric is missing")?;
    match row.value {
        MetricValue::Counter(value) => Ok(value),
        _ => Err("request metric is not a counter".into()),
    }
}

async fn verify_bootstrapped_default_handshake(
    endpoint: &str,
    reader_secret: &str,
) -> Result<(), Box<dyn Error>> {
    let legacy = handshake_response(endpoint, &encode_hello(&Hello::default())?).await?;
    if legacy.kind != FrameKind::Failure {
        return Err("bootstrapped default named pipe accepted a legacy HELLO".into());
    }
    if decode_failure(&legacy.payload)?.code() != ProductErrorCode::AuthorizationDenied {
        return Err("bootstrapped default named pipe returned the wrong legacy denial".into());
    }

    let authenticated = Client::connect_authenticated(endpoint, reader_secret).await?;
    authenticated
        .send_request(
            1,
            2,
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
        )
        .await?;
    if authenticated.response(1, 2).await?
        != ProductResponse::StructureValue(Some(b"value".to_vec()))
    {
        return Err("bootstrapped default named pipe rejected a valid API key".into());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_named_pipe_requires_api_key_after_access_control_bootstrap()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("default-managed-start")?;
    let fixture = managed_reader_product(&test)?;
    let daemon = NativeDaemon::start(
        fixture.product,
        &test.endpoint,
        NativeDaemonConfig::default(),
    )?;
    let verification =
        verify_bootstrapped_default_handshake(&test.endpoint, &fixture.reader_secret).await;
    let shutdown = daemon.shutdown().await;
    verification?;
    drop(shutdown?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_named_pipe_with_service_requires_api_key_after_access_control_bootstrap()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("default-managed-service")?;
    let fixture = managed_reader_product(&test)?;
    let service =
        NativeProductService::start(fixture.product, NativeProductServiceConfig::default())?;
    let daemon =
        NativeDaemon::start_with_service(service, &test.endpoint, NativeDaemonConfig::default())?;
    let verification =
        verify_bootstrapped_default_handshake(&test.endpoint, &fixture.reader_secret).await;
    let shutdown = daemon.shutdown().await;
    verification?;
    drop(shutdown?);
    Ok(())
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
async fn security_operations_require_their_minor_on_named_pipes() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("security-minor")?;
    let fixture = managed_reader_product(&test)?;
    let owner_secret = std::fs::read_to_string(test.root.join("owner.key"))?;
    let telemetry = fixture.product.telemetry().clone();
    let daemon = NativeDaemon::start_authenticated(
        fixture.product,
        &test.endpoint,
        NativeDaemonConfig::default(),
    )?;

    let current = Client::connect_authenticated(&test.endpoint, &owner_secret).await?;
    assert_eq!(current.negotiated_minor, 2);
    current
        .send_request(1, 2, ProductOperation::SecurityStatus)
        .await?;
    assert!(matches!(
        current.response(1, 2).await?,
        ProductResponse::SecurityStatus(_)
    ));
    let requests_after_current = request_count(&telemetry)?;

    let legacy_hello = Hello {
        maximum_minor: 0,
        capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        ..Hello::default()
    };
    let legacy_payload = encode_authenticated_hello(&legacy_hello, &owner_secret)?;
    let legacy =
        Client::connect_with_payload(&test.endpoint, &legacy_hello, &legacy_payload).await?;
    assert_eq!(legacy.negotiated_minor, 0);
    legacy
        .send_request(1, 3, ProductOperation::SecurityStatus)
        .await?;
    assert_eq!(
        legacy.failure_code(1, 3).await?,
        ProductErrorCode::InvalidRequest
    );
    assert_eq!(request_count(&telemetry)?, requests_after_current);

    let minor_one_hello = Hello {
        maximum_minor: 1,
        capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        ..Hello::default()
    };
    let minor_one_payload = encode_authenticated_hello(&minor_one_hello, &owner_secret)?;
    let minor_one =
        Client::connect_with_payload(&test.endpoint, &minor_one_hello, &minor_one_payload).await?;
    assert_eq!(minor_one.negotiated_minor, 1);
    let mut mutation = request(ProductOperation::SecurityPrincipalCreate {
        display_name: "minor two only".to_owned(),
    });
    mutation.idempotency_token = Some(17);
    minor_one
        .codec
        .send(
            &mut &minor_one.stream,
            FrameKind::Execute,
            2,
            4,
            &encode_product_request(&mutation)?,
        )
        .await?;
    assert_eq!(
        minor_one.failure_code(2, 4).await?,
        ProductErrorCode::InvalidRequest
    );
    assert_eq!(request_count(&telemetry)?, requests_after_current);

    drop(current);
    drop(legacy);
    drop(minor_one);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_named_pipe_authenticates_and_revalidates_revocation() -> Result<(), Box<dyn Error>>
{
    let test = TestDirectory::new("managed-revocation")?;
    let fixture = managed_reader_product(&test)?;
    let service =
        NativeProductService::start(fixture.product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let daemon = NativeDaemon::start_with_service_authenticated(
        service,
        &test.endpoint,
        NativeDaemonConfig::default(),
    )?;

    let legacy = handshake_response(&test.endpoint, &encode_hello(&Hello::default())?).await?;
    assert_eq!(legacy.kind, FrameKind::Failure);
    assert_eq!(
        decode_failure(&legacy.payload)?.code(),
        ProductErrorCode::AuthorizationDenied
    );

    let client = Client::connect_authenticated(&test.endpoint, &fixture.reader_secret).await?;
    client
        .send_request(
            1,
            2,
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
        )
        .await?;
    assert_eq!(
        client.response(1, 2).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    handle.revoke_api_key(fixture.owner, fixture.reader_key_id, 5)?;
    client
        .send_request(
            2,
            3,
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
        )
        .await?;
    let mut receive = AsyncFrameIo::new(client.codec.maximum_payload())?;
    let denied = receive
        .receive(&mut &client.stream)
        .await?
        .ok_or("server closed after credential revocation")?;
    assert_eq!(
        (denied.kind, denied.stream_id, denied.request_id),
        (FrameKind::Failure, 2, 3)
    );
    assert_eq!(
        decode_failure(&denied.payload)?.code(),
        ProductErrorCode::AuthorizationDenied
    );

    drop(client);
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
