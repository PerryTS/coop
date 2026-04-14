//! Signal handling for graceful shutdown.
//!
//! Returns a future that resolves when SIGTERM or SIGINT is received.
//! The axum listener awaits this future via `with_graceful_shutdown`,
//! which stops accepting new connections and drains in-flight ones.

use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// Wait for SIGTERM or SIGINT. Resolves as soon as either fires.
pub async fn wait_for_shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    tokio::select! {
        _ = term.recv() => {
            info!("SIGTERM received, shutting down");
        }
        _ = int.recv() => {
            info!("SIGINT received, shutting down");
        }
    }
}
