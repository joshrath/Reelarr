use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    middleware,
    response::Response,
    routing::{get, post},
};
use http_body_util::BodyExt;
use reelarr_server::{
    app,
    auth::{AuthUser, require_auth},
    state::AppState,
};
use reelarr_types::{ApiError, Health, SessionInfo, SetupStatus};
use tower::ServiceExt;

async fn test_app() -> Router {
    let state = AppState::new(reelarr_db::Database::connect(":memory:").await.unwrap());
    let protected = Router::new()
        .route(
            "/protected",
            get(|AuthUser(user): AuthUser| async { Json(user) }),
        )
        .route(
            "/protected",
            post(|State(_): State<AppState>| async { StatusCode::NO_CONTENT }),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    app(state.clone())
        .merge(protected.with_state(state.clone()))
        .merge(
            Router::new()
                .route(
                    "/extractor",
                    get(|AuthUser(user): AuthUser| async { Json(user) }),
                )
                .with_state(state),
        )
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookies: &str,
    csrf: &str,
    body: &str,
) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::COOKIE, cookies)
                .header("x-csrf-token", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn json<T: serde::de::DeserializeOwned>(response: Response) -> T {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
async fn session_response(response: Response) -> (String, SessionInfo) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let cookies: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().unwrap())
        .collect();
    assert!(cookies[0].contains("HttpOnly"));
    assert!(cookies.iter().all(|s| s.contains("SameSite=Lax")));
    let cookie_header = cookies
        .iter()
        .map(|s| s.split(';').next().unwrap())
        .collect::<Vec<_>>()
        .join("; ");
    (cookie_header, json(response).await)
}
const CREDENTIALS: &str = r#"{"username":"admin","password":"a-long-password"}"#;

#[tokio::test]
async fn setup_login_session_protected_logout() {
    let app = test_app().await;
    let status: SetupStatus =
        json(request(&app, "GET", "/api/v1/auth/setup", "", "", "").await).await;
    assert!(status.needs_setup);
    let (guest, anonymous) =
        session_response(request(&app, "GET", "/api/v1/auth/session", "", "", "").await).await;
    assert!(anonymous.user.is_none());
    assert_eq!(
        request(&app, "GET", "/protected", &guest, "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (setup_cookie, setup) = session_response(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            &guest,
            &anonymous.csrf_token,
            CREDENTIALS,
        )
        .await,
    )
    .await;
    assert_eq!(setup.user.as_ref().unwrap().username, "admin");
    assert_ne!(guest, setup_cookie);
    let status: SetupStatus =
        json(request(&app, "GET", "/api/v1/auth/setup", "", "", "").await).await;
    assert!(!status.needs_setup);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            &setup_cookie,
            &setup.csrf_token,
            CREDENTIALS
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let (cookie, login) = session_response(
        request(
            &app,
            "POST",
            "/api/v1/auth/login",
            &setup_cookie,
            &setup.csrf_token,
            CREDENTIALS,
        )
        .await,
    )
    .await;
    assert_ne!(setup_cookie, cookie);
    assert_eq!(
        request(&app, "GET", "/protected", &setup_cookie, "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let session: SessionInfo =
        json(request(&app, "GET", "/api/v1/auth/session", &cookie, "", "").await).await;
    assert_eq!(session, login);
    assert_eq!(
        request(&app, "GET", "/protected", &cookie, "", "")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "POST", "/protected", &cookie, &login.csrf_token, "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, logged_out) = session_response(
        request(
            &app,
            "POST",
            "/api/v1/auth/logout",
            &cookie,
            &login.csrf_token,
            "",
        )
        .await,
    )
    .await;
    assert!(logged_out.user.is_none());
    let response = request(&app, "GET", "/protected", &cookie, "", "").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json::<ApiError>(response).await.code, "unauthorized");
}

#[tokio::test]
async fn no_cookie_health_and_csrf_rejection() {
    let app = test_app().await;
    let response = request(&app, "GET", "/protected", "", "", "").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json::<ApiError>(response).await.code, "unauthorized");
    assert_eq!(
        request(&app, "GET", "/extractor", "", "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let health: Health = json(request(&app, "GET", "/health", "", "", "").await).await;
    assert_eq!(health.status, "ok");
    for endpoint in ["setup", "login", "logout"] {
        let response = request(
            &app,
            "POST",
            &format!("/api/v1/auth/{endpoint}"),
            "",
            "",
            CREDENTIALS,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(json::<ApiError>(response).await.code, "csrf_rejected");
    }
    let (cookie, session) =
        session_response(request(&app, "GET", "/api/v1/auth/session", "", "", "").await).await;
    for token in ["", "wrong"] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/auth/setup",
                &cookie,
                token,
                CREDENTIALS
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let (other_cookie, _) =
        session_response(request(&app, "GET", "/api/v1/auth/session", "", "", "").await).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            &other_cookie,
            &session.csrf_token,
            CREDENTIALS
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let session_cookie_only = cookie.split(';').next().unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            session_cookie_only,
            &session.csrf_token,
            CREDENTIALS
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let (cookie, session) = session_response(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            &cookie,
            &session.csrf_token,
            CREDENTIALS,
        )
        .await,
    )
    .await;
    assert_eq!(
        request(&app, "POST", "/protected", &cookie, "wrong", "")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/login",
            &cookie,
            &session.csrf_token,
            r#"{"username":"admin","password":"wrong"}"#
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/login",
            &cookie,
            &session.csrf_token,
            "{}"
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}
