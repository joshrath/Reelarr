use crate::{auth, state::AppState};
use axum::{Router, middleware};

/// Authentication endpoints stay public; domain routes require a valid session.
pub fn router(state: AppState) -> Router<AppState> {
    let protected = Router::new().layer(middleware::from_fn_with_state(state, auth::require_auth));
    auth::router().merge(protected)
}
