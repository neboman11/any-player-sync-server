use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Method, Request, header},
    routing::{get, patch, post},
};
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
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
    };

    Router::new()
        .route("/health", get(handlers::health))
        .route("/admin/login", get(handlers::admin_login))
        .route("/admin", get(handlers::admin_index))
        .route(
            "/v1/admin/users",
            get(handlers::admin_list_users).post(handlers::admin_create_user),
        )
        .route(
            "/v1/admin/users/:user_id/tokens",
            post(handlers::admin_create_token),
        )
        .route(
            "/v1/admin/users/:user_id/disabled",
            patch(handlers::admin_set_user_disabled),
        )
        .route(
            "/v1/admin/tokens/:token_id",
            axum::routing::delete(handlers::admin_revoke_token),
        )
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
        .layer(
            // Redact query strings from /v1/ws spans to avoid logging bearer
            // tokens that may be passed via the `token` query parameter.
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let uri = if request.uri().path() == "/v1/ws" {
                    request.uri().path().to_owned()
                } else {
                    request.uri().to_string()
                };
                tracing::info_span!("request", method = %request.method(), uri = %uri)
            }),
        )
        .with_state(state)
}
