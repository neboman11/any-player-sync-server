//! HTTP handlers for passage retrieval, play recording, and admin ingestion.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    errors::ApiError,
    handlers::{authenticate_with_headers, require_admin},
    match_keys::{artist_keys, base_title, match_key, song_keys},
    models::AuthenticatedUser,
    passages::{
        MAX_BODY_BYTES, NewDocument, Passages, Subject,
        embed::EmbedKind,
        jobs::{self, Enqueue, JobSummary, TrackRequest},
        rank::{self, BUDGET_TOKENS, Selection},
        store,
    },
    state::AppContext,
};

const MAX_PARAM_BYTES: usize = 1000;
const MAX_PLAYED_CHUNKS: usize = 16;

#[derive(Deserialize)]
pub struct PassageQuery {
    pub song: String,
    pub artist: String,
    #[serde(default)]
    pub album: Option<String>,
}

#[derive(Serialize)]
pub struct Passage {
    pub chunk_id: i64,
    pub text: String,
    pub source: String,
    pub source_url: String,
    pub title: String,
    pub subject: Subject,
    pub lang: String,
}

#[derive(Serialize)]
pub struct PassageResponse {
    pub status: &'static str,
    pub passages: Vec<Passage>,
}

#[derive(Deserialize)]
pub struct PlayedRequest {
    pub event_id: String,
    pub played_at: DateTime<Utc>,
    pub chunk_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub struct IngestRequest {
    pub tracks: Vec<TrackRequest>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Serialize)]
pub struct IngestResponse {
    pub queued: usize,
    pub already_queued: usize,
    pub recently_done: usize,
}

#[derive(Deserialize)]
pub struct NoteRequest {
    pub subject: Subject,
    #[serde(default)]
    pub song: Option<String>,
    pub artist: String,
    #[serde(default)]
    pub album: Option<String>,
    pub title: String,
    pub body: String,
}

#[derive(Serialize)]
pub struct Created {
    pub id: i64,
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub before_id: Option<i64>,
}

fn engine(state: &AppContext) -> Result<&Arc<Passages>, ApiError> {
    state
        .passages
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("DJ passages are not configured".into()))
}

fn valid(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_PARAM_BYTES
        && !value.chars().any(char::is_control)
}

fn db_error(context: &'static str) -> impl FnOnce(sqlx::Error) -> ApiError {
    move |err| {
        tracing::error!(%err, context);
        ApiError::internal(context.into())
    }
}

fn empty(status: &'static str) -> Json<PassageResponse> {
    Json(PassageResponse {
        status,
        passages: vec![],
    })
}

pub async fn lookup(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(q): Query<PassageQuery>,
) -> Result<Json<PassageResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    let engine = engine(&state)?;
    if !valid(&q.song) || !valid(&q.artist) || q.album.as_deref().is_some_and(|a| !valid(a)) {
        return Err(ApiError::bad_request(
            "invalid song, artist or album".into(),
        ));
    }
    let album_key = q.album.as_deref().map(|a| match_key(base_title(a)));
    let candidates = store::candidates(
        &state.pool,
        &song_keys(&q.song),
        &artist_keys(&q.artist),
        album_key.as_deref(),
        user.id,
    )
    .await
    .map_err(db_error("passage lookup failed"))?;
    // Album/artist candidates from an already-ingested artist don't cover a song of theirs the
    // store has never seen, so auto-queue whenever there's no song-subject candidate yet, not
    // only when there are no candidates at all. With candidates already in hand, this only
    // enqueues (respecting quota and idempotency) and falls through to rank and return them.
    if !candidates.iter().any(|c| c.subject == Subject::Song) {
        let under_quota = jobs::auto_count_today(&state.pool, user.id)
            .await
            .map_err(db_error("ingest quota failed"))?
            < jobs::AUTO_DAILY_LIMIT;
        if candidates.is_empty() && !under_quota {
            return Ok(empty("none"));
        }
        if under_quota {
            let track = TrackRequest {
                song: q.song.clone(),
                artist: q.artist.clone(),
                album: q.album.clone(),
            };
            let outcome = jobs::enqueue(&state.pool, &track, Some(user.id), true, false)
                .await
                .map_err(db_error("ingest enqueue failed"))?;
            if candidates.is_empty() {
                return Ok(match outcome {
                    Enqueue::Queued | Enqueue::AlreadyQueued => empty("queued"),
                    Enqueue::RecentlyDone => empty("none"),
                });
            }
        }
    }
    let embedder = engine.embedder.clone();
    let query = format!(
        "the story behind {} by {}: background, writing, recording, meaning, reception",
        base_title(&q.song),
        q.artist
    );
    let vector = tokio::task::spawn_blocking(move || embedder.embed(&[query], EmbedKind::Query))
        .await
        .map_err(|_| ApiError::internal("embedding failed".into()))?
        .map_err(|err| {
            tracing::error!(%err, "query embedding failed");
            ApiError::internal("embedding failed".into())
        })?
        .pop()
        .unwrap_or_default();
    match rank::select(
        &candidates,
        &vector,
        &match_key(base_title(&q.song)),
        Utc::now(),
        BUDGET_TOKENS,
    ) {
        Selection::Empty => Ok(empty("none")),
        Selection::AllExcluded => Ok(empty("exhausted")),
        Selection::Chosen(ids) => {
            let passages = ids
                .iter()
                .filter_map(|id| candidates.iter().find(|c| c.chunk_id == *id))
                .map(|c| Passage {
                    chunk_id: c.chunk_id,
                    text: c.text.clone(),
                    source: c.source.clone(),
                    source_url: c.source_url.clone(),
                    title: c.title.clone(),
                    subject: c.subject,
                    lang: c.lang.clone(),
                })
                .collect();
            Ok(Json(PassageResponse {
                status: "ok",
                passages,
            }))
        }
    }
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

pub async fn played(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(input): Json<PlayedRequest>,
) -> Result<StatusCode, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    engine(&state)?;
    if !valid_uuid(&input.event_id)
        || input.chunk_ids.is_empty()
        || input.chunk_ids.len() > MAX_PLAYED_CHUNKS
        || input.played_at > Utc::now() + chrono::Duration::minutes(5)
    {
        return Err(ApiError::bad_request("invalid played event".into()));
    }
    store::record_plays(
        &state.pool,
        user.id,
        &input.event_id,
        &input.chunk_ids,
        input.played_at,
    )
    .await
    .map_err(db_error("played event failed"))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin(
    state: &Arc<AppContext>,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    let user = authenticate_with_headers(state, headers).await?;
    require_admin(&user)?;
    Ok(user)
}

pub async fn admin_ingest(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(input): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, ApiError> {
    let user = admin(&state, &headers).await?;
    engine(&state)?;
    let mut out = IngestResponse {
        queued: 0,
        already_queued: 0,
        recently_done: 0,
    };
    for track in input
        .tracks
        .iter()
        .filter(|t| valid(&t.song) && valid(&t.artist))
    {
        match jobs::enqueue(&state.pool, track, Some(user.id), false, input.force)
            .await
            .map_err(db_error("ingest enqueue failed"))?
        {
            Enqueue::Queued => out.queued += 1,
            Enqueue::AlreadyQueued => out.already_queued += 1,
            Enqueue::RecentlyDone => out.recently_done += 1,
        }
    }
    Ok(Json(out))
}

pub async fn admin_list_jobs(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<JobSummary>>, ApiError> {
    admin(&state, &headers).await?;
    Ok(Json(
        jobs::list(&state.pool, q.limit.unwrap_or(100), q.before_id)
            .await
            .map_err(db_error("job list failed"))?,
    ))
}

pub async fn admin_misses(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
) -> Result<Json<Vec<TrackRequest>>, ApiError> {
    admin(&state, &headers).await?;
    Ok(Json(
        jobs::misses(&state.pool)
            .await
            .map_err(db_error("misses failed"))?,
    ))
}

pub async fn admin_create_document(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(input): Json<NoteRequest>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let user = admin(&state, &headers).await?;
    let engine = engine(&state)?;
    let subject_named = match input.subject {
        Subject::Song => input.song.as_deref().is_some_and(valid),
        Subject::Album => input.album.as_deref().is_some_and(valid),
        Subject::Artist => true,
    };
    if input.body.trim().is_empty()
        || input.body.len() > MAX_BODY_BYTES
        || !valid(&input.artist)
        || !valid(&input.title)
        || !subject_named
    {
        return Err(ApiError::bad_request("invalid document".into()));
    }
    let reference = format!("note-{}", rand::random::<u64>());
    let doc = NewDocument {
        source: "admin-note".into(),
        source_url: format!("admin-note:{reference}"),
        source_ref: reference,
        subject: input.subject,
        song: input.song,
        album: input.album,
        artist: input.artist,
        title: input.title,
        lang: "und".into(),
        body: input.body,
    };
    let id = store::upsert_document(&state.pool, &engine.embedder, &doc, Some(user.id))
        .await
        .map_err(|err| {
            tracing::error!(%err, "admin document failed");
            ApiError::internal("document store failed".into())
        })?;
    Ok((StatusCode::CREATED, Json(Created { id })))
}

pub async fn admin_delete_document(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    admin(&state, &headers).await?;
    if store::delete_document(&state.pool, id)
        .await
        .map_err(db_error("document delete failed"))?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("document not found".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passages::{
        embed::{Embed, HashEmbedder},
        testutil::*,
    };
    use crate::state::DjCatalog;

    fn state_with(pool: &sqlx::PgPool) -> Arc<AppContext> {
        let embedder: Arc<dyn Embed> = Arc::new(HashEmbedder);
        Arc::new(
            AppContext::new(pool.clone(), DjCatalog::default(), DjCatalog::default())
                .with_passages(Some(Arc::new(Passages { embedder }))),
        )
    }

    fn q(song: &str, artist: &str) -> Query<PassageQuery> {
        Query(PassageQuery {
            song: song.into(),
            artist: artist.into(),
            album: None,
        })
    }

    #[tokio::test]
    async fn lookup_returns_passages_then_excludes_them_after_play() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let state = state_with(&pool);
        let tag = rand::random::<u64>();
        let song = format!("Api Song {tag}");
        let embedder = state.passages.as_ref().unwrap().embedder.clone();
        store::upsert_document(
            &pool,
            &embedder,
            &NewDocument {
                source: "songfacts".into(),
                source_ref: format!("/facts/api/{tag}"),
                source_url: "https://www.songfacts.com/x".into(),
                subject: Subject::Song,
                song: Some(song.clone()),
                album: None,
                artist: "Api Artist".into(),
                title: song.clone(),
                lang: "en".into(),
                body: format!("{song} was recorded in one take."),
            },
            None,
        )
        .await
        .unwrap();
        let (_, token) = user_with_token(&pool, false).await;
        let first = lookup(
            State(state.clone()),
            bearer(&token),
            q(&format!("{song} - 2020 Remaster"), "Api Artist, Guest"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first.status, "ok");
        let ids: Vec<i64> = first.passages.iter().map(|p| p.chunk_id).collect();
        assert!(!ids.is_empty());
        played(
            State(state.clone()),
            bearer(&token),
            Json(PlayedRequest {
                event_id: "12345678-1234-1234-1234-123456789abc".into(),
                played_at: Utc::now(),
                chunk_ids: ids,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            lookup(State(state.clone()), bearer(&token), q(&song, "Api Artist"))
                .await
                .unwrap()
                .0
                .status,
            "exhausted"
        );
    }

    #[tokio::test]
    async fn lookup_auto_queues_a_new_song_by_a_known_artist_and_still_returns_its_passages() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let state = state_with(&pool);
        let tag = rand::random::<u64>();
        let artist = format!("Known Artist {tag}");
        let embedder = state.passages.as_ref().unwrap().embedder.clone();
        // Only an artist-subject document exists; there is no song-subject document for this
        // artist's (brand new) song, so it must still count as "nothing found for this song".
        store::upsert_document(
            &pool,
            &embedder,
            &NewDocument {
                source: "wikipedia-en".into(),
                source_ref: format!("artist/{tag}"),
                source_url: "https://en.wikipedia.org/wiki/x".into(),
                subject: Subject::Artist,
                song: None,
                album: None,
                artist: artist.clone(),
                title: artist.clone(),
                lang: "en".into(),
                body: "A long biography with plenty of background material.".into(),
            },
            None,
        )
        .await
        .unwrap();
        let (_, token) = user_with_token(&pool, false).await;
        let song = format!("Brand New Song {tag}");
        let result = lookup(State(state.clone()), bearer(&token), q(&song, &artist))
            .await
            .unwrap()
            .0;
        assert_eq!(result.status, "ok");
        assert!(!result.passages.is_empty());
        assert!(result.passages.iter().all(|p| p.subject == Subject::Artist));

        let (sk, ak, alk) = jobs::job_keys(&TrackRequest {
            song: song.clone(),
            artist: artist.clone(),
            album: None,
        });
        let job_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM dj_ingest_jobs WHERE song_key=$1 AND artist_key=$2 AND album_key=$3)",
        )
        .bind(&sk)
        .bind(&ak)
        .bind(&alk)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            job_exists,
            "a new song by an already-ingested artist must still be auto-queued"
        );
    }

    #[tokio::test]
    async fn unknown_track_is_queued_then_none_after_an_empty_job() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let state = state_with(&pool);
        let (_, token) = user_with_token(&pool, false).await;
        let song = format!("Nothing Song {}", rand::random::<u64>());
        assert_eq!(
            lookup(State(state.clone()), bearer(&token), q(&song, "Nobody"))
                .await
                .unwrap()
                .0
                .status,
            "queued"
        );
        sqlx::query("UPDATE dj_ingest_jobs SET status='done' WHERE song=$1")
            .bind(&song)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            lookup(State(state.clone()), bearer(&token), q(&song, "Nobody"))
                .await
                .unwrap()
                .0
                .status,
            "none"
        );
    }

    #[tokio::test]
    async fn admin_routes_reject_normal_tokens() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let state = state_with(&pool);
        let (_, user) = user_with_token(&pool, false).await;
        let (_, admin) = user_with_token(&pool, true).await;
        let ingest = || {
            Json(IngestRequest {
                tracks: vec![],
                force: false,
            })
        };
        assert!(
            admin_ingest(State(state.clone()), bearer(&user), ingest())
                .await
                .is_err()
        );
        assert!(
            admin_ingest(State(state.clone()), bearer(&admin), ingest())
                .await
                .is_ok()
        );
        assert!(
            admin_misses(State(state.clone()), bearer(&user))
                .await
                .is_err()
        );
        assert!(
            admin_list_jobs(
                State(state.clone()),
                bearer(&user),
                Query(ListQuery {
                    limit: None,
                    before_id: None
                })
            )
            .await
            .is_err()
        );
        let note = || {
            Json(NoteRequest {
                subject: Subject::Song,
                song: Some("Note Song".into()),
                artist: "Note Artist".into(),
                album: None,
                title: "Liner notes".into(),
                body: "Recorded live to tape.".into(),
            })
        };
        assert!(
            admin_create_document(State(state.clone()), bearer(&user), note())
                .await
                .is_err()
        );
        let (status, Json(created)) =
            admin_create_document(State(state.clone()), bearer(&admin), note())
                .await
                .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert!(
            admin_delete_document(State(state.clone()), bearer(&user), Path(created.id))
                .await
                .is_err()
        );
        assert_eq!(
            admin_delete_document(State(state.clone()), bearer(&admin), Path(created.id))
                .await
                .unwrap(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn lookup_needs_an_embedder_and_bounded_input() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let (_, token) = user_with_token(&pool, false).await;
        let bare = Arc::new(AppContext::new(
            pool.clone(),
            DjCatalog::default(),
            DjCatalog::default(),
        ));
        assert!(
            lookup(State(bare), bearer(&token), q("a", "b"))
                .await
                .is_err()
        );
        assert!(
            lookup(
                State(state_with(&pool)),
                bearer(&token),
                q(&"x".repeat(1001), "b")
            )
            .await
            .is_err()
        );
    }
}
