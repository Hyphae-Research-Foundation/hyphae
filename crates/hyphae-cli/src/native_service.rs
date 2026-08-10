// SPDX-License-Identifier: GPL-3.0-only

//! Explicit native local daemon and optional HTTP v2 edge sharing one owner.

use std::{net::SocketAddr, path::PathBuf};

use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{NativeProduct, NativeProductService, NativeProductServiceConfig};
use hyphae_server::{NativeHttpV2Config, NativeHttpV2Server};
use tokio::{sync::watch, task::JoinHandle};

use crate::{exit::CliFailure, native::default_endpoint};

pub(crate) async fn serve(
    data_dir: PathBuf,
    endpoint: Option<String>,
    http_bind: Option<SocketAddr>,
) -> Result<(), CliFailure> {
    let product = NativeProduct::open(&data_dir)?;
    let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let endpoint = endpoint.unwrap_or_else(|| default_endpoint(&data_dir));
    let daemon =
        NativeDaemon::start_with_service(service, endpoint, NativeDaemonConfig::default())?;
    let (http_shutdown, http_receive) = watch::channel(false);
    let http = match http_bind {
        Some(bind) => {
            let config = NativeHttpV2Config {
                bind,
                ..NativeHttpV2Config::default()
            };
            let bound = NativeHttpV2Server::new(handle, config)?.bind().await?;
            let address = bound.local_addr();
            let task = tokio::spawn(async move {
                bound
                    .run_with_shutdown(async move {
                        wait_for_shutdown(http_receive).await;
                    })
                    .await
            });
            Some(HttpServer { address, task })
        }
        None => None,
    };

    eprintln!(
        "hyphae native local daemon listening on {}",
        daemon.endpoint()
    );
    if let Some(http) = &http {
        eprintln!("hyphae native HTTP v2 listening on {}", http.address);
    }
    let _signal = tokio::signal::ctrl_c().await;
    let _ignored = http_shutdown.send(true);
    if let Some(http) = http {
        http.task.await.map_err(|_| CliFailure::internal())??;
    }
    daemon.shutdown().await?;
    Ok(())
}

struct HttpServer {
    address: SocketAddr,
    task: JoinHandle<Result<(), hyphae_server::NativeHttpV2Error>>,
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
}
