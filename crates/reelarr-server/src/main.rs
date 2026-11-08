use std::net::SocketAddr;

use reelarr_server::{app_with_ui, check_site, leptos_options, state::AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let leptos_options = match leptos_options().and_then(|o| check_site(&o).map(|()| o)) {
        Ok(options) => options,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };

    let port: u16 = std::env::var("REELARR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7979);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    tracing::info!("Reelarr listening on {addr}");
    axum::serve(listener, app_with_ui(AppState::default(), leptos_options))
        .await
        .expect("server error");
}
