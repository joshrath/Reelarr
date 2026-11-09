//! NNTP provider settings API (#76 PR 3).
//!
//! - `GET    /settings/nntp`       stored settings, or 404 `not_configured`
//! - `PUT    /settings/nntp`       validate, encrypt the password, save
//! - `DELETE /settings/nntp`       forget the provider and its password
//! - `POST   /settings/nntp/test`  TLS + login check against candidate settings
//!
//! All routes are in the protected router (session required); mutations,
//! including the test, also require the CSRF double-submit proof. The password
//! is write-only: responses carry `has_password` only. Error messages are
//! static and never echo credentials, and nothing here logs request bodies.
//!
//! **Pool replacement policy.** Saved settings take effect only for work that
//! starts after the change: save and delete notify the download supervisor
//! ([`crate::downloads`]), which builds a new `Pool` from
//! [`reelarr_db::SqliteNntpStore::load`]. New jobs go to that pool, and jobs
//! already downloading finish on the pool they started with, which is shut
//! down once idle. Running downloads are never pre-empted.
use crate::state::{AppState, NntpTrust};
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reelarr_db::NntpSettingsError;
use reelarr_nzb::nntp::{Error as NntpError, Pool};
use reelarr_types::{
    ApiError,
    nntp::{NntpServerSettings, NntpServerUpdate, NntpTestOutcome, NntpTestResult},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/nntp", get(read).put(save).delete(remove))
        .route("/settings/nntp/test", post(test))
}

pub struct NntpApiError(StatusCode, ApiError);
impl IntoResponse for NntpApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}
fn error(status: StatusCode, code: &str, message: impl Into<String>) -> NntpApiError {
    NntpApiError(
        status,
        ApiError {
            code: code.into(),
            message: message.into(),
        },
    )
}
impl From<NntpSettingsError> for NntpApiError {
    fn from(e: NntpSettingsError) -> Self {
        // Display text is static by construction (no values, SQL or server text).
        let message = e.to_string();
        match e {
            NntpSettingsError::Invalid(_) => {
                error(StatusCode::BAD_REQUEST, "invalid_settings", message)
            }
            NntpSettingsError::PasswordRequired(_) => {
                error(StatusCode::BAD_REQUEST, "password_required", message)
            }
            NntpSettingsError::NotConfigured => {
                error(StatusCode::NOT_FOUND, "not_configured", message)
            }
            NntpSettingsError::Store => {
                tracing::error!("NNTP settings storage failed");
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "The NNTP settings could not be read or saved",
                )
            }
        }
    }
}
/// serde/axum rejection text can quote parts of the body; never return it.
fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, NntpApiError> {
    body.map(|Json(v)| v).map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "Expected NNTP settings JSON (see the API docs for the field list)",
        )
    })
}

async fn read(State(state): State<AppState>) -> Result<Json<NntpServerSettings>, NntpApiError> {
    Ok(Json(
        state
            .nntp
            .get()
            .await?
            .ok_or(NntpSettingsError::NotConfigured)?,
    ))
}

async fn save(
    State(state): State<AppState>,
    update: Result<Json<NntpServerUpdate>, JsonRejection>,
) -> Result<Json<NntpServerSettings>, NntpApiError> {
    let saved = state.nntp.save(&body(update)?).await?;
    tracing::info!("NNTP settings saved");
    state.downloads.nntp_settings_changed();
    Ok(Json(saved))
}

async fn remove(State(state): State<AppState>) -> Result<StatusCode, NntpApiError> {
    state.nntp.clear().await?;
    tracing::info!("NNTP settings removed");
    state.downloads.nntp_settings_changed();
    Ok(StatusCode::NO_CONTENT)
}

/// Probe candidate settings (the body applied over the stored row, so an
/// unchanged password can stay `keep`) without saving them. TLS and login
/// only; no article is fetched. Bounded by the candidate's connect timeout.
async fn test(
    State(state): State<AppState>,
    update: Result<Json<NntpServerUpdate>, JsonRejection>,
) -> Result<Json<NntpTestResult>, NntpApiError> {
    let server = state.nntp.resolve(&body(update)?).await?;
    let config = server.config;
    let pool = match state.nntp_trust {
        NntpTrust::Roots(roots) => Pool::with_root_certificates(config, (*roots).clone()),
        // Loading native roots is blocking file I/O.
        NntpTrust::Native => tokio::task::spawn_blocking(move || Pool::new(config))
            .await
            .map_err(|_| {
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "Test failed to start",
                )
            })?,
    };
    let result = match pool {
        Ok(pool) => {
            let result = pool.probe().await;
            pool.shutdown().await;
            result
        }
        Err(e) => Err(e),
    };
    Ok(Json(outcome(result)))
}

fn outcome(result: Result<(), NntpError>) -> NntpTestResult {
    use NntpTestOutcome as O;
    let (outcome, message) = match result {
        Ok(()) => (O::Connected, "Connected and signed in"),
        Err(NntpError::Tls) => (
            O::TlsRejected,
            "TLS failed: the certificate is not trusted, does not match the host, or the port does not speak implicit TLS",
        ),
        Err(NntpError::TrustRoots) => (
            O::TrustRootsUnavailable,
            "No trusted certificate authorities are installed",
        ),
        Err(NntpError::Authentication { .. }) => (
            O::AuthRejected,
            "The server rejected the username or password",
        ),
        Err(NntpError::Timeout(_)) => (
            O::Timeout,
            "The server did not finish connecting within the connect timeout",
        ),
        Err(NntpError::Transport(_)) => (O::Unreachable, "Could not reach the server"),
        Err(NntpError::Unavailable { .. }) => (
            O::ServerUnavailable,
            "The server is temporarily unavailable",
        ),
        Err(NntpError::Response { .. }) => {
            (O::ServerUnavailable, "The server refused the connection")
        }
        Err(NntpError::Config(_)) => (O::ProtocolError, "The settings could not be used"),
        Err(
            NntpError::Incomplete
            | NntpError::Protocol(_)
            | NntpError::Limit(_)
            | NntpError::NotFound
            | NntpError::Closed,
        ) => (
            O::ProtocolError,
            "The server did not respond like an NNTP server",
        ),
    };
    if outcome != O::Connected {
        // Secret-free classification only (no host, credentials or server text).
        tracing::info!(?outcome, "NNTP test-connection failed");
    }
    NntpTestResult {
        ok: outcome == O::Connected,
        outcome,
        message: message.into(),
    }
}
