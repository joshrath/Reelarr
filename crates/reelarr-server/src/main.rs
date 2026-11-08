use std::net::SocketAddr;

use axum::{Json, Router, routing::get};
use reelarr_types::Health;

fn app() -> Router {
    Router::new().route("/health", get(health))
}

async fn health() -> Json<Health> {
    Json(Health::ok())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port: u16 = std::env::var("REELARR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7979);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    tracing::info!("Reelarr listening on {addr}");
    axum::serve(listener, app()).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok() {
        let res = app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_success());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let health: Health = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
    }
}
