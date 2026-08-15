// SPDX-License-Identifier: AGPL-3.0-only

//! Managed Native API-key authentication over the exact local transport.

#![cfg(unix)]

use std::time::{SystemTime, UNIX_EPOCH};

use hyphae_client::v2::{ClientError, HyphaeClient, RequestOptions};
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    BuiltInRole, NativeProduct, NativeProductService, NativeProductServiceConfig,
    ProductAuthorization, ProductErrorCode, ProductOperation, ProductPermission, ProductResponse,
    ProductScope,
};
use hyphae_native_protocol::{
    AsyncFrameIo, FrameKind, ProtocolCapabilities, Welcome, encode_welcome,
};

#[tokio::test]
async fn authenticated_local_client_preserves_typed_authorization_and_connection()
-> Result<(), Box<dyn std::error::Error>> {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let root = std::env::temp_dir().join(format!("hyphae-sdk-local-auth-{suffix}"));
    let data = root.join("data");
    let endpoint = std::env::temp_dir().join(format!("hcl-auth-{suffix}.sock"));
    let owner_path = root.join("owner.key");
    let reader_path = root.join("reader.key");
    std::fs::create_dir_all(&root)?;

    let mut product = NativeProduct::create(&data)?;
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
        ProductAuthorization::from_permissions([ProductPermission::DataRead]),
        None,
        &reader_path,
        5,
    )?;
    let reader_secret = std::fs::read_to_string(&reader_path)?;
    let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let daemon = NativeDaemon::start_with_service_authenticated(
        service,
        endpoint.to_string_lossy(),
        NativeDaemonConfig::default(),
    )?;
    let client = HyphaeClient::local_authenticated(endpoint.to_string_lossy(), &reader_secret)?;

    assert_eq!(
        get_shared(&client).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );
    let denied = client
        .execute(
            ProductOperation::StructureSet {
                key: b"shared".to_vec(),
                value: b"forbidden".to_vec(),
                expires_at_micros: None,
            },
            RequestOptions::default(),
        )
        .await;
    assert!(matches!(
        denied,
        Err(ClientError::Product(error)) if error.code() == ProductErrorCode::AuthorizationDenied
    ));
    assert_eq!(
        get_shared(&client).await?,
        ProductResponse::StructureValue(Some(b"value".to_vec()))
    );

    handle.revoke_api_key(owner, issued.key_id, 5)?;
    let revoked = client
        .execute(
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
            RequestOptions::default(),
        )
        .await;
    assert!(matches!(
        revoked,
        Err(ClientError::Product(error)) if error.code() == ProductErrorCode::AuthorizationDenied
    ));

    drop(client);
    drop(daemon.shutdown().await?);
    if endpoint.exists() {
        std::fs::remove_file(endpoint)?;
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

async fn get_shared(client: &HyphaeClient) -> Result<ProductResponse, ClientError> {
    client
        .execute(
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
            RequestOptions::default(),
        )
        .await
}

#[tokio::test]
async fn authenticated_local_client_rejects_welcome_capability_downgrade()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let endpoint = std::env::temp_dir().join(format!("hcl-downgrade-{suffix}.sock"));
    let listener = UnixListener::bind(&endpoint)?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut codec = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)?;
        let hello = codec
            .receive(&mut stream)
            .await?
            .ok_or_else(|| std::io::Error::other("client closed before HELLO"))?;
        let product = hyphae_native_product::capabilities();
        codec
            .send(
                &mut stream,
                FrameKind::Welcome,
                0,
                hello.request_id,
                &encode_welcome(Welcome {
                    major: 1,
                    minor: 0,
                    capabilities: ProtocolCapabilities::G6,
                    session_id: 1,
                    maximum_frame_payload: 16 * 1024 * 1024,
                    maximum_in_flight: 64,
                    initial_window: 64 * 1024,
                    product_api_version: product.product_api_version,
                    native_directory_format: product.native_directory_format,
                    logical_catalog_codec_version: product.logical_catalog_codec_version,
                    catalog_tree_format_version: product.catalog_tree_format_version,
                    catalog_version: 0,
                    max_sql_statement_bytes: u64::try_from(product.max_sql_statement_bytes)?,
                    max_sql_parameters: u64::try_from(product.max_sql_parameters)?,
                    max_sql_rows: u64::try_from(product.max_sql_rows)?,
                })?,
            )
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = HyphaeClient::local_authenticated(endpoint.to_string_lossy(), "x".repeat(102))?;
    let result = client.capabilities(RequestOptions::default()).await;
    assert!(matches!(
        result,
        Err(ClientError::Protocol(message)) if message.contains("downgraded")
    ));
    server
        .await?
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if endpoint.exists() {
        std::fs::remove_file(endpoint)?;
    }
    Ok(())
}
