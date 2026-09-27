use std::sync::Arc;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, Request, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use chrono::Utc;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    db::{
        authenticate_token, create_token, create_user, list_users, load_snapshot, replace_snapshot,
        revoke_token, set_user_disabled, update_namespace,
    },
    errors::ApiError,
    models::{
        AuthenticatedUser, CreateTokenRequest, CreateUserRequest, DjModelCatalogResponse,
        DjModelDescriptor, DjModelInfoResponse, DjVoiceCatalogResponse, HealthResponse, Namespace,
        NamespacePayload, OperationResponse, SetUserDisabledRequest, SnapshotPayload,
        SnapshotQuery, TokenCreatedResponse, UpdateResponse, WsQuery, namespace_data,
    },
    state::{AppContext, DjCatalog, DjCatalogEntry},
    ws::handle_ws_connection,
};

fn bearer_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?;
    let as_str = value.to_str().ok()?.trim();
    let token = as_str
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token.trim())?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub(crate) async fn authenticate_with_headers(
    state: &Arc<AppContext>,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    let token = bearer_token_from_headers(headers)
        .ok_or_else(|| ApiError::unauthorized("missing Authorization: Bearer token".to_string()))?;
    // A test-only credential exercises the public router without a PostgreSQL dependency.
    #[cfg(test)]
    if token == "catalog-test-token" {
        return Ok(AuthenticatedUser {
            id: 1,
            name: "catalog-test".to_string(),
            is_admin: false,
        });
    }
    authenticate_token(&state.pool, &token).await
}

async fn authenticate_with_headers_or_query_token(
    state: &Arc<AppContext>,
    headers: &HeaderMap,
    query_token: Option<String>,
) -> Result<AuthenticatedUser, ApiError> {
    if let Some(token) = bearer_token_from_headers(headers) {
        return authenticate_token(&state.pool, &token).await;
    }

    if let Some(token) = query_token {
        return authenticate_token(&state.pool, &token).await;
    }

    Err(ApiError::unauthorized(
        "missing bearer token (Authorization header or token query parameter)".to_string(),
    ))
}

pub(crate) fn require_admin(user: &AuthenticatedUser) -> Result<(), ApiError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "admin privileges are required".to_string(),
        ))
    }
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "any-player-sync-server",
        timestamp: Utc::now(),
    })
}

pub async fn get_snapshot(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(query): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    let snapshot = load_snapshot(&state.pool, user.id).await?;

    if let Some(since_version) = query.since_version
        && snapshot.version <= since_version
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }

    Ok(Json(snapshot).into_response())
}

pub async fn put_snapshot(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(payload): Json<SnapshotPayload>,
) -> Result<Json<crate::models::Snapshot>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    let (snapshot, event) = replace_snapshot(&state.pool, user.id, payload).await?;
    state.send_user_event(user.id, event).await;
    Ok(Json(snapshot))
}

pub async fn dj_model_info(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<DjModelInfoResponse>, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_entry_info_response(
        state.dj_model_catalog.default_model(),
        DJ_MODEL_NOT_CONFIGURED,
    )
}

pub async fn dj_model_download(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_download_response(
        &state.dj_model_catalog,
        state
            .dj_model_catalog
            .default_id
            .as_deref()
            .unwrap_or_default(),
        DJ_MODEL_NOT_CONFIGURED,
        request,
    )
    .await
}

pub async fn dj_models(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<DjModelCatalogResponse>, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    let catalog = &state.dj_model_catalog;
    Ok(Json(DjModelCatalogResponse {
        default_id: catalog.default_id.clone(),
        models: catalog
            .entries
            .iter()
            .map(|entry| DjModelDescriptor {
                descriptor: entry.descriptor.clone(),
                format: entry.format(),
            })
            .collect(),
    }))
}

pub async fn dj_model_download_by_id(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(model_id): Path<String>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_download_response(
        &state.dj_model_catalog,
        &model_id,
        DJ_MODEL_NOT_CONFIGURED,
        request,
    )
    .await
}

const DJ_MODEL_NOT_CONFIGURED: &str = "AI DJ model is not configured on this server";
const DJ_VOICE_NOT_CONFIGURED: &str = "AI DJ voice model is not configured on this server";

/// Metadata for the operator-configured AI DJ neural voice bundle (a zip containing
/// the Piper/VITS `.onnx` model + `tokens.txt`; the shared `espeak-ng-data` phoneme
/// tables ship inside the app itself since they're identical across voices).
pub async fn dj_voice_model_info(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<DjModelInfoResponse>, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_entry_info_response(
        state.dj_voice_catalog.default_model(),
        DJ_VOICE_NOT_CONFIGURED,
    )
}

pub async fn dj_voice_model_download(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_download_response(
        &state.dj_voice_catalog,
        state
            .dj_voice_catalog
            .default_id
            .as_deref()
            .unwrap_or_default(),
        DJ_VOICE_NOT_CONFIGURED,
        request,
    )
    .await
}

/// Metadata (size + sha256) for a catalog's default entry, served by the legacy
/// single-file `/info` endpoints. 404s if the operator hasn't configured one.
fn dj_catalog_entry_info_response(
    entry: Option<&DjCatalogEntry>,
    not_configured_message: &str,
) -> Result<Json<DjModelInfoResponse>, ApiError> {
    let entry = entry.ok_or_else(|| ApiError::not_found(not_configured_message.to_string()))?;
    Ok(Json(DjModelInfoResponse {
        version: entry.descriptor.version.clone(),
        size_bytes: entry.descriptor.size_bytes,
        sha256: entry.descriptor.sha256.clone(),
    }))
}

/// Streams a catalog entry's file. Delegates to `tower_http`'s `ServeFile`, which
/// handles `Range` requests so a client can resume an interrupted large download.
async fn dj_catalog_download_response(
    catalog: &DjCatalog,
    id: &str,
    not_configured_message: &str,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let entry = catalog
        .find(id)
        .ok_or_else(|| ApiError::not_found(not_configured_message.to_string()))?;
    match ServeFile::new(entry.path()).oneshot(request).await {
        Ok(response) => Ok(response.into_response()),
        Err(err) => match err {},
    }
}

pub async fn dj_voice_models(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<DjVoiceCatalogResponse>, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    let catalog = &state.dj_voice_catalog;
    Ok(Json(DjVoiceCatalogResponse {
        default_id: catalog.default_id.clone(),
        voices: catalog
            .entries
            .iter()
            .map(|entry| entry.descriptor.clone())
            .collect(),
    }))
}

pub async fn dj_voice_model_download_by_id(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(voice_id): Path<String>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    dj_catalog_download_response(
        &state.dj_voice_catalog,
        &voice_id,
        DJ_VOICE_NOT_CONFIGURED,
        request,
    )
    .await
}

pub async fn get_namespace(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    let namespace = Namespace::parse(&namespace)?;
    if matches!(namespace, Namespace::Snapshot) {
        return Err(ApiError::bad_request(
            "snapshot is only available via /v1/snapshot".into(),
        ));
    }

    let snapshot = load_snapshot(&state.pool, user.id).await?;
    let data = namespace_data(&snapshot, namespace);

    Ok(Json(UpdateResponse {
        version: snapshot.version,
        updated_at: snapshot.updated_at,
        namespace,
        data,
    }))
}

pub async fn put_namespace(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Json(payload): Json<NamespacePayload>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    let namespace = Namespace::parse(&namespace)?;
    if matches!(namespace, Namespace::Snapshot) {
        return Err(ApiError::bad_request(
            "snapshot is only available via /v1/snapshot".into(),
        ));
    }

    let (snapshot, event) = update_namespace(&state.pool, user.id, namespace, payload).await?;
    state.send_user_event(user.id, event).await;

    Ok(Json(UpdateResponse {
        version: snapshot.version,
        updated_at: snapshot.updated_at,
        namespace,
        data: namespace_data(&snapshot, namespace),
    }))
}

pub async fn ws_updates(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = authenticate_with_headers_or_query_token(&state, &headers, query.token).await?;
    let updates_rx = state.subscribe_user(user.id).await;
    Ok(ws.on_upgrade(move |socket| handle_ws_connection(socket, updates_rx)))
}

pub async fn admin_index() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

pub async fn admin_login() -> Html<&'static str> {
    Html(ADMIN_LOGIN_HTML)
}

pub async fn admin_list_users(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::models::UserSummary>>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    require_admin(&user)?;

    let users = list_users(&state.pool).await?;
    Ok(Json(users))
}

pub async fn admin_create_user(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(payload): Json<CreateUserRequest>,
) -> Result<Json<crate::models::UserCreatedResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    require_admin(&user)?;

    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "username must not be empty".to_string(),
        ));
    }
    if name.len() > 255 {
        return Err(ApiError::bad_request(
            "username must not exceed 255 characters".to_string(),
        ));
    }

    let created = create_user(&state.pool, &name, payload.is_admin).await?;
    Ok(Json(created))
}

pub async fn admin_create_token(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(payload): Json<CreateTokenRequest>,
) -> Result<Json<TokenCreatedResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    require_admin(&user)?;

    if let Some(ref label) = payload.label
        && label.len() > 512
    {
        return Err(ApiError::bad_request(
            "token label must not exceed 512 characters".to_string(),
        ));
    }

    let (id, label, token_prefix, token, created_at) =
        create_token(&state.pool, user_id, payload.label).await?;

    Ok(Json(TokenCreatedResponse {
        id,
        user_id,
        label,
        token_prefix,
        token,
        created_at,
    }))
}

pub async fn admin_revoke_token(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(token_id): Path<i64>,
) -> Result<Json<OperationResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    require_admin(&user)?;

    revoke_token(&state.pool, token_id).await?;
    Ok(Json(OperationResponse { ok: true }))
}

pub async fn admin_set_user_disabled(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(payload): Json<SetUserDisabledRequest>,
) -> Result<Json<OperationResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    require_admin(&user)?;

    set_user_disabled(&state.pool, user_id, payload.disabled).await?;
    Ok(Json(OperationResponse { ok: true }))
}

const ADMIN_HTML: &str = include_str!("../static/admin/index.html");
const ADMIN_LOGIN_HTML: &str = include_str!("../static/admin/login.html");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DjCatalogDescriptor;
    use axum::body::to_bytes;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn catalog_entry(id: &str, path: std::path::PathBuf) -> DjCatalogEntry {
        DjCatalogEntry::new(
            DjCatalogDescriptor {
                id: id.to_string(),
                name: "Deep".to_string(),
                version: "voice-v1".to_string(),
                size_bytes: 20,
                sha256: "590cde0323c8ece1ed91c67448110d2247fe64708ce2310d1e988df0d8e3c0bb"
                    .to_string(),
            },
            path,
        )
    }

    fn catalog_of(entry: DjCatalogEntry) -> DjCatalog {
        DjCatalog {
            default_id: Some(entry.descriptor.id.clone()),
            entries: vec![entry],
        }
    }

    fn fixture_path(extension: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "any-player-dj-fixture-{}.{extension}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ));
        fs::write(&path, b"voice-model-fixture\n").expect("write model fixture");
        path
    }

    fn lazy_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:password@localhost/database")
            .expect("create lazy pool")
    }

    #[test]
    fn voice_model_info_is_not_found_when_unconfigured() {
        let error = match dj_catalog_entry_info_response(None, "voice model is not configured") {
            Err(error) => error,
            Ok(_) => panic!("unconfigured voice model must be absent"),
        };

        assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn voice_model_info_requires_authorization() {
        let state = Arc::new(AppContext::new(
            lazy_pool(),
            DjCatalog::default(),
            DjCatalog::default(),
        ));

        let error = match dj_voice_model_info(State(state), HeaderMap::new()).await {
            Err(error) => error,
            Ok(_) => panic!("voice model info must require authorization"),
        };

        assert_eq!(error.into_response().status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn voice_model_info_exposes_download_contract() {
        let response = dj_catalog_entry_info_response(
            Some(&catalog_entry(
                "deep",
                std::path::PathBuf::from("/models/voice.zip"),
            )),
            "voice model is not configured",
        )
        .expect("configured voice model returns info")
        .0;

        assert_eq!(
            serde_json::to_value(response).expect("serialize model info"),
            serde_json::json!({
                "version": "voice-v1",
                "size_bytes": 20,
                "sha256": "590cde0323c8ece1ed91c67448110d2247fe64708ce2310d1e988df0d8e3c0bb"
            })
        );
    }

    #[tokio::test]
    async fn voice_model_download_honors_byte_ranges() {
        let path = fixture_path("zip");
        let catalog = catalog_of(catalog_entry("deep", path.clone()));
        let request = Request::builder()
            .header(header::RANGE, "bytes=6-10")
            .body(Body::empty())
            .expect("build range request");

        let response = dj_catalog_download_response(
            &catalog,
            "deep",
            "voice model is not configured",
            request,
        )
        .await
        .expect("serve voice model range");

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read response body")
                .as_ref(),
            b"model"
        );
        fs::remove_file(path).expect("remove voice model fixture");
    }

    #[tokio::test]
    async fn catalog_route_requires_auth_and_downloads_only_known_id() {
        let fixture_path = fixture_path("zip");
        let catalog = catalog_of(catalog_entry("deep", fixture_path.clone()));
        let app = crate::app::build_router(
            Arc::new(AppContext::new(lazy_pool(), DjCatalog::default(), catalog)),
            Vec::new(),
            1024,
        );

        let error = dj_voice_models(
            State(Arc::new(AppContext::new(
                lazy_pool(),
                DjCatalog::default(),
                DjCatalog::default(),
            ))),
            HeaderMap::new(),
        )
        .await
        .expect_err("catalog requires authorization");
        assert_eq!(error.into_response().status(), StatusCode::UNAUTHORIZED);

        let catalog_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/dj-voice-models")
                    .header(header::AUTHORIZATION, "Bearer catalog-test-token")
                    .body(Body::empty())
                    .expect("build catalog request"),
            )
            .await
            .expect("catalog response");
        assert_eq!(catalog_response.status(), StatusCode::OK);
        let catalog_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(catalog_response.into_body(), usize::MAX)
                .await
                .expect("read catalog response"),
        )
        .expect("parse catalog JSON");
        assert_eq!(catalog_json["default_id"], "deep");
        assert!(catalog_json["voices"][0].get("path").is_none());

        let known_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/dj-voice-models/deep/download")
                    .header(header::AUTHORIZATION, "Bearer catalog-test-token")
                    .body(Body::empty())
                    .expect("build known download request"),
            )
            .await
            .expect("known download response");
        assert_eq!(known_response.status(), StatusCode::OK);

        for voice_id in ["missing", "-bad"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/dj-voice-models/{voice_id}/download"))
                        .header(header::AUTHORIZATION, "Bearer catalog-test-token")
                        .body(Body::empty())
                        .expect("build unknown download request"),
                )
                .await
                .expect("unknown download response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let error_json: serde_json::Value = serde_json::from_slice(
                &to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("read unknown download response"),
            )
            .expect("parse unknown download JSON");
            assert_eq!(error_json["error"]["code"], "not_found");
            assert_eq!(
                error_json["error"]["message"],
                "AI DJ voice model is not configured on this server"
            );
        }

        fs::remove_file(fixture_path).expect("remove voice model fixture");
    }

    #[test]
    fn legacy_voice_model_compatibility() {
        let catalog = catalog_of(catalog_entry(
            "default",
            std::path::PathBuf::from("/models/voice.zip"),
        ));
        let response = dj_catalog_entry_info_response(
            catalog.default_model(),
            "AI DJ voice model is not configured on this server",
        )
        .expect("legacy endpoint resolves catalog default");
        assert_eq!(response.0.version, "voice-v1");
    }

    #[tokio::test]
    async fn model_catalog_reports_format_and_downloads_by_id() {
        let path = fixture_path("litertlm");
        let app = crate::app::build_router(
            Arc::new(AppContext::new(
                lazy_pool(),
                catalog_of(catalog_entry("gemma-4-e2b", path.clone())),
                DjCatalog::default(),
            )),
            Vec::new(),
            1024,
        );
        let request = |uri: &str| {
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer catalog-test-token")
                .body(Body::empty())
                .expect("build request")
        };

        let response = app
            .clone()
            .oneshot(request("/v1/dj-models"))
            .await
            .expect("model catalog response");
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read model catalog"),
        )
        .expect("parse model catalog");
        assert_eq!(json["default_id"], "gemma-4-e2b");
        assert_eq!(json["models"][0]["id"], "gemma-4-e2b");
        assert_eq!(json["models"][0]["format"], "litertlm");
        assert!(json["models"][0].get("path").is_none());

        for (uri, status) in [
            ("/v1/dj-models/gemma-4-e2b/download", StatusCode::OK),
            ("/v1/dj-model/download", StatusCode::OK),
            ("/v1/dj-models/missing/download", StatusCode::NOT_FOUND),
        ] {
            let response = app.clone().oneshot(request(uri)).await.expect("response");
            assert_eq!(response.status(), status, "{uri}");
        }
        fs::remove_file(path).expect("remove model fixture");
    }
}
