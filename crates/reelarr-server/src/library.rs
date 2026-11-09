//! Library folder settings: list, add, re-check and remove library roots.
//! Removing a root only forgets its rows; files on disk are never touched.
use crate::{auth::AuthUser, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use reelarr_media::db::RootError;
use reelarr_media::store::StoreError;
use reelarr_types::{ApiError, LibraryRoot, LibraryRootId, LibraryRootInfo, NewLibraryRoot};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/library/roots", get(list).post(add))
        .route("/library/roots/{id}", delete(remove))
        .route("/library/roots/{id}/check", post(check))
}

pub struct LibraryError(StatusCode, ApiError);

impl IntoResponse for LibraryError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> LibraryError {
    LibraryError(
        status,
        ApiError {
            code: code.into(),
            message: message.into(),
        },
    )
}

impl From<StoreError> for LibraryError {
    fn from(e: StoreError) -> Self {
        tracing::error!(error = %e, "library store failed");
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "The library database could not be read",
        )
    }
}

impl From<RootError> for LibraryError {
    fn from(e: RootError) -> Self {
        let message = e.to_string();
        match e {
            RootError::InvalidPath(_) => error(StatusCode::BAD_REQUEST, "invalid_path", message),
            RootError::Duplicate { .. } => error(StatusCode::CONFLICT, "duplicate_root", message),
            RootError::Overlaps { .. } => error(StatusCode::CONFLICT, "overlapping_root", message),
            RootError::HasFiles { .. } => error(StatusCode::CONFLICT, "root_has_files", message),
            RootError::NotFound => error(StatusCode::NOT_FOUND, "not_found", message),
            RootError::Store(e) => e.into(),
        }
    }
}

async fn list(
    _: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<LibraryRootInfo>>, LibraryError> {
    Ok(Json(state.library_roots.list_info().await?))
}

/// Checks a root's health now and returns its updated info.
async fn refresh(state: &AppState, root: &LibraryRoot) -> Result<LibraryRootInfo, LibraryError> {
    let known = state
        .library_roots
        .list_info()
        .await?
        .into_iter()
        .find(|info| info.root.id == root.id)
        .map_or(0, |info| info.file_count as usize);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let health = state.root_health.check(root, known).await.into_health(now);
    state
        .library_roots
        .set_health(root.id, &health, now)
        .await?;
    state
        .library_roots
        .list_info()
        .await?
        .into_iter()
        .find(|info| info.root.id == root.id)
        .ok_or_else(|| RootError::NotFound.into())
}

async fn add(
    _: AuthUser,
    State(state): State<AppState>,
    body: Result<Json<NewLibraryRoot>, JsonRejection>,
) -> Result<(StatusCode, Json<LibraryRootInfo>), LibraryError> {
    let Json(body) =
        body.map_err(|e| error(StatusCode::BAD_REQUEST, "invalid_body", e.body_text()))?;
    let root = state.library_roots.add(body).await?;
    Ok((StatusCode::CREATED, Json(refresh(&state, &root).await?)))
}

async fn check(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<LibraryRootInfo>, LibraryError> {
    let root = state
        .library_roots
        .get(LibraryRootId(id))
        .await?
        .ok_or(RootError::NotFound)?;
    Ok(Json(refresh(&state, &root).await?))
}

#[derive(Debug, Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    forget_files: bool,
}

async fn remove(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<RemoveQuery>,
) -> Result<StatusCode, LibraryError> {
    state
        .library_roots
        .remove(LibraryRootId(id), query.forget_files)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
