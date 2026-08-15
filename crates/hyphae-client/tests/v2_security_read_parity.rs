// SPDX-License-Identifier: AGPL-3.0-only

//! Managed security read-plane parity across local protocol and HTTP `/v2/execute`.

#![cfg(unix)]

use std::{error::Error, fs, path::PathBuf, time::SystemTime};

use hyphae_client::v2::{
    ClientError, HttpTransport, HyphaeClient, ProductOperation, ProductResponse, RequestOptions,
    SecurityAssignmentListRequest, SecurityAuditReadRequest, SecurityKeyListRequest,
    SecurityPrincipalListRequest, SecurityRoleListRequest,
};
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    ApiKeyId, AuthenticatedAuthority, BuiltInRole, MetricId, MetricValue, NativeProduct,
    NativeProductService, NativeProductServiceConfig, ProductDurabilityPolicy, ProductErrorCode,
    ProductLimits, ProductScope, TelemetryRegistry,
};
use hyphae_server::{NativeHttpV2Config, NativeHttpV2Server};

struct TestDirectory {
    root: PathBuf,
    data: PathBuf,
    endpoint: PathBuf,
}

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hyphae-sdk-security-parity-{}-{suffix}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(Self {
            data: root.join("data"),
            endpoint: PathBuf::from("/tmp").join(format!(
                "hyphae-security-{}-{suffix:x}.sock",
                std::process::id()
            )),
            root,
        })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_file(&self.endpoint);
        let _ignored = fs::remove_dir_all(&self.root);
    }
}

struct ManagedFixture {
    directory: TestDirectory,
    service: NativeProductService,
    handle: hyphae_native_product::NativeProductHandle,
    owner: AuthenticatedAuthority,
    credential: String,
    key_id: ApiKeyId,
    telemetry: TelemetryRegistry,
}

impl ManagedFixture {
    fn create() -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let owner_path = directory.root.join("owner.key");
        let auditor_path = directory.root.join("auditor.key");
        let mut product = NativeProduct::create(&directory.data)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, 2)?;
        let auditor = product.create_security_principal(&owner, "Transport auditor", 2)?;
        let owner = product.authenticate_api_key(&owner_secret, 3)?;
        product.assign_built_in_role(
            &owner,
            auditor.principal_id,
            BuiltInRole::Auditor,
            ProductScope::Instance,
            3,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, 4)?;
        let issued = product.issue_api_key_to_file(
            &owner,
            auditor.principal_id,
            "transport-auditor",
            [BuiltInRole::Auditor],
            BuiltInRole::Auditor.authorization(),
            None,
            &auditor_path,
            4,
        )?;
        let credential = fs::read_to_string(auditor_path)?;
        let owner = product.authenticate_api_key(&owner_secret, 5)?;
        let telemetry = product.telemetry().clone();
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        let handle = service.handle();
        Ok(Self {
            directory,
            service,
            handle,
            owner,
            credential,
            key_id: issued.key_id,
            telemetry,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_security_reads_are_identical_and_revalidate_revocation()
-> Result<(), Box<dyn Error>> {
    let fixture = ManagedFixture::create()?;
    let server = NativeHttpV2Server::new_managed(
        fixture.handle.clone(),
        NativeHttpV2Config {
            bind: "127.0.0.1:0".parse()?,
            ..NativeHttpV2Config::default()
        },
    )?
    .bind()
    .await?;
    let address = server.local_addr();
    let (shutdown_http, http_shutdown) = tokio::sync::oneshot::channel::<()>();
    let serve_http = tokio::spawn(server.run_with_shutdown(async move {
        let _ignored = http_shutdown.await;
    }));
    let daemon = NativeDaemon::start_with_service_authenticated(
        fixture.service,
        fixture.directory.endpoint.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let local = HyphaeClient::local_authenticated(
        fixture.directory.endpoint.to_string_lossy(),
        &fixture.credential,
    )?;
    let http = HyphaeClient::new(
        HttpTransport::new(&format!("http://{address}"))?.bearer_token(&fixture.credential)?,
    );

    let local_status = local.security_status(RequestOptions::default()).await?;
    let http_status = http
        .execute(ProductOperation::SecurityStatus, RequestOptions::default())
        .await?;
    assert_response_parity(&local_status, &http_status)?;
    assert_principal_pages(&local, &http).await?;
    assert_role_pages(&local, &http).await?;
    assert_assignment_pages(&local, &http).await?;
    assert_key_pages(&local, &http).await?;
    assert_audit_pages(&local, &http).await?;
    assert_payload_bearing_retired_shape_is_rejected_before_http_dispatch(
        address,
        &fixture.credential,
        &fixture.telemetry,
    )
    .await?;

    fixture
        .handle
        .revoke_api_key(fixture.owner, fixture.key_id, 6)?;
    assert_authorization_denied(local.security_status(RequestOptions::default()).await);
    assert_authorization_denied(
        http.execute(ProductOperation::SecurityStatus, RequestOptions::default())
            .await,
    );

    drop(local);
    drop(http);
    let _ignored = shutdown_http.send(());
    serve_http.await??;
    drop(daemon.shutdown().await?);
    Ok(())
}

async fn assert_payload_bearing_retired_shape_is_rejected_before_http_dispatch(
    address: std::net::SocketAddr,
    credential: &str,
    telemetry: &TelemetryRegistry,
) -> Result<(), Box<dyn Error>> {
    let request = hyphae_native_protocol::WireRequest {
        operation: ProductOperation::SecurityStatus,
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::MEMORY,
    };
    let mut encoded = hyphae_native_protocol::encode_product_request(&request)?;
    encoded.push(0);
    let encoded_length = u32::try_from(encoded.len())?;
    encoded[8..12].copy_from_slice(&encoded_length.to_le_bytes());
    let requests_before = request_count(telemetry)?;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v2/execute"))
        .bearer_auth(credential)
        .header(
            reqwest::header::CONTENT_TYPE,
            hyphae_contracts::v2::PRODUCT_MEDIA_TYPE_V2,
        )
        .header(
            reqwest::header::ACCEPT,
            hyphae_contracts::v2::PRODUCT_ERROR_MEDIA_TYPE_V2,
        )
        .header(hyphae_contracts::v2::REQUEST_ID_HEADER_V2, "9001")
        .body(encoded)
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(request_count(telemetry)?, requests_before);
    Ok(())
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

async fn assert_principal_pages(
    local: &HyphaeClient,
    http: &HyphaeClient,
) -> Result<(), Box<dyn Error>> {
    let first_request = SecurityPrincipalListRequest::new(None, 1)?;
    let first = local
        .security_principal_list(first_request, RequestOptions::default())
        .await?;
    let remote = http
        .execute(
            ProductOperation::SecurityPrincipalList(first_request),
            RequestOptions::default(),
        )
        .await?;
    assert_response_parity(&first, &remote)?;
    let ProductResponse::SecurityPrincipalPage(first) = first else {
        return Err("principal list returned the wrong response".into());
    };
    let continuation = SecurityPrincipalListRequest::new(first.next_cursor, 1)?;
    let local = local
        .execute(
            ProductOperation::SecurityPrincipalList(continuation),
            RequestOptions::default(),
        )
        .await?;
    let remote = http
        .security_principal_list(continuation, RequestOptions::default())
        .await?;
    assert_response_parity(&local, &remote)
}

async fn assert_role_pages(
    local: &HyphaeClient,
    http: &HyphaeClient,
) -> Result<(), Box<dyn Error>> {
    let first_request = SecurityRoleListRequest::new(None, 1)?;
    let first = local
        .security_role_list(first_request, RequestOptions::default())
        .await?;
    let remote = http
        .execute(
            ProductOperation::SecurityRoleList(first_request),
            RequestOptions::default(),
        )
        .await?;
    assert_response_parity(&first, &remote)?;
    let ProductResponse::SecurityRolePage(first) = first else {
        return Err("role list returned the wrong response".into());
    };
    let continuation = SecurityRoleListRequest::new(first.next_cursor, 1)?;
    let local = local
        .execute(
            ProductOperation::SecurityRoleList(continuation),
            RequestOptions::default(),
        )
        .await?;
    let remote = http
        .security_role_list(continuation, RequestOptions::default())
        .await?;
    assert_response_parity(&local, &remote)
}

async fn assert_assignment_pages(
    local: &HyphaeClient,
    http: &HyphaeClient,
) -> Result<(), Box<dyn Error>> {
    let first_request = SecurityAssignmentListRequest::new(None, 1)?;
    let first = local
        .security_assignment_list(first_request, RequestOptions::default())
        .await?;
    let remote = http
        .execute(
            ProductOperation::SecurityAssignmentList(first_request),
            RequestOptions::default(),
        )
        .await?;
    assert_response_parity(&first, &remote)?;
    let ProductResponse::SecurityAssignmentPage(first) = first else {
        return Err("assignment list returned the wrong response".into());
    };
    let continuation = SecurityAssignmentListRequest::new(first.next_cursor, 1)?;
    let local = local
        .execute(
            ProductOperation::SecurityAssignmentList(continuation),
            RequestOptions::default(),
        )
        .await?;
    let remote = http
        .security_assignment_list(continuation, RequestOptions::default())
        .await?;
    assert_response_parity(&local, &remote)
}

async fn assert_key_pages(local: &HyphaeClient, http: &HyphaeClient) -> Result<(), Box<dyn Error>> {
    let first_request = SecurityKeyListRequest::new(None, 1)?;
    let first = local
        .security_key_list(first_request, RequestOptions::default())
        .await?;
    let remote = http
        .execute(
            ProductOperation::SecurityKeyList(first_request),
            RequestOptions::default(),
        )
        .await?;
    assert_response_parity(&first, &remote)?;
    let ProductResponse::SecurityKeyPage(first) = first else {
        return Err("key list returned the wrong response".into());
    };
    let continuation = SecurityKeyListRequest::new(first.next_cursor, 1)?;
    let local = local
        .execute(
            ProductOperation::SecurityKeyList(continuation),
            RequestOptions::default(),
        )
        .await?;
    let remote = http
        .security_key_list(continuation, RequestOptions::default())
        .await?;
    assert_response_parity(&local, &remote)
}

async fn assert_audit_pages(
    local: &HyphaeClient,
    http: &HyphaeClient,
) -> Result<(), Box<dyn Error>> {
    let first_request = SecurityAuditReadRequest::new(None, 1)?;
    let first = local
        .security_audit_read(first_request, RequestOptions::default())
        .await?;
    let remote = http
        .execute(
            ProductOperation::SecurityAuditRead(first_request),
            RequestOptions::default(),
        )
        .await?;
    assert_response_parity(&first, &remote)?;
    let ProductResponse::SecurityAuditPage(first) = first else {
        return Err("audit read returned the wrong response".into());
    };
    let continuation = SecurityAuditReadRequest::new(first.next_cursor, 1)?;
    let local = local
        .execute(
            ProductOperation::SecurityAuditRead(continuation),
            RequestOptions::default(),
        )
        .await?;
    let remote = http
        .security_audit_read(continuation, RequestOptions::default())
        .await?;
    assert_response_parity(&local, &remote)
}

fn assert_response_parity(
    local: &ProductResponse,
    remote: &ProductResponse,
) -> Result<(), Box<dyn Error>> {
    assert_eq!(local, remote);
    assert_eq!(
        hyphae_native_protocol::encode_product_response(local)?,
        hyphae_native_protocol::encode_product_response(remote)?
    );
    Ok(())
}

fn assert_authorization_denied(result: Result<ProductResponse, ClientError>) {
    assert!(matches!(
        result,
        Err(ClientError::Product(error)) if error.code() == ProductErrorCode::AuthorizationDenied
    ));
}
