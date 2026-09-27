//! Passage-store tables. The `vector` extension is not trusted and the app role is not a
//! superuser, so production creates it at cluster bootstrap (VectorChord image, `CREATE EXTENSION vchord CASCADE`); here we only
//! try (which succeeds for superuser test databases) and report whether it is present.

use sqlx::PgPool;

/// Advisory lock key serializing concurrent `ensure` calls (e.g. two server replicas racing
/// against a fresh database) so their `CREATE TABLE IF NOT EXISTS` statements don't collide on
/// Postgres's catalog (duplicate key on `pg_type`).
const DJ_PASSAGE_SCHEMA_LOCK: i64 = 0x444a5041; // "DJPA"

pub async fn ensure(pool: &PgPool) -> anyhow::Result<bool> {
    let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(pool)
        .await;
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(pool)
            .await?;
    if !present {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DJ_PASSAGE_SCHEMA_LOCK)
        .execute(&mut *tx)
        .await?;
    for statement in SCHEMA {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    // A crash mid-job leaves it `running`. That is not reset here: a rolling-update replica
    // could otherwise re-queue a job another (still live) replica is actively processing,
    // processing it twice. jobs::claim instead takes over a `running` job once its lease is
    // stale (see STALE_LEASE_MINUTES there).
    tx.commit().await?;
    Ok(true)
}

const SCHEMA: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS dj_documents (
        id BIGSERIAL PRIMARY KEY,
        source TEXT NOT NULL,
        source_ref TEXT NOT NULL,
        source_url TEXT NOT NULL,
        subject TEXT NOT NULL CHECK (subject IN ('song', 'album', 'artist')),
        song_key TEXT,
        album_key TEXT,
        artist_key TEXT NOT NULL,
        title TEXT NOT NULL,
        lang TEXT NOT NULL,
        body TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        added_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
        UNIQUE (source, source_ref)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_dj_documents_song ON dj_documents (subject, song_key, artist_key)",
    "CREATE INDEX IF NOT EXISTS idx_dj_documents_album ON dj_documents (subject, album_key, artist_key)",
    "CREATE INDEX IF NOT EXISTS idx_dj_documents_artist ON dj_documents (subject, artist_key)",
    r#"CREATE TABLE IF NOT EXISTS dj_chunks (
        id BIGSERIAL PRIMARY KEY,
        document_id BIGINT NOT NULL REFERENCES dj_documents(id) ON DELETE CASCADE,
        ord INT NOT NULL,
        text TEXT NOT NULL,
        token_count INT NOT NULL,
        embedding vector(384) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_dj_chunks_document ON dj_chunks (document_id)",
    r#"CREATE TABLE IF NOT EXISTS dj_passage_plays (
        user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        event_id TEXT NOT NULL,
        chunk_id BIGINT NOT NULL REFERENCES dj_chunks(id) ON DELETE CASCADE,
        played_at TIMESTAMPTZ NOT NULL,
        PRIMARY KEY (user_id, event_id, chunk_id)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_dj_passage_plays_reuse ON dj_passage_plays (user_id, chunk_id, played_at DESC)",
    r#"CREATE TABLE IF NOT EXISTS dj_ingest_jobs (
        id BIGSERIAL PRIMARY KEY,
        song TEXT NOT NULL,
        artist TEXT NOT NULL,
        album TEXT,
        song_key TEXT NOT NULL,
        artist_key TEXT NOT NULL,
        album_key TEXT NOT NULL DEFAULT '',
        requested_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
        auto BOOLEAN NOT NULL DEFAULT FALSE,
        status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
        attempts INT NOT NULL DEFAULT 0,
        next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        results JSONB NOT NULL DEFAULT '{}'::jsonb,
        last_error TEXT,
        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        UNIQUE (song_key, artist_key, album_key)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_dj_ingest_jobs_claim ON dj_ingest_jobs (status, next_attempt_at, id)",
    "CREATE INDEX IF NOT EXISTS idx_dj_ingest_jobs_auto ON dj_ingest_jobs (requested_by, created_at) WHERE auto",
];

#[cfg(test)]
mod tests {
    use crate::passages::testutil::test_pool;

    #[tokio::test]
    async fn ensure_creates_tables_and_is_idempotent() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        assert!(super::ensure(&pool).await.unwrap());
        assert!(super::ensure(&pool).await.unwrap());
        let tables: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_name IN \
             ('dj_documents','dj_chunks','dj_passage_plays','dj_ingest_jobs')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables, 4);
    }

    #[tokio::test]
    async fn ensure_is_safe_under_concurrent_callers() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        // Simulate a fresh database (or two server replicas racing to create the schema) by
        // dropping the passage tables, then racing several concurrent `ensure` calls.
        sqlx::query(
            "DROP TABLE IF EXISTS dj_passage_plays, dj_chunks, dj_documents, dj_ingest_jobs CASCADE",
        )
        .execute(&pool)
        .await
        .unwrap();
        let results = futures_util::future::join_all((0..8).map(|_| super::ensure(&pool))).await;
        for result in results {
            assert!(result.unwrap());
        }
    }
}
