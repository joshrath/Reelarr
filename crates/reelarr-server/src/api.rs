use crate::{auth, state::AppState, stream};
use axum::{Router, middleware};

/// Authentication endpoints stay public; domain routes require a valid session.
///
/// Merge every domain router into `protected` BEFORE applying
/// `.route_layer(require_auth)` (via `from_fn_with_state`). Never use `.layer()`
/// on an empty router and then merge domain routes: layers do not retroactively
/// protect routes added afterwards. See the #50 fix and auth boundary in #7.
pub fn router(state: AppState) -> Router<AppState> {
    // route_layer only wraps routes added before it, so add domain routes first.
    let protected = Router::new().merge(stream::router());
    #[cfg(test)]
    let protected = protected.merge(tests::domain_router());
    let protected =
        protected.route_layer(middleware::from_fn_with_state(state, auth::require_auth));
    auth::router().merge(protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Extension, Json,
        body::Body,
        http::{Request, StatusCode, header},
        routing::get,
    };
    use http_body_util::BodyExt;
    use reelarr_types::{ApiError, UserInfo};
    use tower::ServiceExt;

    // No AuthUser extractor fallback: success requires the *router middleware*
    // to insert the authenticated extension. This catches the empty-router layer
    // regression even if individual production handlers authenticate themselves.
    pub(super) fn domain_router() -> Router<AppState> {
        Router::new().route(
            "/__auth_convention",
            get(|Extension(auth::AuthUser(user)): Extension<auth::AuthUser>| async { Json(user) }),
        )
    }

    #[tokio::test]
    async fn real_api_protected_domain_route_returns_401_json_then_authenticated_success() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            reelarr_db::Database::connect(dir.path().join("reelarr.db"))
                .await
                .unwrap(),
        );
        let user = UserInfo {
            id: "router-regression-user".into(),
            username: "admin".into(),
        };
        let session = state.auth.create_session(Some(user.clone())).unwrap();
        let app = Router::new()
            .nest("/api/v1", router(state.clone()))
            .with_state(state);
        let request = || {
            Request::get("/api/v1/__auth_convention")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: ApiError = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "unauthorized");
        assert_eq!(error.message, "A valid session is required");
        let mut signed_in = request();
        signed_in.headers_mut().insert(
            header::COOKIE,
            format!("reelarr_session={}", session.token)
                .parse()
                .unwrap(),
        );
        let response = app.oneshot(signed_in).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(serde_json::from_slice::<UserInfo>(&body).unwrap(), user);
    }
}
