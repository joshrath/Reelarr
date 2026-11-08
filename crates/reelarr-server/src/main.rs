use std::net::SocketAddr;

use reelarr_server::{app_with_ui, leptos_options, state::AppState};

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
    axum::serve(listener, app_with_ui(AppState::default(), leptos_options()))
        .await
        .expect("server error");
}
