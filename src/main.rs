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
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let dir = CONFIG.archive_dir.clone();
    if let Err(err) = std::fs::read_dir(&dir) {
        eprintln!(
            "ARCHIVE_DIR {} does not exist or is not readable: {err}",
            dir.display()
        );
        std::process::exit(1);
    }
    let explicit = CONFIG.inpx_path.clone();
    let catalog = tokio::task::spawn_blocking(move || {
        let inpx_path = match explicit {
            Some(p) => Some(p),
            None => match inpx::discover(&dir) {
                Ok(p) => p,
                Err(msg) => {
                    tracing::error!("{msg}");
                    std::process::exit(1);
                }
            },
        };
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
    let mut state = AppState::new(Arc::new(catalog), CONFIG.api_key.as_deref().map(Arc::from));
    state.max_entry_bytes = CONFIG.max_entry_bytes;
    state.timeout = CONFIG.request_timeout;
    state.max_concurrent = CONFIG.max_concurrent_downloads;

    let addr = SocketAddr::from(([0, 0, 0, 0], CONFIG.port));

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, get_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("Webserver shutdown...");
}
