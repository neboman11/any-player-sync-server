mod app;
mod config;
mod db;
mod dj_fact_sources;
mod dj_facts;
mod errors;
mod handlers;
mod match_keys;
mod models;
mod passages;
mod shutdown;
mod state;
mod ws;

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info, warn};

use crate::{
    app::build_router,
    config::AppConfig,
    db::{ensure_bootstrap_admin, ensure_schema},
    models::{DjCatalogDescriptor, DjCatalogManifest},
    shutdown::shutdown_signal,
    state::{AppContext, DjCatalog, DjCatalogEntry, DjModelInfo},
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

/// Loads an operator-owned AI DJ catalog (`kind` is `voice` or `model`, for messages).
/// Without a manifest, the legacy single-file `DJ_*_PATH`/`DJ_*_VERSION` config becomes a
/// one-entry catalog with ID `default`. `formats`, when non-empty, restricts entry files
/// to those extensions (script models must be a runtime-loadable `.task`/`.litertlm`).
fn load_dj_catalog(
    kind: &str,
    manifest_path: Option<&std::path::PathBuf>,
    legacy_path: Option<&std::path::PathBuf>,
    legacy_version: &str,
    formats: &[&str],
) -> anyhow::Result<DjCatalog> {
    let has_format = |path: &std::path::Path| {
        formats.is_empty()
            || path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| formats.contains(&ext))
    };
    let Some(manifest_path) = manifest_path else {
        if let Some(path) = legacy_path {
            if !is_safe_voice_component(legacy_version) {
                anyhow::bail!("legacy {kind} version is unsafe");
            }
            if !has_format(path) {
                anyhow::bail!("legacy {kind} file must be one of: {}", formats.join(", "));
            }
        }
        let entries: Vec<_> = load_dj_model_info(legacy_path, legacy_version)
            .map(|info| {
                DjCatalogEntry::new(
                    DjCatalogDescriptor {
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
        return Ok(DjCatalog {
            default_id: (!entries.is_empty()).then(|| "default".to_string()),
            entries,
        });
    };

    let source = std::fs::read_to_string(manifest_path).map_err(|err| {
        anyhow::anyhow!(
            "failed to read {kind} manifest {}: {err}",
            manifest_path.display()
        )
    })?;
    let manifest: DjCatalogManifest = serde_json::from_str(&source).map_err(|err| {
        anyhow::anyhow!("invalid {kind} manifest {}: {err}", manifest_path.display())
    })?;

    if let Some(default_id) = &manifest.default_id
        && !is_safe_voice_component(default_id)
    {
        anyhow::bail!("{kind} manifest default_id is unsafe");
    }

    let mut ids = HashSet::new();
    for entry in &manifest.entries {
        if !is_safe_voice_component(&entry.id) || !is_safe_voice_component(&entry.version) {
            anyhow::bail!("{kind} manifest contains an unsafe id or version");
        }
        if !entry.path.is_absolute() {
            anyhow::bail!("{kind} manifest paths must be absolute");
        }
        if !has_format(&entry.path) {
            anyhow::bail!(
                "{kind} manifest file {} must be one of: {}",
                entry.path.display(),
                formats.join(", ")
            );
        }
        let metadata = std::fs::metadata(&entry.path).map_err(|err| {
            anyhow::anyhow!(
                "{kind} manifest file {} is unavailable: {err}",
                entry.path.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "{kind} manifest file {} is not a regular file",
                entry.path.display()
            );
        }
        if !ids.insert(entry.id.as_str()) {
            anyhow::bail!("{kind} manifest contains duplicate id '{}'", entry.id);
        }
    }
    if let Some(default_id) = &manifest.default_id
        && !ids.contains(default_id.as_str())
    {
        anyhow::bail!("{kind} manifest default_id is not in the manifest");
    }

    let entries = manifest
        .entries
        .into_iter()
        .filter_map(|entry| {
            let info = load_dj_model_info(Some(&entry.path), &entry.version)?;
            Some(DjCatalogEntry::new(
                DjCatalogDescriptor {
                    id: entry.id,
                    name: entry.name,
                    version: info.version,
                    size_bytes: info.size_bytes,
                    sha256: info.sha256,
                },
                info.path,
            ))
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        anyhow::bail!("{kind} manifest has no usable files");
    }
    if let Some(default_id) = &manifest.default_id
        && !entries
            .iter()
            .any(|entry| entry.descriptor.id == *default_id)
    {
        anyhow::bail!("{kind} manifest default_id file is unavailable");
    }
    Ok(DjCatalog {
        default_id: manifest.default_id,
        entries,
    })
}

fn load_dj_voice_catalog(
    manifest_path: Option<&std::path::PathBuf>,
    legacy_path: Option<&std::path::PathBuf>,
    legacy_version: &str,
) -> anyhow::Result<DjCatalog> {
    load_dj_catalog("voice", manifest_path, legacy_path, legacy_version, &[])
}

/// File extensions the Android app can run: MediaPipe `.task` or LiteRT-LM `.litertlm`.
const DJ_MODEL_FORMATS: &[&str] = &["task", "litertlm"];

fn load_dj_model_catalog(
    manifest_path: Option<&std::path::PathBuf>,
    legacy_path: Option<&std::path::PathBuf>,
    legacy_version: &str,
) -> anyhow::Result<DjCatalog> {
    load_dj_catalog(
        "model",
        manifest_path,
        legacy_path,
        legacy_version,
        DJ_MODEL_FORMATS,
    )
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

    let dj_model_catalog = load_dj_model_catalog(
        config.dj_models_manifest_path.as_ref(),
        config.dj_model_path.as_ref(),
        &config.dj_model_version,
    )?;
    info!(
        default_id = ?dj_model_catalog.default_id,
        models = dj_model_catalog.entries.len(),
        "DJ model catalog loaded"
    );

    let dj_voice_catalog = load_dj_voice_catalog(
        config.dj_voice_models_manifest_path.as_ref(),
        config.dj_voice_model_path.as_ref(),
        &config.dj_voice_model_version,
    )?;
    info!(
        default_id = ?dj_voice_catalog.default_id,
        voices = dj_voice_catalog.entries.len(),
        "DJ voice catalog loaded"
    );

    let passages = match (
        passages::schema::ensure(&pool).await?,
        &config.dj_embedding_model_dir,
    ) {
        (false, _) => {
            warn!("pgvector extension missing; DJ passages disabled");
            None
        }
        (true, None) => {
            warn!("DJ_EMBEDDING_MODEL_DIR not set; DJ passages disabled");
            None
        }
        (true, Some(dir)) => {
            let dir = dir.clone();
            match tokio::task::spawn_blocking(move || passages::embed::BertEmbedder::load(&dir))
                .await?
            {
                Ok(model) => Some(Arc::new(passages::Passages {
                    embedder: Arc::new(model),
                })),
                Err(err) => {
                    error!(%err, "failed to load DJ embedding model; DJ passages disabled");
                    None
                }
            }
        }
    };
    if let Some(engine) = &passages {
        let sources = Arc::new(passages::sources::Sources::from_config(
            config.lastfm_api_key.clone(),
            config.genius_token.clone(),
        ));
        tokio::spawn(passages::worker::run(
            pool.clone(),
            engine.embedder.clone(),
            sources,
        ));
        info!("DJ passage ingest worker started");
    }

    let state =
        Arc::new(AppContext::new(pool, dj_model_catalog, dj_voice_catalog).with_passages(passages));

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
    use super::{load_dj_model_catalog, load_dj_model_info, load_dj_voice_catalog};
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

    #[test]
    fn model_manifest_accepts_only_runtime_formats() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let manifest = std::env::temp_dir().join(format!("any-player-model-manifest-{stamp}"));
        for (extension, accepted) in [("litertlm", true), ("task", true), ("zip", false)] {
            let model = std::env::temp_dir().join(format!("any-player-model-{stamp}.{extension}"));
            fs::write(&model, b"model-fixture\n").expect("write model fixture");
            fs::write(
                &manifest,
                format!(
                    r#"{{"default_id":"gemma","models":[{{"id":"gemma","name":"Gemma","version":"v1","path":"{}"}}]}}"#,
                    model.display()
                ),
            )
            .expect("write model manifest");
            let result = load_dj_model_catalog(Some(&manifest), None, "unversioned");
            assert_eq!(result.is_ok(), accepted, "{extension}");
            if let Ok(catalog) = result {
                assert_eq!(catalog.entries[0].format(), extension);
            }
            fs::remove_file(model).expect("remove model fixture");
        }
        fs::remove_file(manifest).expect("remove model manifest");
    }
}
