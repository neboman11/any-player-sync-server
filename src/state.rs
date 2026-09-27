use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::{RwLock, broadcast};

use crate::models::{DjCatalogDescriptor, UpdateEvent};

/// Capacity per-user broadcast channel. Slow WebSocket clients that fall more
/// than this many messages behind will receive a Lagged error and must refresh
/// via a full snapshot.
const USER_CHANNEL_CAPACITY: usize = 64;

/// Metadata for an operator-configured AI DJ file, computed once at startup so repeated
/// catalog/info calls don't re-hash a large file.
#[derive(Clone)]
pub struct DjModelInfo {
    pub path: PathBuf,
    pub version: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct DjCatalogEntry {
    pub descriptor: DjCatalogDescriptor,
    path: PathBuf,
}

impl DjCatalogEntry {
    pub(crate) fn new(descriptor: DjCatalogDescriptor, path: PathBuf) -> Self {
        Self { descriptor, path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The file extension (e.g. `task`/`litertlm`), validated at load for script models.
    pub(crate) fn format(&self) -> String {
        self.path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_string()
    }
}

#[derive(Clone, Default)]
pub struct DjCatalog {
    pub default_id: Option<String>,
    pub entries: Vec<DjCatalogEntry>,
}

impl DjCatalog {
    pub fn default_model(&self) -> Option<&DjCatalogEntry> {
        self.default_id.as_deref().and_then(|id| self.find(id))
    }

    pub fn find(&self, id: &str) -> Option<&DjCatalogEntry> {
        self.entries.iter().find(|entry| entry.descriptor.id == id)
    }
}

pub struct AppContext {
    pub pool: PgPool,
    /// Operator-configured AI DJ script-generation models (`.task`/`.litertlm`).
    pub dj_model_catalog: DjCatalog,
    /// Operator-configured AI DJ neural voice bundles (Piper/VITS `.onnx` + `tokens.txt`
    /// zipped together).
    pub dj_voice_catalog: DjCatalog,
    /// AI DJ passage retrieval and admin ingest (None when pgvector or the embedding model is
    /// missing).
    pub passages: Option<Arc<crate::passages::Passages>>,
    user_channels: RwLock<HashMap<i64, broadcast::Sender<UpdateEvent>>>,
}

impl AppContext {
    pub fn new(pool: PgPool, dj_model_catalog: DjCatalog, dj_voice_catalog: DjCatalog) -> Self {
        Self {
            pool,
            dj_model_catalog,
            dj_voice_catalog,
            passages: None,
            user_channels: RwLock::new(HashMap::new()),
        }
    }

    /// Enables DJ passage retrieval and ingestion (None when pgvector or the model is missing).
    pub fn with_passages(mut self, passages: Option<Arc<crate::passages::Passages>>) -> Self {
        self.passages = passages;
        self
    }

    /// Subscribe to update events for the given user. Creates a channel for
    /// that user if one does not already exist.
    pub async fn subscribe_user(&self, user_id: i64) -> broadcast::Receiver<UpdateEvent> {
        let mut map = self.user_channels.write().await;
        map.entry(user_id)
            .or_insert_with(|| broadcast::channel(USER_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Send an update event to all active WebSocket connections for this user.
    /// If no channel exists for the user (no active subscribers), the event is
    /// silently dropped. Stale channel entries (no remaining receivers) are
    /// removed to prevent unbounded map growth.
    pub async fn send_user_event(&self, user_id: i64, event: UpdateEvent) {
        // Fast path: try to send under a read lock.
        let map = self.user_channels.read().await;

        let mut should_cleanup = false;

        if let Some(tx) = map.get(&user_id) {
            // If send fails and there are no receivers, this sender is stale.
            if tx.send(event).is_err() && tx.receiver_count() == 0 {
                should_cleanup = true;
            }
        }

        // Drop the read lock before potentially taking a write lock.
        drop(map);

        if should_cleanup {
            let mut map = self.user_channels.write().await;

            if let Some(tx) = map.get(&user_id)
                && tx.receiver_count() == 0
            {
                map.remove(&user_id);
            }
        }
    }
}
