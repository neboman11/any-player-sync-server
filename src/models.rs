use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::errors::ApiError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub app_state: Value,
    pub playlists: Value,
    pub provider_configuration: Value,
    pub settings: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateResponse {
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub namespace: Namespace,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespacePayload {
    pub expected_version: Option<i64>,
    pub client_id: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub expected_version: Option<i64>,
    pub client_id: Option<String>,
    pub app_state: Value,
    pub playlists: Value,
    pub provider_configuration: Value,
    pub settings: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub service: &'static str,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateEvent {
    pub event_type: String,
    pub namespace: Namespace,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub source_client_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserScopedUpdateEvent {
    pub user_id: i64,
    pub event: UpdateEvent,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthenticatedUser {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    AppState,
    Playlists,
    ProviderConfiguration,
    Settings,
    Snapshot,
}

impl Namespace {
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "app-state" => Ok(Self::AppState),
            "playlists" => Ok(Self::Playlists),
            "provider-configuration" => Ok(Self::ProviderConfiguration),
            "settings" => Ok(Self::Settings),
            _ => Err(ApiError::bad_request(format!(
                "unsupported namespace '{value}'. Supported: app-state, playlists, provider-configuration, settings"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SnapshotQuery {
    pub since_version: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub label: String,
    pub token_prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserSummary {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub tokens: Vec<TokenInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserCreatedResponse {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenCreatedResponse {
    pub id: i64,
    pub user_id: i64,
    pub label: String,
    pub token_prefix: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    #[serde(default)]
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetUserDisabledRequest {
    pub disabled: bool,
}

#[derive(Debug, Serialize)]
pub struct OperationResponse {
    pub ok: bool,
}

pub fn namespace_data(snapshot: &Snapshot, namespace: Namespace) -> Value {
    match namespace {
        Namespace::AppState => snapshot.app_state.clone(),
        Namespace::Playlists => snapshot.playlists.clone(),
        Namespace::ProviderConfiguration => snapshot.provider_configuration.clone(),
        Namespace::Settings => snapshot.settings.clone(),
        Namespace::Snapshot => json!({
            "app_state": snapshot.app_state,
            "playlists": snapshot.playlists,
            "provider_configuration": snapshot.provider_configuration,
            "settings": snapshot.settings,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{Namespace, Snapshot, namespace_data};
    use chrono::Utc;
    use serde_json::json;

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            version: 7,
            updated_at: Utc::now(),
            app_state: json!({ "state": "playing" }),
            playlists: json!([{ "id": "p1" }]),
            provider_configuration: json!({ "jellyfin": { "base_url": "http://localhost" } }),
            settings: json!({ "audio_normalization_enabled": true }),
        }
    }

    #[test]
    fn parses_supported_namespaces() {
        assert!(matches!(
            Namespace::parse("app-state"),
            Ok(Namespace::AppState)
        ));
        assert!(matches!(
            Namespace::parse("playlists"),
            Ok(Namespace::Playlists)
        ));
        assert!(matches!(
            Namespace::parse("provider-configuration"),
            Ok(Namespace::ProviderConfiguration)
        ));
        assert!(matches!(
            Namespace::parse("settings"),
            Ok(Namespace::Settings)
        ));
    }

    #[test]
    fn rejects_unsupported_namespace() {
        let error = Namespace::parse("app_state").expect_err("should fail");
        let message = format!("{error:?}");
        assert!(message.contains("unsupported namespace"));
    }

    #[test]
    fn builds_snapshot_namespace_payload() {
        let snapshot = sample_snapshot();
        let value = namespace_data(&snapshot, Namespace::Snapshot);

        assert_eq!(value["app_state"]["state"], "playing");
        assert_eq!(value["playlists"][0]["id"], "p1");
        assert_eq!(
            value["provider_configuration"]["jellyfin"]["base_url"],
            "http://localhost"
        );
        assert_eq!(value["settings"]["audio_normalization_enabled"], true);
    }
}
