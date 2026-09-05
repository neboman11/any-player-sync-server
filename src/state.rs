use std::collections::HashMap;
use std::path::PathBuf;

use sqlx::PgPool;
use tokio::sync::{RwLock, broadcast};

use crate::models::UpdateEvent;

/// Capacity per-user broadcast channel. Slow WebSocket clients that fall more
/// than this many messages behind will receive a Lagged error and must refresh
/// via a full snapshot.
const USER_CHANNEL_CAPACITY: usize = 64;

/// Metadata for the operator-configured AI DJ on-device model file, computed once
/// at startup so repeated `/v1/dj-model/info` calls don't re-hash a large file.
#[derive(Clone)]
pub struct DjModelInfo {
    pub path: PathBuf,
    pub version: String,
    pub size_bytes: u64,
    pub sha256: String,
}

pub struct AppContext {
    pub pool: PgPool,
    pub dj_model: Option<DjModelInfo>,
    /// Operator-configured AI DJ neural voice bundle (Piper/VITS `.onnx` + `tokens.txt`
    /// zipped together), served the same way as [dj_model] but via `/v1/dj-voice-model/*`.
    pub dj_voice_model: Option<DjModelInfo>,
    user_channels: RwLock<HashMap<i64, broadcast::Sender<UpdateEvent>>>,
}

impl AppContext {
    pub fn new(
        pool: PgPool,
        dj_model: Option<DjModelInfo>,
        dj_voice_model: Option<DjModelInfo>,
    ) -> Self {
        Self {
            pool,
            dj_model,
            dj_voice_model,
            user_channels: RwLock::new(HashMap::new()),
        }
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
