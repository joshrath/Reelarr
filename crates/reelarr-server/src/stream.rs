//! Playback routes. Both require a signed-in session; the browser's
//! `<video>` element sends the session cookie on same-origin requests.
use crate::{auth::AuthUser, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, header},
    response::{IntoResponse, Response},
    routing::get,
};
use reelarr_stream::{ClientProfile, StreamError};
use reelarr_types::MediaFileId;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/media/{id}/playback", get(playback))
        // GET also answers HEAD.
        .route("/media/{id}/stream", get(stream))
}

async fn playback(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, StreamError> {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let info = reelarr_stream::playback_info(
        state.media.as_ref(),
        MediaFileId(id),
        ClientProfile::from_user_agent(ua),
    )
    .await?;
    let mut response = Json(info).into_response();
    // The answer depends on the browser.
    response
        .headers_mut()
        .insert(header::VARY, header::USER_AGENT.as_str().parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-cache".parse().unwrap());
    Ok(response)
}

async fn stream(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    reelarr_stream::stream(state.media.as_ref(), MediaFileId(id), &method, &headers).await
}
