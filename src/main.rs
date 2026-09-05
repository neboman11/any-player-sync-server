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

/// Hashes and stat's an operator-configured DJ model file once at startup (used for
/// both the LLM model and the neural voice bundle). Returns `None` (with a warning)
/// if the path is unset or the file can't be read - the corresponding download
/// endpoints then just report "not configured" rather than failing server startup,
/// since these features are entirely optional.
fn load_dj_model_info(path: Option<&std::path::PathBuf>, version: &str) -> Option<DjModelInfo> {
    let path = path?;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %path.display(), %err, "configured model path set but file could not be opened");
            return None;
        }
    };
    let size_bytes = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) => {
            warn!(path = %path.display(), %err, "failed to stat model file");
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
                warn!(path = %path.display(), %err, "failed to hash model file");
                return None;
            }
        };
        hasher.update(&buf[..read]);
    }
    let sha256 = format!("{:x}", hasher.finalize());

    Some(DjModelInfo {
        path: path.clone(),
        version: version.to_string(),
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

    let dj_model = load_dj_model_info(config.dj_model_path.as_ref(), &config.dj_model_version);
    if config.dj_model_path.is_some() && dj_model.is_none() {
        warn!(
            "DJ_MODEL_PATH was set but could not be loaded - AI DJ model endpoints will report not-configured"
        );
    } else if let Some(ref model) = dj_model {
        info!(version = %model.version, size_bytes = model.size_bytes, "DJ model loaded");
    }

    let dj_voice_model = load_dj_model_info(
        config.dj_voice_model_path.as_ref(),
        &config.dj_voice_model_version,
    );
    if config.dj_voice_model_path.is_some() && dj_voice_model.is_none() {
        warn!(
            "DJ_VOICE_MODEL_PATH was set but could not be loaded - AI DJ voice model endpoints will report not-configured"
        );
    } else if let Some(ref model) = dj_voice_model {
        info!(version = %model.version, size_bytes = model.size_bytes, "DJ voice model loaded");
    }

    let state = Arc::new(AppContext::new(pool, dj_model, dj_voice_model));

    let app = build_router(state, config.cors_allowed_origins, config.max_body_size);

    info!(address = %config.bind_address, "sync server listening");
    let listener = tokio::net::TcpListener::bind(config.bind_address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::load_dj_model_info;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn loads_voice_bundle_metadata() {
        let path = std::env::temp_dir().join(format!(
            "any-player-voice-model-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ));
        fs::write(&path, b"voice-model-fixture\n").expect("write voice model fixture");

        let info = load_dj_model_info(Some(&path), "voice-v1").expect("load voice model fixture");

        assert_eq!(info.version, "voice-v1");
        assert_eq!(info.size_bytes, 20);
        assert_eq!(
            info.sha256,
            "590cde0323c8ece1ed91c67448110d2247fe64708ce2310d1e988df0d8e3c0bb"
        );
        fs::remove_file(path).expect("remove voice model fixture");
    }
}
