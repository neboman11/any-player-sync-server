use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;

use crate::{
    db::{load_snapshot, replace_snapshot, update_namespace},
    errors::ApiError,
    models::{
        HealthResponse, Namespace, NamespacePayload, SnapshotPayload, SnapshotQuery,
        UpdateResponse, namespace_data,
    },
    state::AppContext,
    ws::handle_ws_connection,
};

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "any-player-sync-server",
        timestamp: Utc::now(),
    })
}

pub async fn get_snapshot(
    State(state): State<Arc<AppContext>>,
    Query(query): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    let snapshot = load_snapshot(&state.pool).await?;

    if let Some(since_version) = query.since_version {
        if snapshot.version <= since_version {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
    }

    Ok(Json(snapshot).into_response())
}

pub async fn put_snapshot(
    State(state): State<Arc<AppContext>>,
    Json(payload): Json<SnapshotPayload>,
) -> Result<Json<crate::models::Snapshot>, ApiError> {
    let (snapshot, update_event) = replace_snapshot(&state.pool, payload).await?;
    let _ = state.updates_tx.send(update_event);
    Ok(Json(snapshot))
}

pub async fn get_namespace(
    State(state): State<Arc<AppContext>>,
    Path(namespace): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let namespace = Namespace::parse(&namespace)?;
    if matches!(namespace, Namespace::Snapshot) {
        return Err(ApiError::bad_request(
            "snapshot is only available via /v1/snapshot".into(),
        ));
    }

    let snapshot = load_snapshot(&state.pool).await?;
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
    Path(namespace): Path<String>,
    Json(payload): Json<NamespacePayload>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let namespace = Namespace::parse(&namespace)?;
    if matches!(namespace, Namespace::Snapshot) {
        return Err(ApiError::bad_request(
            "snapshot is only available via /v1/snapshot".into(),
        ));
    }

    let (snapshot, update_event) = update_namespace(&state.pool, namespace, payload).await?;
    let _ = state.updates_tx.send(update_event);

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
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state.updates_tx.subscribe()))
}
