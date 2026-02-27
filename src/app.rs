use std::sync::Arc;

use axum::{Router, extract::DefaultBodyLimit, http::HeaderValue, routing::get};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use tracing::warn;

use crate::{handlers, state::AppContext};

pub fn build_router(
    state: Arc<AppContext>,
    cors_allowed_origins: Vec<String>,
    max_body_size: usize,
) -> Router {
    let cors = if cors_allowed_origins.is_empty() {
        warn!(
            "CORS_ALLOWED_ORIGINS is not set — all origins are permitted. \
             Set CORS_ALLOWED_ORIGINS to a comma-separated list of allowed origins in production."
        );
        CorsLayer::permissive()
    } else {
        let origins: Vec<HeaderValue> = cors_allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        CorsLayer::new().allow_origin(AllowOrigin::list(origins))
    };

    Router::new()
        .route("/health", get(handlers::health))
        .route(
            "/v1/snapshot",
            get(handlers::get_snapshot).put(handlers::put_snapshot),
        )
        .route(
            "/v1/state/:namespace",
            get(handlers::get_namespace).put(handlers::put_namespace),
        )
        .route("/v1/ws", get(handlers::ws_updates))
        .layer(DefaultBodyLimit::max(max_body_size))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
