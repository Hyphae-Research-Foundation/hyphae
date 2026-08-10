// SPDX-License-Identifier: GPL-3.0-only

//! Basic v2 transport connectivity coverage.

use std::time::{SystemTime, UNIX_EPOCH};

use hyphae_client::v2::{HttpTransport, HyphaeClient, ProductResponse, RequestOptions};
use hyphae_native_product::{NativeProduct, NativeProductService, NativeProductServiceConfig};
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

    drop(real);
    let product = daemon.shutdown().await?;
    drop(product);
    if endpoint.exists() {
        std::fs::remove_file(endpoint)?;
    }
    std::fs::remove_dir_all(path)?;
    Ok(())
}
