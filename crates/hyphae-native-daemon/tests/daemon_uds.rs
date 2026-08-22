// SPDX-License-Identifier: Apache-2.0

//! Multi-client daemon handshake, completion, flow control, and disconnect tests.

#![cfg(unix)]

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use hyphae_native_daemon::{DaemonError, NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    ApiKeyId, BuiltInRole, MetricId, MetricValue, NativeProduct, NativeProductService,
    NativeProductServiceConfig, ProductAuthorization, ProductDurabilityPolicy, ProductErrorCode,
    ProductLimits, ProductOperation, ProductResponse, ProductScope, TelemetryRegistry,
};
use hyphae_native_protocol::{
    AsyncFrameIo, FrameKind, Hello, OwnedFrame, ProtocolCapabilities, ProvisionalStream,
    WireRequest, decode_end, decode_failure, decode_welcome, encode_authenticated_hello,
    encode_cancel, encode_deadline, encode_frame, encode_hello, encode_product_request,
    encode_window_update,
};
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::{Stream, prelude::*};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
const TEST_IO_TIMEOUT: Duration = Duration::from_secs(10);
const ACK_LOSS_INITIAL_WINDOW: u32 = 1;

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
    negotiated_minor: u16,
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
        let payload = encode_hello(&hello)?;
        Self::connect_with_payload(path, hello, &payload).await
    }

    async fn connect_authenticated(path: &Path, api_key: &str) -> Result<Self, Box<dyn Error>> {
        Self::connect_authenticated_for_request(path, api_key, None).await
    }

    async fn connect_authenticated_for_request(
        path: &Path,
        api_key: &str,
        request: Option<&WireRequest>,
    ) -> Result<Self, Box<dyn Error>> {
        let hello = Hello {
            capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            ..Hello::default()
        };
        let payload = encode_authenticated_hello(&hello, api_key)?;
        let terminal = request.map(encode_product_request).transpose()?;
        Self::connect_with_payload_and_request(path, hello, &payload, terminal.as_deref()).await
    }

    async fn connect_authenticated_with_window(
        path: &Path,
        api_key: &str,
        initial_window: u32,
    ) -> Result<Self, Box<dyn Error>> {
        let hello = Hello {
            capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            initial_window,
            ..Hello::default()
        };
        let payload = encode_authenticated_hello(&hello, api_key)?;
        Self::connect_with_payload(path, hello, &payload).await
    }

    async fn connect_with_payload(
        path: &Path,
        hello: Hello,
        payload: &[u8],
    ) -> Result<Self, Box<dyn Error>> {
        Self::connect_with_payload_and_request(path, hello, payload, None).await
    }

    async fn connect_with_payload_and_request(
        path: &Path,
        hello: Hello,
        payload: &[u8],
        request: Option<&[u8]>,
    ) -> Result<Self, Box<dyn Error>> {
        let path = path.to_string_lossy();
        let name = path.to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        let mut codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
        codec
            .send(&mut &stream, FrameKind::Hello, 0, 1, payload)
            .await?;
        if let Some(request) = request {
            codec
                .send(&mut &stream, FrameKind::Execute, 1, 2, request)
                .await?;
        }
        let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
        let welcome = receive
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

    /// Receives response `DATA` while leaving terminal `END` unread.
    async fn response_data_started(
        &self,
        stream_id: u32,
        request_id: u64,
    ) -> Result<(), Box<dyn Error>> {
        let mut receive = AsyncFrameIo::new(self.codec.maximum_payload())?;
        let frame = receive
            .receive(&mut &self.stream)
            .await?
            .ok_or("stream ended before response DATA")?;
        assert_eq!(
            (frame.kind, frame.stream_id, frame.request_id),
            (FrameKind::Data, stream_id, request_id)
        );
        Ok(())
    }

    async fn failure_code(
        &self,
        stream_id: u32,
        request_id: u64,
    ) -> Result<ProductErrorCode, Box<dyn Error>> {
        let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
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

async fn handshake_response(path: &Path, payload: &[u8]) -> Result<OwnedFrame, Box<dyn Error>> {
    let path = path.to_string_lossy();
    let name = path.to_fs_name::<GenericFilePath>()?;
    let stream = Stream::connect(name).await?;
    let codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
    codec
        .send(&mut &stream, FrameKind::Hello, 0, 1, payload)
        .await?;
    let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
    receive
        .receive(&mut &stream)
        .await?
        .ok_or_else(|| "server closed before handshake response".into())
}

struct ManagedReaderFixture {
    product: NativeProduct,
    owner_secret: String,
    reader_secret: String,
    reader_key_id: ApiKeyId,
}

fn managed_reader_product(test: &TestDirectory) -> Result<ManagedReaderFixture, Box<dyn Error>> {
    let owner_path = test.root.join("owner.key");
    let reader_path = test.root.join("reader.key");
    let mut product = NativeProduct::create(&test.data)?;
    product.migration_store_public_entries(&[(b"shared".to_vec(), b"value".to_vec())])?;
    product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
    let owner_secret = fs::read_to_string(&owner_path)?;
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
        owner_secret,
        reader_secret: fs::read_to_string(reader_path)?,
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
    endpoint: &Path,
    reader_secret: &str,
) -> Result<(), Box<dyn Error>> {
    let legacy = handshake_response(endpoint, &encode_hello(&Hello::default())?).await?;
    if legacy.kind != FrameKind::Failure {
        return Err("bootstrapped default daemon accepted a legacy HELLO".into());
    }
    if decode_failure(&legacy.payload)?.code() != ProductErrorCode::AuthorizationDenied {
        return Err("bootstrapped default daemon returned the wrong legacy denial".into());
    }

    let authenticated = Client::connect_authenticated(endpoint, reader_secret).await?;
    authenticated
        .send_request(
            1,
            2,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    if authenticated.response(1, 2).await?
        != ProductResponse::StructureValue(Some(b"value".to_vec()))
    {
        return Err("bootstrapped default daemon rejected a valid API key".into());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_start_requires_api_key_after_access_control_bootstrap()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("default-managed-start")?;
    let fixture = managed_reader_product(&test)?;
    let daemon = NativeDaemon::start(
        fixture.product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let verification =
        verify_bootstrapped_default_handshake(&test.socket, &fixture.reader_secret).await;
    let shutdown = daemon.shutdown().await;
    verification?;
    drop(shutdown?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_start_with_service_requires_api_key_after_access_control_bootstrap()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("default-managed-service")?;
    let fixture = managed_reader_product(&test)?;
    let service =
        NativeProductService::start(fixture.product, NativeProductServiceConfig::default())?;
    let daemon = NativeDaemon::start_with_service(
        service,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let verification =
        verify_bootstrapped_default_handshake(&test.socket, &fixture.reader_secret).await;
    let shutdown = daemon.shutdown().await;
    verification?;
    drop(shutdown?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_start_preserves_legacy_hello_for_empty_access_control_catalog()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("default-empty-legacy")?;
    let daemon = NativeDaemon::start(
        NativeProduct::create(&test.data)?,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let response = handshake_response(&test.socket, &encode_hello(&Hello::default())?).await?;
    if response.kind != FrameKind::Welcome {
        return Err("empty default daemon rejected a legacy HELLO".into());
    }
    let welcome = decode_welcome(&response.payload)?;
    assert!(
        !welcome
            .capabilities
            .contains(ProtocolCapabilities::API_KEY_AUTH)
    );
    drop(daemon.shutdown().await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_unmanaged_start_rejects_bootstrapped_access_control()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("acceptance-managed-denial")?;
    let fixture = managed_reader_product(&test)?;
    let service =
        NativeProductService::start(fixture.product, NativeProductServiceConfig::default())?;
    match NativeDaemon::start_with_service_for_acceptance(
        service,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
        "denied acceptance identity",
    ) {
        Err(DaemonError::Product(error))
            if error.code() == ProductErrorCode::AuthorizationDenied =>
        {
            Ok(())
        }
        Err(error) => Err(format!("acceptance unmanaged startup returned {error}").into()),
        Ok(daemon) => {
            drop(daemon.shutdown().await?);
            Err("acceptance unmanaged daemon started over a bootstrapped catalog".into())
        }
    }
}

#[test]
fn response_completion_does_not_depend_on_the_blocking_pool() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let test = TestDirectory::new("async-completion")?;
        let daemon = NativeDaemon::start(
            NativeProduct::create(&test.data)?,
            test.socket.to_string_lossy(),
            NativeDaemonConfig::default(),
        )?;
        let client = Client::connect(&test.socket, 64 * 1024).await?;

        let blocking_started = Arc::new(Barrier::new(2));
        let worker_started = Arc::clone(&blocking_started);
        let (release, released) = mpsc::sync_channel(0);
        let blocking_worker = tokio::task::spawn_blocking(move || {
            worker_started.wait();
            released.recv()
        });
        blocking_started.wait();

        client
            .send_request(1, 2, &request(ProductOperation::Capabilities))
            .await?;
        let response = tokio::time::timeout(Duration::from_secs(2), client.response(1, 2)).await;

        release.send(())?;
        blocking_worker.await??;
        let response = response.map_err(|_| "response waited for a blocking worker")??;
        assert!(matches!(response, ProductResponse::Capabilities(_)));

        drop(client);
        daemon.shutdown().await?;
        Ok::<(), Box<dyn Error>>(())
    })
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
async fn managed_missing_malformed_and_wrong_credentials_are_uniform() -> Result<(), Box<dyn Error>>
{
    let test = TestDirectory::new("managed-auth-denial")?;
    let fixture = managed_reader_product(&test)?;
    let daemon = NativeDaemon::start_authenticated(
        fixture.product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let authenticated_hello = Hello {
        capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        ..Hello::default()
    };
    let wrong = format!("hyp1_{}_{}", "0".repeat(32), "0".repeat(64));
    let wrong_namespace_hello = Hello {
        database: "private".to_owned(),
        ..authenticated_hello.clone()
    };
    let payloads = [
        encode_hello(&Hello::default())?,
        encode_authenticated_hello(&authenticated_hello, &"x".repeat(102))?,
        encode_authenticated_hello(&authenticated_hello, &wrong)?,
        encode_authenticated_hello(&wrong_namespace_hello, &wrong)?,
    ];
    let mut errors = Vec::new();
    for payload in payloads {
        let path = test.socket.to_string_lossy();
        let stream = Stream::connect(path.to_fs_name::<GenericFilePath>()?).await?;
        let mut codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
        codec
            .send(&mut &stream, FrameKind::Hello, 0, 1, &payload)
            .await?;
        let response = codec
            .receive(&mut &stream)
            .await?
            .ok_or("server closed before authentication denial")?;
        assert_eq!(response.kind, FrameKind::Failure);
        errors.push(decode_failure(&response.payload)?);
    }
    for error in &errors {
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
        assert_eq!(error.category(), errors[0].category());
        assert_eq!(error.retry(), errors[0].retry());
        assert_eq!(error.message(), errors[0].message());
        assert_eq!(error.transaction_state(), errors[0].transaction_state());
        assert_eq!(error.details(), errors[0].details());
    }

    let healthy = Client::connect_authenticated(&test.socket, &fixture.reader_secret).await?;
    healthy
        .send_request(
            1,
            2,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    assert_eq!(
        healthy.response(1, 2).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );
    drop(healthy);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_reader_reads_and_denied_write_does_not_poison_connection()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("managed-reader")?;
    let fixture = managed_reader_product(&test)?;
    let daemon = NativeDaemon::start_authenticated(
        fixture.product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect_authenticated(&test.socket, &fixture.reader_secret).await?;

    client
        .send_request(
            1,
            2,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    assert_eq!(
        client.response(1, 2).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    client
        .send_request(
            2,
            3,
            &request(ProductOperation::StructureSet {
                key: b"shared".to_vec(),
                value: b"forbidden".to_vec(),
                expires_at_micros: None,
            }),
        )
        .await?;
    let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
    let denied = receive
        .receive(&mut &client.stream)
        .await?
        .ok_or("server closed after authorization denial")?;
    assert_eq!(
        (denied.kind, denied.stream_id, denied.request_id),
        (FrameKind::Failure, 2, 3)
    );
    assert_eq!(
        decode_failure(&denied.payload)?.code(),
        ProductErrorCode::AuthorizationDenied
    );

    client
        .send_request(
            3,
            4,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    assert_eq!(
        client.response(3, 4).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );
    drop(client);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn security_operations_require_their_minor_and_reject_retired_shapes_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("security-minor")?;
    let fixture = managed_reader_product(&test)?;
    let owner_secret = fs::read_to_string(test.root.join("owner.key"))?;
    let telemetry = fixture.product.telemetry().clone();
    let daemon = NativeDaemon::start_authenticated(
        fixture.product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;

    let current = Client::connect_authenticated(&test.socket, &owner_secret).await?;
    assert_eq!(current.negotiated_minor, 4);
    current
        .send_request(1, 2, &request(ProductOperation::SecurityStatus))
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
    let legacy = Client::connect_with_payload(&test.socket, legacy_hello, &legacy_payload).await?;
    assert_eq!(legacy.negotiated_minor, 0);
    legacy
        .send_request(1, 3, &request(ProductOperation::SecurityStatus))
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
        Client::connect_with_payload(&test.socket, minor_one_hello, &minor_one_payload).await?;
    assert_eq!(minor_one.negotiated_minor, 1);
    let mut mutation = request(ProductOperation::SecurityPrincipalCreate {
        display_name: "minor two only".to_owned(),
    });
    mutation.idempotency_token = Some(17);
    minor_one.send_request(2, 4, &mutation).await?;
    assert_eq!(
        minor_one.failure_code(2, 4).await?,
        ProductErrorCode::InvalidRequest
    );
    assert_eq!(request_count(&telemetry)?, requests_after_current);

    let mut retired = encode_product_request(&request(ProductOperation::SecurityStatus))?;
    retired.push(0);
    let retired_length = u32::try_from(retired.len())?;
    retired[8..12].copy_from_slice(&retired_length.to_le_bytes());
    current
        .codec
        .send(&mut &current.stream, FrameKind::Execute, 2, 4, &retired)
        .await?;
    assert_eq!(
        current.failure_code(2, 4).await?,
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
async fn managed_revocation_denies_the_next_operation_on_the_same_connection()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("managed-revocation")?;
    let fixture = managed_reader_product(&test)?;
    let service =
        NativeProductService::start(fixture.product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let daemon = NativeDaemon::start_with_service_authenticated(
        service,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = Client::connect_authenticated(&test.socket, &fixture.reader_secret).await?;
    client
        .send_request(
            1,
            2,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    assert_eq!(
        client.response(1, 2).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    let owner = handle.open_authenticated_session(hyphae_native_product::ApiKeyCredential::new(
        &fixture.owner_secret,
    )?)?;
    owner.dispatch(
        owner.request_context(99, 5).with_idempotency_token(99),
        ProductOperation::SecurityApiKeyRevoke {
            key_id: fixture.reader_key_id,
        },
    )?;
    client
        .send_request(
            2,
            3,
            &request(ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            }),
        )
        .await?;
    let mut receive = AsyncFrameIo::new(16 * 1024 * 1024)?;
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
async fn fresh_connection_replays_self_revoke_after_ack_loss() -> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("self-revoke-reconnect")?;
    let fixture = managed_reader_product(&test)?;
    let actor_secret = fixture.owner_secret.clone();
    let actor = fixture.product.authenticate_api_key(&actor_secret, 0)?;
    let actor_key_id = actor.key_id();
    let daemon = NativeDaemon::start_authenticated(
        fixture.product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let mut revoke = request(ProductOperation::SecurityApiKeyRevokeSelf {
        key_id: actor_key_id,
    });
    revoke.idempotency_token = Some(0x7a01);
    revoke.durability = ProductDurabilityPolicy::STRICT;
    {
        let client = Client::connect_authenticated_with_window(
            &test.socket,
            &actor_secret,
            ACK_LOSS_INITIAL_WINDOW,
        )
        .await?;
        client.send_request(1, 2, &revoke).await?;
        tokio::time::timeout(TEST_IO_TIMEOUT, client.response_data_started(1, 2)).await??;
    }

    let replay = tokio::time::timeout(
        TEST_IO_TIMEOUT,
        Client::connect_authenticated_for_request(&test.socket, &actor_secret, Some(&revoke)),
    )
    .await??;
    let response = tokio::time::timeout(TEST_IO_TIMEOUT, replay.response(1, 2)).await??;
    assert!(matches!(response, ProductResponse::SecurityMutated(_)));
    drop(replay);
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_fresh_connection_has_no_nonterminal_or_mismatch_oracle()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("revoked-no-oracle")?;
    let fixture = managed_reader_product(&test)?;
    let actor_secret = fixture.owner_secret.clone();
    let actor = fixture.product.authenticate_api_key(&actor_secret, 0)?;
    let actor_key_id = actor.key_id();
    let mut product = fixture.product;
    product.revoke_api_key(&actor, actor_key_id, 1)?;
    let daemon = NativeDaemon::start_authenticated(
        product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let mut wrong_replay = request(ProductOperation::SecurityApiKeyRevokeSelf {
        key_id: actor_key_id,
    });
    wrong_replay.idempotency_token = Some(0xdead);
    wrong_replay.durability = ProductDurabilityPolicy::STRICT;
    for body in [request(ProductOperation::Capabilities), wrong_replay] {
        let hello = Hello {
            capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            ..Hello::default()
        };
        let hello_payload = encode_authenticated_hello(&hello, &actor_secret)?;
        let encoded = encode_product_request(&body)?;
        let path = test.socket.to_string_lossy();
        let stream = Stream::connect(path.to_fs_name::<GenericFilePath>()?).await?;
        let mut codec = AsyncFrameIo::new(16 * 1024 * 1024)?;
        codec
            .send(&mut &stream, FrameKind::Hello, 0, 1, &hello_payload)
            .await?;
        codec
            .send(&mut &stream, FrameKind::Execute, 1, 2, &encoded)
            .await?;
        let denied = codec
            .receive(&mut &stream)
            .await?
            .ok_or("server closed before uniform denial")?;
        assert_eq!(
            (denied.kind, denied.stream_id, denied.request_id),
            (FrameKind::Failure, 0, 1)
        );
        assert_eq!(
            decode_failure(&denied.payload)?.code(),
            ProductErrorCode::AuthorizationDenied
        );
    }
    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_connection_replays_zero_overlap_self_rotation_after_ack_loss()
-> Result<(), Box<dyn Error>> {
    let test = TestDirectory::new("self-rotate-reconnect")?;
    let fixture = managed_reader_product(&test)?;
    let predecessor_secret = fixture.owner_secret.clone();
    let mut product = fixture.product;
    let actor = product.authenticate_api_key(&predecessor_secret, 0)?;
    let predecessor_key_id = actor.key_id();
    let started = product.start_api_key_rotation_idempotent(
        &actor,
        predecessor_key_id,
        "zero-overlap",
        0,
        None,
        0x7a02,
        6,
        true,
    )?;
    let successor = started.secret.take().ok_or("missing successor secret")?;
    let successor_key_id = started.key_id;
    let confirmation_digest = successor.confirmation_digest();
    let daemon = NativeDaemon::start_authenticated(
        product,
        test.socket.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let mut activate = request(ProductOperation::SecurityApiKeyRotateSelfActivate {
        successor_key_id,
        confirmation_digest,
    });
    activate.idempotency_token = Some(0x7a03);
    activate.durability = ProductDurabilityPolicy::STRICT;
    {
        let client = Client::connect_authenticated_with_window(
            &test.socket,
            &predecessor_secret,
            ACK_LOSS_INITIAL_WINDOW,
        )
        .await?;
        client.send_request(1, 2, &activate).await?;
        tokio::time::timeout(TEST_IO_TIMEOUT, client.response_data_started(1, 2)).await??;
    }

    let replay = tokio::time::timeout(
        TEST_IO_TIMEOUT,
        Client::connect_authenticated_for_request(
            &test.socket,
            &predecessor_secret,
            Some(&activate),
        ),
    )
    .await??;
    let response = tokio::time::timeout(TEST_IO_TIMEOUT, replay.response(1, 2)).await??;
    assert!(matches!(
        response,
        ProductResponse::SecurityApiKeyActivated(ref receipt)
            if receipt.key_id == successor_key_id
                && receipt.predecessor_key_id == Some(predecessor_key_id)
    ));
    drop(replay);
    daemon.shutdown().await?;
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

    for _ in 0..128 {
        client
            .send_request(1, 2, &request(ProductOperation::Capabilities))
            .await?;
        assert!(matches!(
            client.response(1, 2).await?,
            ProductResponse::Capabilities(_)
        ));
    }

    for _ in 0..128 {
        client
            .codec
            .send(
                &mut &client.stream,
                FrameKind::Cancel,
                2,
                3,
                &encode_cancel(1),
            )
            .await?;
        client
            .send_request(2, 3, &request(ProductOperation::Capabilities))
            .await?;
        let Err(cancelled) = client.response(2, 3).await else {
            return Err("cancelled request completed during stream reuse".into());
        };
        assert!(cancelled.to_string().contains("cancelled"));
    }

    client
        .send_request(2, 3, &request(ProductOperation::Capabilities))
        .await?;
    assert!(matches!(
        client.response(2, 3).await?,
        ProductResponse::Capabilities(_)
    ));

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
