mod config;
mod services;
mod views;

use std::{net::SocketAddr, sync::Arc, time::Instant};

use tracing::info;

use crate::{
    config::CONFIG,
    services::{
        archive::{ArchiveIndex, Catalog},
        inpx,
    },
    views::{get_router, AppState},
};
use tracing_subscriber::{filter, layer::SubscriberExt, util::SubscriberInitExt};

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received...");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(filter::LevelFilter::INFO)
        .init();

    let dir = CONFIG.archive_dir.clone();
    let explicit = CONFIG.inpx_path.clone();
    let catalog = tokio::task::spawn_blocking(move || {
        let inpx_path = explicit.or_else(|| inpx::discover(&dir));
        if let Some(path) = inpx_path {
            let start = Instant::now();
            match inpx::load(&path) {
                Ok(idx) => {
                    if idx.is_empty() {
                        tracing::warn!("INPX {} contains no books", path.display());
                    }
                    info!(
                        "Loaded INPX {} (version {}): {} books in {} archives, {:?}",
                        path.display(),
                        idx.version,
                        idx.len(),
                        idx.archives(),
                        start.elapsed()
                    );
                    return Catalog::Inpx(idx);
                }
                Err(err) => {
                    tracing::error!(
                        "Cannot load INPX {}: {err}; falling back to scan",
                        path.display()
                    )
                }
            }
        } else {
            info!("No .inpx found, using archive range scan");
        }
        let index = ArchiveIndex::scan(&dir);
        info!("Indexed {} archives", index.len());
        if index.is_empty() {
            tracing::warn!("No archives found in {}", dir.display());
        }
        Catalog::Ranges(index)
    })
    .await
    .unwrap();
    let state = AppState {
        index: Arc::new(catalog),
        api_key: CONFIG.api_key.as_deref().map(Arc::from),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, get_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("Webserver shutdown...");
}
