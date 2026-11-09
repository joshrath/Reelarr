use std::{net::SocketAddr, path::PathBuf, sync::Arc};

mod healthcheck;

use reelarr_db::{Database, secrets::XChaChaSecretCipher};
use reelarr_server::{
    app_with_ui, check_site, leptos_options,
    state::{AppState, DEFAULT_DOWNLOADS_ROOT, DownloadOptions},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("healthcheck") => return healthcheck::run(),
        Some(arg) => return Err(format!("unknown command: {arg}").into()),
        None => {}
    }
    tracing_subscriber::fmt::init();

    let leptos_options = match leptos_options().and_then(|o| check_site(&o).map(|()| o)) {
        Ok(options) => options,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };

    let data_dir = std::env::var_os("REELARR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/config"));
    tokio::fs::create_dir_all(&data_dir).await?;
    // Connecting applies migrations; errors return before any socket is bound.
    let db = Database::connect(data_dir.join("reelarr.db")).await?;
    // Secret-at-rest key: REELARR_SECRET_KEY, else the 0600 secret.key in the
    // data directory (created on first start). Blocking file I/O; fail before binding.
    let key_dir = data_dir.clone();
    let cipher =
        tokio::task::spawn_blocking(move || XChaChaSecretCipher::from_env_or_data_dir(&key_dir))
            .await?
            .map_err(|e| format!("cannot load the secret-at-rest key: {e}"))?;
    let mut downloads = DownloadOptions::new(
        std::env::var_os("REELARR_DOWNLOADS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DOWNLOADS_ROOT)),
    );
    if let Some(jobs) = std::env::var("REELARR_MAX_ACTIVE_DOWNLOADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        downloads.scheduler.max_concurrent_jobs = jobs;
    }
    let state = AppState::with_options(db, Arc::new(cipher), downloads);
    // Reconcile durable snapshots before listening; only then may the
    // scheduler dispatch anything.
    state.downloads.start().await?;
    tracing::debug!(connections = state.db.pool().size(), "Database initialized");

    let port: u16 = std::env::var("REELARR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7979);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    tracing::info!("Reelarr listening on {addr}");
    let downloads = state.downloads.clone();
    let served = axum::serve(listener, app_with_ui(state, leptos_options))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    // Stop and join running downloads; they resume from checkpoints on restart.
    downloads.shutdown().await;
    served?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("reelarr.db"))
            .await
            .unwrap();
        let res = reelarr_server::app(AppState::new(db))
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_success());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let health: reelarr_types::Health = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
    }
}
