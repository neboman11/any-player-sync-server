use std::sync::Arc;

use axum::{Router, routing::get};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{handlers, state::AppContext};

pub fn build_router(state: Arc<AppContext>) -> Router {
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
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
