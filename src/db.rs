use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;

use crate::{
    errors::ApiError,
    models::{Namespace, NamespacePayload, Snapshot, SnapshotPayload, UpdateEvent},
};

pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sync_document (
            id SMALLINT PRIMARY KEY,
            version BIGINT NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            app_state JSONB NOT NULL,
            playlists JSONB NOT NULL,
            provider_configuration JSONB NOT NULL,
            settings JSONB NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO sync_document (
            id,
            version,
            updated_at,
            app_state,
            playlists,
            provider_configuration,
            settings
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .bind(1_i16)
    .bind(0_i64)
    .bind(Utc::now())
    .bind(json!({}))
    .bind(json!([]))
    .bind(json!({}))
    .bind(json!({}))
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn load_snapshot(pool: &PgPool) -> Result<Snapshot, ApiError> {
    let row = sqlx::query_as::<
        _,
        (
            i64,
            DateTime<Utc>,
            serde_json::Value,
            serde_json::Value,
            serde_json::Value,
            serde_json::Value,
        ),
    >(
        r#"
        SELECT
            version,
            updated_at,
            app_state,
            playlists,
            provider_configuration,
            settings
        FROM sync_document
        WHERE id = 1
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|err| ApiError::internal(format!("failed to read snapshot: {err}")))?;

    Ok(Snapshot {
        version: row.0,
        updated_at: row.1,
        app_state: row.2,
        playlists: row.3,
        provider_configuration: row.4,
        settings: row.5,
    })
}

pub async fn update_namespace(
    pool: &PgPool,
    namespace: Namespace,
    payload: NamespacePayload,
) -> Result<(Snapshot, UpdateEvent), ApiError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|err| ApiError::internal(format!("failed to start transaction: {err}")))?;

    let current = sqlx::query_as::<_, (i64,)>("SELECT version FROM sync_document WHERE id = 1")
        .fetch_one(&mut *transaction)
        .await
        .map_err(|err| ApiError::internal(format!("failed to read current version: {err}")))?
        .0;

    if let Some(expected) = payload.expected_version
        && expected != current
    {
        return Err(ApiError::conflict(format!(
            "expected version {expected}, but current version is {current}"
        )));
    }

    let source_client_id = payload.client_id.clone();

    let new_version = current + 1;
    let updated_at = Utc::now();
    let data = payload.data;

    let query = match namespace {
        Namespace::AppState => {
            "UPDATE sync_document SET app_state = $1, version = $2, updated_at = $3 WHERE id = 1"
        }
        Namespace::Playlists => {
            "UPDATE sync_document SET playlists = $1, version = $2, updated_at = $3 WHERE id = 1"
        }
        Namespace::ProviderConfiguration => {
            "UPDATE sync_document SET provider_configuration = $1, version = $2, updated_at = $3 WHERE id = 1"
        }
        Namespace::Settings => {
            "UPDATE sync_document SET settings = $1, version = $2, updated_at = $3 WHERE id = 1"
        }
        Namespace::Snapshot => unreachable!(),
    };

    sqlx::query(query)
        .bind(data)
        .bind(new_version)
        .bind(updated_at)
        .execute(&mut *transaction)
        .await
        .map_err(|err| ApiError::internal(format!("failed to update snapshot: {err}")))?;

    transaction
        .commit()
        .await
        .map_err(|err| ApiError::internal(format!("failed to commit update: {err}")))?;

    let snapshot = load_snapshot(pool).await?;
    let event = UpdateEvent {
        event_type: "state_updated",
        namespace,
        version: new_version,
        updated_at,
        source_client_id,
    };

    Ok((snapshot, event))
}

pub async fn replace_snapshot(
    pool: &PgPool,
    payload: SnapshotPayload,
) -> Result<(Snapshot, UpdateEvent), ApiError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|err| ApiError::internal(format!("failed to start transaction: {err}")))?;

    let current = sqlx::query_as::<_, (i64,)>("SELECT version FROM sync_document WHERE id = 1")
        .fetch_one(&mut *transaction)
        .await
        .map_err(|err| ApiError::internal(format!("failed to read current version: {err}")))?
        .0;

    if let Some(expected) = payload.expected_version
        && expected != current
    {
        return Err(ApiError::conflict(format!(
            "expected version {expected}, but current version is {current}"
        )));
    }

    let source_client_id = payload.client_id.clone();
    let new_version = current + 1;
    let updated_at = Utc::now();

    sqlx::query(
        r#"
        UPDATE sync_document
        SET
            app_state = $1,
            playlists = $2,
            provider_configuration = $3,
            settings = $4,
            version = $5,
            updated_at = $6
        WHERE id = 1
        "#,
    )
    .bind(payload.app_state)
    .bind(payload.playlists)
    .bind(payload.provider_configuration)
    .bind(payload.settings)
    .bind(new_version)
    .bind(updated_at)
    .execute(&mut *transaction)
    .await
    .map_err(|err| ApiError::internal(format!("failed to write snapshot: {err}")))?;

    transaction
        .commit()
        .await
        .map_err(|err| ApiError::internal(format!("failed to commit update: {err}")))?;

    let snapshot = load_snapshot(pool).await?;
    let event = UpdateEvent {
        event_type: "state_updated",
        namespace: Namespace::Snapshot,
        version: new_version,
        updated_at,
        source_client_id,
    };

    Ok((snapshot, event))
}
