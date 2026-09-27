//! Test helpers for DB-backed passage tests. Tests return early (pass) without TEST_DATABASE_URL.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

use crate::db::{create_token, create_user, ensure_schema};

/// Serializes DB-touching passage tests against each other. `cargo test` runs test functions
/// concurrently by default, but some tests (e.g. the schema advisory-lock regression test) drop
/// and recreate the shared passage tables, which would otherwise corrupt sibling tests reading
/// or writing those tables mid-flight.
fn db_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Connects and ensures the schema, returning the pool plus a guard the caller must hold for
/// the rest of the test (dropping it early re-allows other DB tests to interleave).
pub async fn test_pool() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = db_test_lock().lock().await;
    let pool = PgPool::connect(&url).await.expect("test database");
    ensure_schema(&pool).await.expect("base schema");
    assert!(
        super::schema::ensure(&pool).await.expect("passage schema"),
        "pgvector missing"
    );
    Some((pool, guard))
}

/// Creates a uniquely named user and returns (user_id, bearer token).
#[allow(dead_code)]
pub async fn user_with_token(pool: &PgPool, admin: bool) -> (i64, String) {
    let name = format!("passage-test-{}", rand::random::<u64>());
    let user = create_user(pool, &name, admin).await.expect("user");
    let token = create_token(pool, user.id, None).await.expect("token").3;
    (user.id, token)
}

#[allow(dead_code)]
pub fn bearer(token: &str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}
