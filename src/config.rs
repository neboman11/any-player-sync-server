use std::net::SocketAddr;

use anyhow::Context;

pub struct AppConfig {
    pub bind_address: SocketAddr,
    pub database_url: String,
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

        let bind_address = bind_address
            .parse()
            .with_context(|| format!("invalid BIND_ADDRESS '{bind_address}'"))?;

        Ok(Self {
            bind_address,
            database_url,
        })
    }
}
