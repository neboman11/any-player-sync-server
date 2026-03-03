use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::models::UserScopedUpdateEvent;

#[derive(Clone)]
pub struct AppContext {
    pub pool: PgPool,
    pub updates_tx: broadcast::Sender<UserScopedUpdateEvent>,
}
