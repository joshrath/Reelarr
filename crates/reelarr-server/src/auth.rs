//! Session/CSRF HTTP contract. See docs/auth-stub.md for bootstrap and rotation.
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{FromRequestParts, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reelarr_core::auth::{AuthError, Session};
use reelarr_types::{ApiError, LoginRequest, SessionInfo, SetupRequest, SetupStatus, UserInfo};
use subtle::ConstantTimeEq;

pub struct AuthRejection(StatusCode, ApiError);
impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}
fn error(status: StatusCode, code: &str, message: &str) -> AuthRejection {
    AuthRejection(
        status,
        ApiError {
            code: code.into(),
            message: message.into(),
        },
    )
}
fn unauthorized() -> AuthRejection {
    error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "A valid session is required",
    )
}
impl From<AuthError> for AuthRejection {
    fn from(value: AuthError) -> Self {
        match value {
            AuthError::AlreadyConfigured => error(
                StatusCode::CONFLICT,
                "already_configured",
                "Setup has already completed",
            ),
            AuthError::InvalidCredentials => error(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "Invalid username or password",
            ),
            AuthError::InvalidInput => error(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "Username must be 1–128 bytes and password 12–1024 bytes",
            ),
            AuthError::Unavailable => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "auth_unavailable",
                "Authentication is unavailable",
            ),
        }
    }
}
fn json_error(_: JsonRejection) -> AuthRejection {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_json",
        "Expected a valid JSON request",
    )
}
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|s| s.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}
fn session(headers: &HeaderMap, state: &AppState) -> Result<Session, AuthRejection> {
    let token = cookie(headers, "reelarr_session").ok_or_else(unauthorized)?;
    state.auth.lookup_session(token)?.ok_or_else(unauthorized)
}
fn csrf(headers: &HeaderMap, state: &AppState) -> Result<Session, AuthRejection> {
    let rejection = || {
        error(
            StatusCode::FORBIDDEN,
            "csrf_rejected",
            "Fetch a session and supply its X-CSRF-Token and cookies",
        )
    };
    // Anonymous sessions are deliberately allowed here for setup and login.
    let session = match session(headers, state) {
        Ok(session) => session,
        Err(AuthRejection(StatusCode::UNAUTHORIZED, _)) => return Err(rejection()),
        Err(other) => return Err(other),
    };
    let token = headers
        .get("x-csrf-token")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(rejection)?;
    let cookie = cookie(headers, "reelarr_csrf").ok_or_else(rejection)?;
    if !bool::from(token.as_bytes().ct_eq(cookie.as_bytes()))
        || !bool::from(token.as_bytes().ct_eq(session.csrf_token.as_bytes()))
    {
        return Err(rejection());
    }
    Ok(session)
}

/// Requires a valid authenticated session. Anonymous bootstrap sessions are rejected.
#[derive(Clone, Debug)]
pub struct AuthUser(pub UserInfo);
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AuthRejection;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        session(&parts.headers, state)?
            .user
            .map(Self)
            .ok_or_else(unauthorized)
    }
}

/// Apply with `from_fn_with_state(state.clone(), require_auth)` to protected routes.
/// Mutating protected routes also require the double-submit CSRF proof.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthRejection> {
    let user = session(request.headers(), &state)?
        .user
        .ok_or_else(unauthorized)?;
    if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) {
        csrf(request.headers(), &state)?;
    }
    request.extensions_mut().insert(AuthUser(user));
    Ok(next.run(request).await)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/setup", get(setup_status).post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(get_session))
}
async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, AuthRejection> {
    Ok(Json(SetupStatus {
        needs_setup: state.auth.needs_setup()?,
    }))
}
fn session_response(session: Session) -> Response {
    let mut response = Json(SessionInfo {
        user: session.user,
        csrf_token: session.csrf_token.clone(),
    })
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response.headers_mut().append(
        header::SET_COOKIE,
        format!(
            "reelarr_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400",
            session.token
        )
        .parse()
        .unwrap(),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        format!(
            "reelarr_csrf={}; Path=/; SameSite=Lax; Max-Age=86400",
            session.csrf_token
        )
        .parse()
        .unwrap(),
    );
    response
}
async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AuthRejection> {
    let existing = cookie(&headers, "reelarr_session")
        .map(|token| state.auth.lookup_session(token))
        .transpose()?
        .flatten();
    Ok(session_response(match existing {
        Some(session) => session,
        None => state.auth.create_session(None)?,
    }))
}
fn rotate(
    state: &AppState,
    old: Session,
    user: Option<UserInfo>,
) -> Result<Response, AuthRejection> {
    let new = state.auth.create_session(user)?;
    state.auth.revoke_session(&old.token)?;
    Ok(session_response(new))
}
async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SetupRequest>, JsonRejection>,
) -> Result<Response, AuthRejection> {
    let old = csrf(&headers, &state)?;
    let Json(body) = body.map_err(json_error)?;
    let service = state.auth.clone();
    let user =
        tokio::task::spawn_blocking(move || service.create_admin(&body.username, &body.password))
            .await
            .map_err(|_| AuthRejection::from(AuthError::Unavailable))??;
    rotate(&state, old, Some(user))
}
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, AuthRejection> {
    let old = csrf(&headers, &state)?;
    let Json(body) = body.map_err(json_error)?;
    let service = state.auth.clone();
    let user =
        tokio::task::spawn_blocking(move || service.verify_login(&body.username, &body.password))
            .await
            .map_err(|_| AuthRejection::from(AuthError::Unavailable))??;
    rotate(&state, old, Some(user))
}
async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AuthRejection> {
    let old = csrf(&headers, &state)?;
    rotate(&state, old, None)
}
