use chrono::{DateTime, Utc};
use rand::{Rng, distr::Alphanumeric};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::{
    errors::ApiError,
    models::{
        AuthenticatedUser, Namespace, NamespacePayload, Snapshot, SnapshotPayload, TokenInfo,
        UpdateEvent, UserCreatedResponse, UserSummary,
    },
};

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn token_prefix(token: &str) -> String {
    token.chars().take(8).collect()
}

fn generate_token() -> String {
    let random: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    format!("ap_{random}")
}

pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            is_admin BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            disabled_at TIMESTAMPTZ
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS auth_tokens (
            id BIGSERIAL PRIMARY KEY,
            user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            token_hash TEXT NOT NULL UNIQUE,
            token_prefix TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT '',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_used_at TIMESTAMPTZ,
            revoked_at TIMESTAMPTZ
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_auth_tokens_user_id
        ON auth_tokens(user_id);
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS user_sync_document (
            user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            id SMALLINT NOT NULL,
            version BIGINT NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            app_state JSONB NOT NULL,
            playlists JSONB NOT NULL,
            provider_configuration JSONB NOT NULL,
            settings JSONB NOT NULL,
            PRIMARY KEY (user_id, id)
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn ensure_bootstrap_admin(
    pool: &PgPool,
    admin_name: &str,
    admin_token: Option<&str>,
) -> anyhow::Result<()> {
    let Some(admin_token) = admin_token else {
        return Ok(());
    };

    let admin_name = admin_name.trim();
    if admin_name.is_empty() {
        anyhow::bail!("ADMIN_BOOTSTRAP_NAME cannot be empty when ADMIN_BOOTSTRAP_TOKEN is set");
    }

    let mut transaction = pool.begin().await?;

    let user_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO users (name, is_admin)
        VALUES ($1, TRUE)
        ON CONFLICT (name)
        DO UPDATE SET is_admin = TRUE
        RETURNING id
        "#,
    )
    .bind(admin_name)
    .fetch_one(&mut *transaction)
    .await?;

    let token_hash = hash_token(admin_token);
    let token_prefix = token_prefix(admin_token);

    sqlx::query(
        r#"
        INSERT INTO auth_tokens (user_id, token_hash, token_prefix, label, revoked_at)
        VALUES ($1, $2, $3, 'bootstrap', NULL)
        ON CONFLICT (token_hash)
        DO UPDATE SET user_id = $1, token_prefix = $3, label = 'bootstrap', revoked_at = NULL
        "#,
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(token_prefix)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    info!(admin_name, "ensured bootstrap admin user/token");
    Ok(())
}

async fn ensure_user_document(pool: &PgPool, user_id: i64) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO user_sync_document (
            user_id,
            id,
            version,
            updated_at,
            app_state,
            playlists,
            provider_configuration,
            settings
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (user_id, id) DO NOTHING
        "#,
    )
    .bind(user_id)
    .bind(1_i16)
    .bind(0_i64)
    .bind(Utc::now())
    .bind(json!({}))
    .bind(json!([]))
    .bind(json!({}))
    .bind(json!({}))
    .execute(pool)
    .await
    .map_err(|err| {
        error!(user_id, "failed to ensure user snapshot row: {err}");
        ApiError::internal("failed to ensure user snapshot row".to_string())
    })?;

    Ok(())
}

pub async fn authenticate_token(pool: &PgPool, token: &str) -> Result<AuthenticatedUser, ApiError> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(ApiError::unauthorized("missing bearer token".to_string()));
    }

    let token_hash = hash_token(trimmed);

    let row = sqlx::query_as::<_, (i64, String, bool, Option<DateTime<Utc>>, i64)>(
        r#"
        SELECT u.id, u.name, u.is_admin, u.disabled_at, t.id
        FROM auth_tokens t
        JOIN users u ON u.id = t.user_id
        WHERE t.token_hash = $1
          AND t.revoked_at IS NULL
        "#,
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await
    .map_err(|err| {
        error!("failed to validate token: {err}");
        ApiError::internal("failed to validate token".to_string())
    })?
    .ok_or_else(|| ApiError::unauthorized("invalid bearer token".to_string()))?;

    if row.3.is_some() {
        return Err(ApiError::forbidden("user account is disabled".to_string()));
    }

    let _ = sqlx::query("UPDATE auth_tokens SET last_used_at = NOW() WHERE id = $1")
        .bind(row.4)
        .execute(pool)
        .await;

    Ok(AuthenticatedUser {
        id: row.0,
        name: row.1,
        is_admin: row.2,
    })
}

pub async fn load_snapshot(pool: &PgPool, user_id: i64) -> Result<Snapshot, ApiError> {
    ensure_user_document(pool, user_id).await?;

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
        FROM user_sync_document
        WHERE user_id = $1
          AND id = 1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|err| {
        error!(user_id, "failed to read snapshot from database: {err}");
        ApiError::internal("failed to read snapshot".to_string())
    })?;

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
    user_id: i64,
    namespace: Namespace,
    payload: NamespacePayload,
) -> Result<(Snapshot, UpdateEvent), ApiError> {
    ensure_user_document(pool, user_id).await?;

    let mut transaction = pool.begin().await.map_err(|err| {
        error!("failed to start transaction: {err}");
        ApiError::internal("failed to start transaction".to_string())
    })?;

    let current = sqlx::query_as::<_, (i64,)>(
        "SELECT version FROM user_sync_document WHERE user_id = $1 AND id = 1",
    )
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|err| {
        error!(user_id, "failed to read current version: {err}");
        ApiError::internal("failed to read current version".to_string())
    })?
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
            "UPDATE user_sync_document SET app_state = $1, version = $2, updated_at = $3 WHERE user_id = $4 AND id = 1 \
             RETURNING version, updated_at, app_state, playlists, provider_configuration, settings"
        }
        Namespace::Playlists => {
            "UPDATE user_sync_document SET playlists = $1, version = $2, updated_at = $3 WHERE user_id = $4 AND id = 1 \
             RETURNING version, updated_at, app_state, playlists, provider_configuration, settings"
        }
        Namespace::ProviderConfiguration => {
            "UPDATE user_sync_document SET provider_configuration = $1, version = $2, updated_at = $3 WHERE user_id = $4 AND id = 1 \
             RETURNING version, updated_at, app_state, playlists, provider_configuration, settings"
        }
        Namespace::Settings => {
            "UPDATE user_sync_document SET settings = $1, version = $2, updated_at = $3 WHERE user_id = $4 AND id = 1 \
             RETURNING version, updated_at, app_state, playlists, provider_configuration, settings"
        }
        Namespace::Snapshot => unreachable!(),
    };

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
    >(query)
    .bind(data)
    .bind(new_version)
    .bind(updated_at)
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|err| {
        error!(user_id, "failed to update snapshot: {err}");
        ApiError::internal("failed to update snapshot".to_string())
    })?;

    transaction.commit().await.map_err(|err| {
        error!(user_id, "failed to commit update: {err}");
        ApiError::internal("failed to commit update".to_string())
    })?;

    let snapshot = Snapshot {
        version: row.0,
        updated_at: row.1,
        app_state: row.2,
        playlists: row.3,
        provider_configuration: row.4,
        settings: row.5,
    };
    let event = UpdateEvent {
        event_type: "state_updated".to_string(),
        namespace,
        version: new_version,
        updated_at,
        source_client_id,
    };

    Ok((snapshot, event))
}

pub async fn replace_snapshot(
    pool: &PgPool,
    user_id: i64,
    payload: SnapshotPayload,
) -> Result<(Snapshot, UpdateEvent), ApiError> {
    ensure_user_document(pool, user_id).await?;

    let mut transaction = pool.begin().await.map_err(|err| {
        error!("failed to start transaction: {err}");
        ApiError::internal("failed to start transaction".to_string())
    })?;

    let current = sqlx::query_as::<_, (i64,)>(
        "SELECT version FROM user_sync_document WHERE user_id = $1 AND id = 1",
    )
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|err| {
        error!(user_id, "failed to read current version: {err}");
        ApiError::internal("failed to read current version".to_string())
    })?
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
        UPDATE user_sync_document
        SET
            app_state = $1,
            playlists = $2,
            provider_configuration = $3,
            settings = $4,
            version = $5,
            updated_at = $6
        WHERE user_id = $7
          AND id = 1
        RETURNING version, updated_at, app_state, playlists, provider_configuration, settings
        "#,
    )
    .bind(payload.app_state)
    .bind(payload.playlists)
    .bind(payload.provider_configuration)
    .bind(payload.settings)
    .bind(new_version)
    .bind(updated_at)
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|err| {
        error!(user_id, "failed to write snapshot: {err}");
        ApiError::internal("failed to write snapshot".to_string())
    })?;

    transaction.commit().await.map_err(|err| {
        error!(user_id, "failed to commit update: {err}");
        ApiError::internal("failed to commit update".to_string())
    })?;

    let snapshot = Snapshot {
        version: row.0,
        updated_at: row.1,
        app_state: row.2,
        playlists: row.3,
        provider_configuration: row.4,
        settings: row.5,
    };
    let event = UpdateEvent {
        event_type: "state_updated".to_string(),
        namespace: Namespace::Snapshot,
        version: new_version,
        updated_at,
        source_client_id,
    };

    Ok((snapshot, event))
}

pub async fn list_users(pool: &PgPool) -> Result<Vec<UserSummary>, ApiError> {
    let users = sqlx::query_as::<_, (i64, String, bool, DateTime<Utc>, Option<DateTime<Utc>>)>(
        r#"
        SELECT id, name, is_admin, created_at, disabled_at
        FROM users
        ORDER BY name ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|err| {
        error!("failed to list users: {err}");
        ApiError::internal("failed to list users".to_string())
    })?;

    let tokens = sqlx::query_as::<
        _,
        (
            i64,
            i64,
            String,
            String,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
        ),
    >(
        r#"
        SELECT id, user_id, label, token_prefix, created_at, last_used_at, revoked_at
        FROM auth_tokens
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|err| {
        error!("failed to list tokens: {err}");
        ApiError::internal("failed to list tokens".to_string())
    })?;

    let mut by_user: std::collections::HashMap<i64, Vec<TokenInfo>> =
        std::collections::HashMap::new();
    for token in tokens {
        by_user.entry(token.1).or_default().push(TokenInfo {
            id: token.0,
            label: token.2,
            token_prefix: token.3,
            created_at: token.4,
            last_used_at: token.5,
            revoked_at: token.6,
        });
    }

    Ok(users
        .into_iter()
        .map(|user| UserSummary {
            id: user.0,
            name: user.1,
            is_admin: user.2,
            created_at: user.3,
            disabled_at: user.4,
            tokens: by_user.remove(&user.0).unwrap_or_default(),
        })
        .collect())
}

pub async fn create_user(
    pool: &PgPool,
    name: &str,
    is_admin: bool,
) -> Result<UserCreatedResponse, ApiError> {
    let normalized_name = name.trim();
    if normalized_name.is_empty() {
        return Err(ApiError::bad_request("name is required".to_string()));
    }

    let row = sqlx::query_as::<_, (i64, String, bool, DateTime<Utc>)>(
        r#"
        INSERT INTO users (name, is_admin)
        VALUES ($1, $2)
        RETURNING id, name, is_admin, created_at
        "#,
    )
    .bind(normalized_name)
    .bind(is_admin)
    .fetch_optional(pool)
    .await
    .map_err(|err| {
        if let sqlx::Error::Database(db_err) = &err {
            if db_err.code().as_deref() == Some("23505") {
                return ApiError::conflict(
                    "a user with that name already exists".to_string(),
                );
            }
        }
        error!("failed to create user: {err}");
        ApiError::internal("failed to create user".to_string())
    })?
    .ok_or_else(|| ApiError::internal("failed to create user".to_string()))?;

    Ok(UserCreatedResponse {
        id: row.0,
        name: row.1,
        is_admin: row.2,
        created_at: row.3,
    })
}

pub async fn set_user_disabled(
    pool: &PgPool,
    user_id: i64,
    disabled: bool,
) -> Result<(), ApiError> {
    let updated = sqlx::query(
        r#"
        UPDATE users
        SET disabled_at = CASE WHEN $2 THEN NOW() ELSE NULL END
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .bind(disabled)
    .execute(pool)
    .await
    .map_err(|err| {
        error!(user_id, "failed to update user disabled state: {err}");
        ApiError::internal("failed to update user".to_string())
    })?;

    if updated.rows_affected() == 0 {
        return Err(ApiError::not_found("user not found".to_string()));
    }

    Ok(())
}

pub async fn create_token(
    pool: &PgPool,
    user_id: i64,
    label: Option<String>,
) -> Result<(i64, String, String, String, DateTime<Utc>), ApiError> {
    let user_exists = sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|err| {
            error!(user_id, "failed to check user for token create: {err}");
            ApiError::internal("failed to create token".to_string())
        })?
        .is_some();

    if !user_exists {
        return Err(ApiError::not_found("user not found".to_string()));
    }

    let token = generate_token();
    let hash = hash_token(&token);
    let prefix = token_prefix(&token);
    let normalized_label = label.unwrap_or_else(|| "manual".to_string());

    let row = sqlx::query_as::<_, (i64, DateTime<Utc>)>(
        r#"
        INSERT INTO auth_tokens (user_id, token_hash, token_prefix, label)
        VALUES ($1, $2, $3, $4)
        RETURNING id, created_at
        "#,
    )
    .bind(user_id)
    .bind(hash)
    .bind(prefix.clone())
    .bind(normalized_label.clone())
    .fetch_one(pool)
    .await
    .map_err(|err| {
        error!(user_id, "failed to create token: {err}");
        ApiError::internal("failed to create token".to_string())
    })?;

    Ok((row.0, normalized_label, prefix, token, row.1))
}

pub async fn revoke_token(pool: &PgPool, token_id: i64) -> Result<(), ApiError> {
    let result = sqlx::query(
        r#"
        UPDATE auth_tokens
        SET revoked_at = NOW()
        WHERE id = $1
          AND revoked_at IS NULL
        "#,
    )
    .bind(token_id)
    .execute(pool)
    .await
    .map_err(|err| {
        error!(token_id, "failed to revoke token: {err}");
        ApiError::internal("failed to revoke token".to_string())
    })?;

    if result.rows_affected() == 0 {
        warn!(
            token_id,
            "token revoke requested for missing/already-revoked token"
        );
        return Err(ApiError::not_found("token not found".to_string()));
    }

    Ok(())
}
