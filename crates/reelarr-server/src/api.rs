use crate::{auth, state::AppState, stream};
use axum::{Router, middleware};

/// Authentication endpoints stay public; domain routes require a valid session.
pub fn router(state: AppState) -> Router<AppState> {
    // route_layer only wraps routes added before it, so add domain routes first.
    let protected = Router::new()
        .merge(stream::router())
        .route_layer(middleware::from_fn_with_state(state, auth::require_auth));
    auth::router().merge(protected)
}
