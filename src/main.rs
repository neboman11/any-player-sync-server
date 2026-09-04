mod app;
mod config;
mod db;
mod errors;
mod handlers;
mod models;
mod shutdown;
mod state;
mod ws;

use std::io::Read;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

use crate::{
    app::build_router,
    config::AppConfig,
    db::{ensure_bootstrap_admin, ensure_schema},
    shutdown::shutdown_signal,
    state::{AppContext, DjModelInfo},
};

/// Hashes and stat's the operator-configured DJ model file once at startup. Returns
/// `None` (with a warning) if `DJ_MODEL_PATH` is unset or the file can't be read -
/// the AI DJ model-download endpoints then just report "not configured" rather than
/// failing server startup, since this feature is entirely optional.
fn load_dj_model_info(config: &AppConfig) -> Option<DjModelInfo> {
    let path = config.dj_model_path.as_ref()?;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %path.display(), %err, "DJ_MODEL_PATH set but file could not be opened");
            return None;
        }
    };
    let size_bytes = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) => {
            warn!(path = %path.display(), %err, "failed to stat DJ model file");
            return None;
        }
    };

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                warn!(path = %path.display(), %err, "failed to hash DJ model file");
                return None;
            }
        };
        hasher.update(&buf[..read]);
    }
    let sha256 = format!("{:x}", hasher.finalize());

    Some(DjModelInfo {
        path: path.clone(),
        version: config.dj_model_version.clone(),
        size_bytes,
        sha256,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config = AppConfig::from_env()?;

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .min_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .idle_timeout(std::time::Duration::from_secs(600))
        .connect(&config.database_url)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "failed to connect to postgres ({}): {err}",
                config.database_url_safe
            )
        })?;
    ensure_schema(&pool).await?;
    ensure_bootstrap_admin(
        &pool,
        &config.admin_bootstrap_name,
        config.admin_bootstrap_token.as_deref(),
    )
    .await?;

    let dj_model = load_dj_model_info(&config);
    if config.dj_model_path.is_some() && dj_model.is_none() {
        warn!("DJ_MODEL_PATH was set but could not be loaded - AI DJ model endpoints will report not-configured");
    } else if let Some(ref model) = dj_model {
        info!(version = %model.version, size_bytes = model.size_bytes, "DJ model loaded");
    }

    let state = Arc::new(AppContext::new(pool, dj_model));

    let app = build_router(state, config.cors_allowed_origins, config.max_body_size);

    info!(address = %config.bind_address, "sync server listening");
    let listener = tokio::net::TcpListener::bind(config.bind_address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
