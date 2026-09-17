// SPDX-License-Identifier: Apache-2.0

//! Basic v2 transport connectivity coverage.

use std::time::{SystemTime, UNIX_EPOCH};

use hyphae_client::v2::{
    CatalogVisibleCursor, CatalogVisibleListFilter, CatalogVisibleListRequest, HttpTransport,
    HyphaeClient, ProductResponse, RequestOptions,
};
use hyphae_native_product::{
    NativeProduct, NativeProductService, NativeProductServiceConfig, ObjectId, ProductStructureKey,
    ProductStructureMutation, ProductStructureMutationResult,
};
use hyphae_server::{NativeHttpV2Config, NativeHttpV2Server};

#[tokio::test]
async fn real_http_capabilities_execute() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!(
        "hyphae-sdk-http-v2-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let service = NativeProductService::start(
        NativeProduct::create(&path)?,
        NativeProductServiceConfig::default(),
    )?;
    let config = NativeHttpV2Config {
        bind: "127.0.0.1:0".parse()?,
        ..NativeHttpV2Config::default()
    };
    let server = NativeHttpV2Server::new(service.handle(), config)?
        .bind()
        .await?;
    let address = server.local_addr();
    let shutdown = tokio::sync::oneshot::channel::<()>();
    let serve = tokio::spawn(server.run_with_shutdown(async move {
        let _ignored = shutdown.1.await;
    }));

    let real = HyphaeClient::new(HttpTransport::new(&format!("http://{address}"))?);
    let real_response = real.capabilities(RequestOptions::default()).await?;
    assert!(matches!(real_response, ProductResponse::Capabilities(_)));
    let ids = visible_catalog_sequence(&real).await?;
    assert!(!ids.is_empty());
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_structure_noop(&real).await?;

    let _ignored = shutdown.0.send(());
    serve.await??;
    drop(real);
    let product = service.shutdown()?;
    drop(product);
    std::fs::remove_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn real_local_capabilities_execute() -> Result<(), Box<dyn std::error::Error>> {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let path = std::env::temp_dir().join(format!("hyphae-sdk-local-v2-{suffix}"));
    let endpoint = std::env::temp_dir().join(format!("hyphae-sdk-local-v2-{suffix}.sock"));
    let daemon = hyphae_native_daemon::NativeDaemon::start(
        NativeProduct::create(&path)?,
        endpoint.to_string_lossy(),
        hyphae_native_daemon::NativeDaemonConfig::default(),
    )?;

    let real = HyphaeClient::local(endpoint.to_string_lossy())?;
    let real_response = real.capabilities(RequestOptions::default()).await?;
    assert!(matches!(real_response, ProductResponse::Capabilities(_)));
    let ids = visible_catalog_sequence(&real).await?;
    assert!(!ids.is_empty());
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_structure_noop(&real).await?;

    drop(real);
    let product = daemon.shutdown().await?;
    drop(product);
    if endpoint.exists() {
        std::fs::remove_file(endpoint)?;
    }
    std::fs::remove_dir_all(path)?;
    Ok(())
}

async fn visible_catalog_sequence(
    client: &HyphaeClient,
) -> Result<Vec<u128>, Box<dyn std::error::Error>> {
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let response = client
            .catalog_visible_list(visible_catalog_request(cursor), RequestOptions::default())
            .await?;
        let ProductResponse::CatalogVisiblePage(page) = response else {
            return Err("visible catalog returned another response variant".into());
        };
        ids.extend(page.items.into_iter().map(|item| item.id.get()));
        let Some(next) = page.cursor else {
            break;
        };
        cursor = Some(next);
    }
    Ok(ids)
}

fn visible_catalog_request(cursor: Option<CatalogVisibleCursor>) -> CatalogVisibleListRequest {
    CatalogVisibleListRequest {
        filter: CatalogVisibleListFilter {
            parent: None,
            kind: None,
        },
        cursor,
        item_limit: 1,
        visit_limit: 8,
        byte_limit: 4_096,
    }
}

async fn assert_structure_noop(client: &HyphaeClient) -> Result<(), Box<dyn std::error::Error>> {
    client
        .structure_set(
            b"conditional".to_vec(),
            b"present".to_vec(),
            None,
            RequestOptions::default(),
        )
        .await?;
    let response = client
        .structure_mutate(
            vec![ProductStructureMutation::StringSetConditional {
                key: ProductStructureKey {
                    keyspace: ObjectId::new(3)?,
                    key: b"conditional".to_vec(),
                },
                value: b"other".to_vec(),
                expires_at_micros: None,
                if_present: false,
            }],
            RequestOptions::default(),
        )
        .await?;
    assert!(matches!(
        response,
        ProductResponse::StructureMutationBatch(receipt)
            if receipt.commit.is_none()
                && !receipt.results[0].changed
                && receipt.results[0].result
                    == ProductStructureMutationResult::Boolean(false)
    ));
    Ok(())
}
