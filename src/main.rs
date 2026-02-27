mod app;
mod config;
mod db;
mod errors;
mod handlers;
mod models;
mod shutdown;
mod state;
mod ws;

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::broadcast;
use tracing::info;

use crate::{
    app::build_router, config::AppConfig, db::ensure_schema, shutdown::shutdown_signal,
    state::AppContext,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config = AppConfig::from_env()?;

    let pool = PgPool::connect(&config.database_url).await.map_err(|err| {
        anyhow::anyhow!(
            "failed to connect to postgres ({}): {err}",
            config.database_url_safe
        )
    })?;
    ensure_schema(&pool).await?;

    // A capacity of 512 messages is used for the broadcast channel. Slow or
    // disconnected WebSocket clients that fall more than 512 messages behind will
    // receive a RecvError::Lagged error and must refresh from a full snapshot on
    // reconnection.
    let (updates_tx, _) = broadcast::channel(512);
    let state = Arc::new(AppContext { pool, updates_tx });

    let app = build_router(state, config.cors_allowed_origins, config.max_body_size);

    info!(address = %config.bind_address, "sync server listening");
    let listener = tokio::net::TcpListener::bind(config.bind_address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
