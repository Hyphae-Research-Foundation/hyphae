// SPDX-License-Identifier: AGPL-3.0-only

//! Managed security mutation lifecycle across local Native v1.2 and HTTP `/v2/execute`.

#![cfg(unix)]

use std::{error::Error, fs, path::PathBuf, time::SystemTime};

use hyphae_client::v2::{
    BuiltInRole, ClientError, CustomRoleGrant, HttpTransport, HyphaeClient, ProductErrorCode,
    ProductLimits, ProductOperation, ProductPermission, ProductResponse, ProductScope,
    RequestOptions,
};
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    MetricId, MetricValue, NativeProduct, NativeProductService, NativeProductServiceConfig,
    ProductDurabilityPolicy, TelemetryRegistry,
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
            "hyphae-sdk-security-write-{}-{suffix}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(Self {
            data: root.join("data"),
            endpoint: PathBuf::from("/tmp").join(format!(
                "hyphae-security-write-{}-{suffix:x}.sock",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_security_mutations_replay_across_local_and_http_and_reject_conflicts()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let owner_path = directory.root.join("owner.key");
    let mut product = NativeProduct::create(&directory.data)?;
    product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
    let owner_secret = fs::read_to_string(owner_path)?;
    let telemetry = product.telemetry().clone();
    let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let server = NativeHttpV2Server::new_managed(
        handle,
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
        service,
        directory.endpoint.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let local =
        HyphaeClient::local_authenticated(directory.endpoint.to_string_lossy(), &owner_secret)?;
    let http = HyphaeClient::new(
        HttpTransport::new(&format!("http://{address}"))?.bearer_token(&owner_secret)?,
    );

    assert_mutation_lifecycle(&local, &http).await?;

    let requests_before = request_count(&telemetry)?;
    assert_missing_idempotency_rejected_before_client_dispatch(&local, &http).await;
    assert_eq!(request_count(&telemetry)?, requests_before);
    assert_http_rejects_missing_and_zero_idempotency_before_dispatch(
        address,
        &owner_secret,
        &telemetry,
    )
    .await?;

    drop(local);
    drop(http);
    let _ignored = shutdown_http.send(());
    serve_http.await??;
    drop(daemon.shutdown().await?);
    Ok(())
}

async fn assert_mutation_lifecycle(
    local: &HyphaeClient,
    http: &HyphaeClient,
) -> Result<(), Box<dyn Error>> {
    let created = local
        .security_principal_create("Managed application", mutation_options(101))
        .await?;
    let replayed = http
        .security_principal_create("Managed application", mutation_options(101))
        .await?;
    assert_eq!(replayed, created);
    assert_eq!(
        hyphae_native_protocol::encode_product_response(
            &ProductResponse::SecurityPrincipalMutated(created)
        )?,
        hyphae_native_protocol::encode_product_response(
            &ProductResponse::SecurityPrincipalMutated(replayed)
        )?
    );

    let conflict = http
        .security_principal_create("Different request", mutation_options(101))
        .await;
    assert!(matches!(
        conflict,
        Err(ClientError::Product(error)) if error.code() == ProductErrorCode::IdempotencyConflict
    ));

    let enabled = local
        .security_principal_set_enabled(created.principal_id, true, mutation_options(102))
        .await?;
    let role = local
        .security_custom_role_create(
            "Scoped reader",
            vec![
                CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
                    .ok_or("valid custom-role grant")?,
            ],
            mutation_options(103),
        )
        .await?;
    let built_in_assignment = local
        .security_built_in_assignment_create(
            created.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            mutation_options(104),
        )
        .await?;
    let custom_assignment = local
        .security_custom_assignment_create(
            created.principal_id,
            role.role_id,
            mutation_options(105),
        )
        .await?;
    let revoked = local
        .security_assignment_revoke(built_in_assignment.assignment_id, mutation_options(106))
        .await?;
    assert!(
        enabled.authorization_epoch < role.authorization_epoch
            && role.authorization_epoch < built_in_assignment.authorization_epoch
            && built_in_assignment.authorization_epoch < custom_assignment.authorization_epoch
            && custom_assignment.authorization_epoch < revoked.authorization_epoch
    );
    let owner_assignment = local
        .security_built_in_assignment_create(
            created.principal_id,
            BuiltInRole::Owner,
            ProductScope::Instance,
            mutation_options(107),
        )
        .await;
    assert!(matches!(
        owner_assignment,
        Err(ClientError::Product(error)) if error.code() == ProductErrorCode::InvalidRequest
    ));
    Ok(())
}

fn mutation_options(idempotency_token: u128) -> RequestOptions {
    RequestOptions {
        idempotency_token: Some(idempotency_token),
        ..RequestOptions::default()
    }
}

async fn assert_missing_idempotency_rejected_before_client_dispatch(
    local: &HyphaeClient,
    http: &HyphaeClient,
) {
    let missing = local
        .security_principal_create("missing token", RequestOptions::default())
        .await;
    assert!(matches!(missing, Err(ClientError::Protocol(_))));
    let zero = http
        .security_principal_create("zero token", mutation_options(0))
        .await;
    assert!(matches!(zero, Err(ClientError::Protocol(_))));
}

async fn assert_http_rejects_missing_and_zero_idempotency_before_dispatch(
    address: std::net::SocketAddr,
    credential: &str,
    telemetry: &TelemetryRegistry,
) -> Result<(), Box<dyn Error>> {
    let request = hyphae_native_protocol::WireRequest {
        operation: ProductOperation::SecurityPrincipalCreate {
            display_name: "wire rejection".to_owned(),
        },
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: Some(107),
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    let encoded = hyphae_native_protocol::encode_product_request(&request)?;
    let missing = strip_idempotency(encoded.clone())?;
    let mut zero = encoded;
    zero[32..48].fill(0);
    let requests_before = request_count(telemetry)?;
    for (offset, body) in [missing, zero].into_iter().enumerate() {
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
            .header(
                hyphae_contracts::v2::REQUEST_ID_HEADER_V2,
                (9_001 + offset).to_string(),
            )
            .body(body)
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    }
    assert_eq!(request_count(telemetry)?, requests_before);
    Ok(())
}

fn strip_idempotency(mut encoded: Vec<u8>) -> Result<Vec<u8>, Box<dyn Error>> {
    encoded.drain(32..48);
    encoded[73..80].fill(0);
    let length = u32::try_from(encoded.len())?;
    encoded[8..12].copy_from_slice(&length.to_le_bytes());
    Ok(encoded)
}

fn request_count(telemetry: &TelemetryRegistry) -> Result<u64, Box<dyn Error>> {
    let row = telemetry
        .snapshot(0, None)
        .metrics
        .into_iter()
        .find(|row| row.descriptor.id == MetricId::Requests)
        .ok_or("request metric is missing")?;
    match row.value {
        MetricValue::Counter(value) => Ok(value),
        _ => Err("request metric is not a counter".into()),
    }
}
