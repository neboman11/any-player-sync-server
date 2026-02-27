use std::net::SocketAddr;

use anyhow::Context;

pub struct AppConfig {
    pub bind_address: SocketAddr,
    pub database_url: String,
    /// Same URL as `database_url` but with the password replaced by `****`, safe for logs.
    pub database_url_safe: String,
    /// Allowed CORS origins (comma-separated via `CORS_ALLOWED_ORIGINS`). Empty means all origins are allowed.
    pub cors_allowed_origins: Vec<String>,
    /// Maximum request body size in bytes (default: 1 MiB, configurable via `MAX_BODY_SIZE`).
    pub max_body_size: usize,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_address =
            std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8080".into());
        let db_host = std::env::var("DB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let db_port = std::env::var("DB_PORT").unwrap_or_else(|_| "5432".into());
        let db_user = std::env::var("DB_USER").unwrap_or_else(|_| "postgres".into());
        let db_password = std::env::var("DB_PASSWORD").unwrap_or_else(|_| "postgres".into());
        let db_name = std::env::var("DB_NAME").unwrap_or_else(|_| "any_player_sync".into());
        let db_sslmode = std::env::var("DB_SSLMODE").unwrap_or_else(|_| "prefer".into());

        let database_url = format!(
            "postgres://{db_user}:{db_password}@{db_host}:{db_port}/{db_name}?sslmode={db_sslmode}"
        );
        let database_url_safe =
            format!("postgres://{db_user}:****@{db_host}:{db_port}/{db_name}?sslmode={db_sslmode}");

        let cors_allowed_origins = std::env::var("CORS_ALLOWED_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        let max_body_size = std::env::var("MAX_BODY_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024 * 1024); // 1 MiB default

        let bind_address = bind_address
            .parse()
            .with_context(|| format!("invalid BIND_ADDRESS '{bind_address}'"))?;

        Ok(Self {
            bind_address,
            database_url,
            database_url_safe,
            cors_allowed_origins,
            max_body_size,
        })
    }
}
