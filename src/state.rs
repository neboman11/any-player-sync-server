use std::collections::HashMap;
use std::sync::RwLock;

use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::models::UpdateEvent;

/// Capacity per-user broadcast channel. Slow WebSocket clients that fall more
/// than this many messages behind will receive a Lagged error and must refresh
/// via a full snapshot.
const USER_CHANNEL_CAPACITY: usize = 64;

pub struct AppContext {
    pub pool: PgPool,
    user_channels: RwLock<HashMap<i64, broadcast::Sender<UpdateEvent>>>,
}

impl AppContext {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            user_channels: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to update events for the given user. Creates a channel for
    /// that user if one does not already exist.
    pub fn subscribe_user(&self, user_id: i64) -> broadcast::Receiver<UpdateEvent> {
        let mut map = self
            .user_channels
            .write()
            .expect("user_channels lock poisoned");
        map.entry(user_id)
            .or_insert_with(|| broadcast::channel(USER_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Send an update event to all active WebSocket connections for this user.
    /// If no channel exists for the user (no active subscribers), the event is
    /// silently dropped.
    pub fn send_user_event(&self, user_id: i64, event: UpdateEvent) {
        let map = self
            .user_channels
            .read()
            .expect("user_channels lock poisoned");
        if let Some(tx) = map.get(&user_id) {
            let _ = tx.send(event);
        }
    }
}
