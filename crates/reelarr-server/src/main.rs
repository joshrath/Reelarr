use std::{net::SocketAddr, path::PathBuf};

use reelarr_db::Database;
use reelarr_server::{app_with_ui, check_site, leptos_options, state::AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let state = AppState::new(db);
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
    axum::serve(listener, app_with_ui(state, leptos_options)).await?;
    Ok(())
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
