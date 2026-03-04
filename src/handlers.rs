use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use chrono::Utc;

use crate::{
    db::{
        authenticate_token, create_token, create_user, list_users, load_snapshot, replace_snapshot,
        revoke_token, set_user_disabled, update_namespace,
    },
    errors::ApiError,
    models::{
        AuthenticatedUser, CreateTokenRequest, CreateUserRequest, HealthResponse, Namespace,
        NamespacePayload, OperationResponse, SetUserDisabledRequest, SnapshotPayload,
        SnapshotQuery, TokenCreatedResponse, UpdateResponse, WsQuery,
        namespace_data,
    },
    state::AppContext,
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

async fn authenticate_with_headers(
    state: &Arc<AppContext>,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    let token = bearer_token_from_headers(headers)
        .ok_or_else(|| ApiError::unauthorized("missing Authorization: Bearer token".to_string()))?;
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

fn require_admin(user: &AuthenticatedUser) -> Result<(), ApiError> {
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
    Ok(ws.on_upgrade(move |socket| {
        handle_ws_connection(socket, updates_rx)
    }))
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

    let created = create_user(&state.pool, &payload.name, payload.is_admin).await?;
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
