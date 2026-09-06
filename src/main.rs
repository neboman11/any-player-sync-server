mod app;
mod config;
mod db;
mod errors;
mod handlers;
mod models;
mod shutdown;
mod state;
mod ws;

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

use crate::{
    app::build_router,
    config::AppConfig,
    db::{ensure_bootstrap_admin, ensure_schema},
    models::{DjVoiceCatalogManifest, DjVoiceDescriptor},
    shutdown::shutdown_signal,
    state::{AppContext, DjModelInfo, DjVoiceCatalog, DjVoiceModel},
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
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => {
            warn!(path = %path.display(), "configured model path is not a regular file");
            return None;
        }
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

fn is_safe_voice_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn load_dj_voice_catalog(
    manifest_path: Option<&std::path::PathBuf>,
    legacy_path: Option<&std::path::PathBuf>,
    legacy_version: &str,
) -> anyhow::Result<DjVoiceCatalog> {
    let Some(manifest_path) = manifest_path else {
        if legacy_path.is_some() && !is_safe_voice_component(legacy_version) {
            anyhow::bail!("legacy voice model version is unsafe");
        }
        let voices: Vec<_> = load_dj_model_info(legacy_path, legacy_version)
            .map(|info| {
                DjVoiceModel::new(
                    DjVoiceDescriptor {
                        id: "default".to_string(),
                        name: "Default".to_string(),
                        version: info.version,
                        size_bytes: info.size_bytes,
                        sha256: info.sha256,
                    },
                    info.path,
                )
            })
            .into_iter()
            .collect();
        return Ok(DjVoiceCatalog {
            default_id: (!voices.is_empty()).then(|| "default".to_string()),
            voices,
        });
    };

    let source = std::fs::read_to_string(manifest_path).map_err(|err| {
        anyhow::anyhow!(
            "failed to read voice manifest {}: {err}",
            manifest_path.display()
        )
    })?;
    let manifest: DjVoiceCatalogManifest = serde_json::from_str(&source).map_err(|err| {
        anyhow::anyhow!("invalid voice manifest {}: {err}", manifest_path.display())
    })?;

    if let Some(default_id) = &manifest.default_id
        && !is_safe_voice_component(default_id)
    {
        anyhow::bail!("voice manifest default_id is unsafe");
    }

    let mut ids = HashSet::new();
    for voice in &manifest.voices {
        if !is_safe_voice_component(&voice.id) || !is_safe_voice_component(&voice.version) {
            anyhow::bail!("voice manifest contains an unsafe id or version");
        }
        if !voice.path.is_absolute() {
            anyhow::bail!("voice manifest bundle paths must be absolute");
        }
        let metadata = std::fs::metadata(&voice.path).map_err(|err| {
            anyhow::anyhow!(
                "voice manifest bundle {} is unavailable: {err}",
                voice.path.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "voice manifest bundle {} is not a regular file",
                voice.path.display()
            );
        }
        if !ids.insert(voice.id.as_str()) {
            anyhow::bail!("voice manifest contains duplicate id '{}'", voice.id);
        }
    }
    if let Some(default_id) = &manifest.default_id
        && !ids.contains(default_id.as_str())
    {
        anyhow::bail!("voice manifest default_id is not in voices");
    }

    let voices = manifest
        .voices
        .into_iter()
        .filter_map(|voice| {
            let info = load_dj_model_info(Some(&voice.path), &voice.version)?;
            Some(DjVoiceModel::new(
                DjVoiceDescriptor {
                    id: voice.id,
                    name: voice.name,
                    version: info.version,
                    size_bytes: info.size_bytes,
                    sha256: info.sha256,
                },
                info.path,
            ))
        })
        .collect::<Vec<_>>();
    if voices.is_empty() {
        anyhow::bail!("voice manifest has no usable bundles");
    }
    if let Some(default_id) = &manifest.default_id
        && !voices
            .iter()
            .any(|voice| voice.descriptor.id == *default_id)
    {
        anyhow::bail!("voice manifest default_id bundle is unavailable");
    }
    Ok(DjVoiceCatalog {
        default_id: manifest.default_id,
        voices,
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

    let dj_voice_catalog = load_dj_voice_catalog(
        config.dj_voice_models_manifest_path.as_ref(),
        config.dj_voice_model_path.as_ref(),
        &config.dj_voice_model_version,
    )?;
    if let Some(ref default_id) = dj_voice_catalog.default_id {
        info!(
            %default_id,
            voices = dj_voice_catalog.voices.len(),
            "DJ voice catalog loaded"
        );
    } else {
        info!(
            voices = dj_voice_catalog.voices.len(),
            "DJ voice catalog loaded"
        );
    }

    let state = Arc::new(AppContext::new(pool, dj_model, dj_voice_catalog));

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
    use super::{load_dj_model_info, load_dj_voice_catalog};
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

    fn manifest_with_ids(ids: &[&str], bundle_path: &std::path::Path) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "any-player-voice-manifest-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        ));
        let voices = ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"id":"{id}","name":"Voice","version":"v1","path":"{}"}}"#,
                    bundle_path.display()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            &path,
            format!(r#"{{"default_id":null,"voices":[{voices}]}}"#),
        )
        .expect("write voice manifest");
        path
    }

    #[test]
    fn manifest_rejects_duplicate_and_unsafe_ids() {
        let bundle = std::env::temp_dir().join(format!(
            "any-player-voice-bundle-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        ));
        fs::write(&bundle, b"voice-model-fixture\n").expect("write voice bundle");
        let duplicate = manifest_with_ids(&["deep", "deep"], &bundle);
        let error = match load_dj_voice_catalog(Some(&duplicate), None, "unversioned") {
            Err(error) => error,
            Ok(_) => panic!("duplicate ids are rejected before valid bundles load"),
        };
        assert!(error.to_string().contains("duplicate id"));
        fs::remove_file(duplicate).expect("remove duplicate manifest");
        fs::remove_file(bundle).expect("remove duplicate bundle");

        let unsafe_id = manifest_with_ids(&["../escape"], std::path::Path::new("/missing.zip"));
        assert!(load_dj_voice_catalog(Some(&unsafe_id), None, "unversioned").is_err());
        fs::remove_file(unsafe_id).expect("remove unsafe manifest");
    }

    #[test]
    fn legacy_voice_model_rejects_unsafe_version() {
        let bundle = std::env::temp_dir().join(format!(
            "any-player-legacy-voice-bundle-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        ));
        fs::write(&bundle, b"voice-model-fixture\n").expect("write legacy voice bundle");
        assert!(load_dj_voice_catalog(None, Some(&bundle), "../escape").is_err());
        fs::remove_file(bundle).expect("remove legacy voice bundle");
    }
}
