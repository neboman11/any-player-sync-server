//! Ingest job queue. One row per (song, artist, album) key tuple; the worker claims rows with
//! SKIP LOCKED so each is processed once even with concurrent claimers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgPool;

use crate::match_keys::{base_title, match_key};

pub const AUTO_DAILY_LIMIT: i64 = 200;
pub const MAX_ATTEMPTS: i32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackRequest {
    pub song: String,
    pub artist: String,
    #[serde(default)]
    pub album: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub track: TrackRequest,
    pub attempts: i32,
    pub results: Map<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct JobSummary {
    pub id: i64,
    pub song: String,
    pub artist: String,
    pub album: Option<String>,
    pub status: String,
    pub attempts: i32,
    pub results: Value,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, PartialEq)]
pub enum Enqueue {
    Queued,
    AlreadyQueued,
    RecentlyDone,
}

pub fn job_keys(track: &TrackRequest) -> (String, String, String) {
    (
        match_key(base_title(&track.song)),
        match_key(&track.artist),
        track
            .album
            .as_deref()
            .map(|a| match_key(base_title(a)))
            .unwrap_or_default(),
    )
}

pub async fn enqueue(
    pool: &PgPool,
    track: &TrackRequest,
    requested_by: Option<i64>,
    auto: bool,
    force: bool,
) -> Result<Enqueue, sqlx::Error> {
    let (sk, ak, alk) = job_keys(track);
    let inserted: Option<i64> = sqlx::query_scalar(
        r#"INSERT INTO dj_ingest_jobs (song, artist, album, song_key, artist_key, album_key, requested_by, auto)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
           ON CONFLICT (song_key, artist_key, album_key) DO NOTHING RETURNING id"#,
    )
    .bind(&track.song).bind(&track.artist).bind(&track.album)
    .bind(&sk).bind(&ak).bind(&alk).bind(requested_by).bind(auto)
    .fetch_optional(pool)
    .await?;
    if inserted.is_some() {
        return Ok(Enqueue::Queued);
    }
    // A finished job is requeued when forced (admin) or after the 30-day no-match window.
    let requeued: Option<i64> = sqlx::query_scalar(
        r#"UPDATE dj_ingest_jobs SET status='queued', attempts=0, next_attempt_at=NOW(),
               results='{}'::jsonb, last_error=NULL, updated_at=NOW()
           WHERE song_key=$1 AND artist_key=$2 AND album_key=$3 AND status IN ('done','failed')
             AND ($4 OR updated_at < NOW() - INTERVAL '30 days')
           RETURNING id"#,
    )
    .bind(&sk)
    .bind(&ak)
    .bind(&alk)
    .bind(force)
    .fetch_optional(pool)
    .await?;
    if requeued.is_some() {
        return Ok(Enqueue::Queued);
    }
    let status: String = sqlx::query_scalar(
        "SELECT status FROM dj_ingest_jobs WHERE song_key=$1 AND artist_key=$2 AND album_key=$3",
    )
    .bind(&sk)
    .bind(&ak)
    .bind(&alk)
    .fetch_one(pool)
    .await?;
    Ok(if matches!(status.as_str(), "done" | "failed") {
        Enqueue::RecentlyDone
    } else {
        Enqueue::AlreadyQueued
    })
}

pub async fn auto_count_today(pool: &PgPool, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM dj_ingest_jobs WHERE auto AND requested_by=$1 AND created_at > NOW() - INTERVAL '1 day'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// A `running` job whose lease is older than this is assumed abandoned (crash, failed `finish`,
/// or a replica that was killed mid-job) and can be claimed again. Far longer than one job takes
/// even with Songfacts spacing and 8 s per-request timeouts.
const STALE_LEASE_MINUTES: i64 = 30;

pub async fn claim(pool: &PgPool) -> Result<Option<Job>, sqlx::Error> {
    let row: Option<(i64, String, String, Option<String>, i32, Value)> = sqlx::query_as(
        r#"UPDATE dj_ingest_jobs SET status='running', attempts=attempts+1, updated_at=NOW()
           WHERE id = (SELECT id FROM dj_ingest_jobs
                       WHERE (status='queued' AND next_attempt_at <= NOW())
                          OR (status='running' AND updated_at < NOW() - make_interval(mins => $1))
                       ORDER BY next_attempt_at, id FOR UPDATE SKIP LOCKED LIMIT 1)
           RETURNING id, song, artist, album, attempts, results"#,
    )
    .bind(STALE_LEASE_MINUTES as i32)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, song, artist, album, attempts, results)| Job {
        id,
        track: TrackRequest {
            song,
            artist,
            album,
        },
        attempts,
        results: results.as_object().cloned().unwrap_or_default(),
    }))
}

fn state(v: &Value) -> &str {
    v.get("state").and_then(Value::as_str).unwrap_or("")
}

pub async fn finish(
    pool: &PgPool,
    job: &Job,
    mut results: Map<String, Value>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    for v in results.values_mut() {
        if let Some(o) = v.as_object_mut() {
            o.entry("at").or_insert(json!(now));
        }
    }
    let errors: Vec<String> = results
        .iter()
        .filter(|(_, v)| state(v) == "error")
        .map(|(k, v)| {
            format!(
                "{k}: {}",
                v.get("message").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    let all_error = !results.is_empty() && results.values().all(|v| state(v) == "error");
    let (status, delay_minutes) = if !errors.is_empty() && job.attempts < MAX_ATTEMPTS {
        ("queued", 2_i32.pow(job.attempts.clamp(0, 10) as u32))
    } else if all_error {
        ("failed", 0)
    } else {
        ("done", 0)
    };
    sqlx::query(
        r#"UPDATE dj_ingest_jobs SET status=$2, results=$3, last_error=$4, updated_at=NOW(),
               next_attempt_at = NOW() + make_interval(mins => $5)
           WHERE id=$1"#,
    )
    .bind(job.id)
    .bind(status)
    .bind(Value::Object(results))
    .bind((!errors.is_empty()).then(|| errors.join("; ")))
    .bind(delay_minutes)
    .execute(pool)
    .await?;
    Ok(())
}

type JobRow = (
    i64,
    String,
    String,
    Option<String>,
    String,
    i32,
    Value,
    Option<String>,
    DateTime<Utc>,
);

pub async fn list(
    pool: &PgPool,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<JobSummary>, sqlx::Error> {
    let rows: Vec<JobRow> = sqlx::query_as(
        r#"SELECT id, song, artist, album, status, attempts, results, last_error, updated_at
           FROM dj_ingest_jobs WHERE ($2::bigint IS NULL OR id < $2) ORDER BY id DESC LIMIT $1"#,
    )
    .bind(limit.clamp(1, 500))
    .bind(before_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| JobSummary {
            id: r.0,
            song: r.1,
            artist: r.2,
            album: r.3,
            status: r.4,
            attempts: r.5,
            results: r.6,
            last_error: r.7,
            updated_at: r.8,
        })
        .collect())
}

pub async fn misses(pool: &PgPool) -> Result<Vec<TrackRequest>, sqlx::Error> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT song, artist, album FROM dj_ingest_jobs
           WHERE status IN ('done', 'failed')
             AND NOT EXISTS (SELECT 1 FROM jsonb_each(results) r WHERE r.value->>'state' = 'found')
           ORDER BY artist, song"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(song, artist, album)| TrackRequest {
            song,
            artist,
            album,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::passages::testutil::*;

    fn track(tag: &str) -> TrackRequest {
        TrackRequest {
            song: format!("Job Song {tag}"),
            artist: "Job Artist".into(),
            album: None,
        }
    }

    async fn status(pool: &PgPool, id: i64) -> String {
        sqlx::query_scalar("SELECT status FROM dj_ingest_jobs WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_and_concurrent_claims_are_exclusive() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let t = track(&rand::random::<u64>().to_string());
        assert_eq!(
            enqueue(&pool, &t, None, false, false).await.unwrap(),
            Enqueue::Queued
        );
        assert_eq!(
            enqueue(&pool, &t, None, false, false).await.unwrap(),
            Enqueue::AlreadyQueued
        );
        let (a, b) = tokio::join!(claim(&pool), claim(&pool));
        let ids: Vec<i64> = [a.unwrap(), b.unwrap()]
            .into_iter()
            .flatten()
            .map(|j| j.id)
            .collect();
        assert!(ids.len() < 2 || ids[0] != ids[1]);
    }

    #[tokio::test]
    async fn claim_takes_over_a_stale_running_job_but_not_a_fresh_one() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let stale = track(&format!("stale-{}", rand::random::<u64>()));
        let fresh_job = track(&format!("fresh-{}", rand::random::<u64>()));
        enqueue(&pool, &stale, None, false, false).await.unwrap();
        enqueue(&pool, &fresh_job, None, false, false)
            .await
            .unwrap();

        async fn id_of(pool: &PgPool, t: &TrackRequest) -> i64 {
            let (sk, ak, alk) = job_keys(t);
            sqlx::query_scalar(
                "SELECT id FROM dj_ingest_jobs WHERE song_key=$1 AND artist_key=$2 AND album_key=$3",
            )
            .bind(&sk)
            .bind(&ak)
            .bind(&alk)
            .fetch_one(pool)
            .await
            .unwrap()
        }
        let stale_id = id_of(&pool, &stale).await;
        let fresh_id = id_of(&pool, &fresh_job).await;

        sqlx::query(
            "UPDATE dj_ingest_jobs SET status='running', updated_at = NOW() - INTERVAL '31 minutes' WHERE id=$1",
        )
        .bind(stale_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE dj_ingest_jobs SET status='running', updated_at = NOW() - INTERVAL '1 minute' WHERE id=$1",
        )
        .bind(fresh_id)
        .execute(&pool)
        .await
        .unwrap();

        // Other tests leave stray `queued` rows behind, so drain whatever claim() hands back
        // until our stale job turns up (or work runs out), rather than assuming it's claimed
        // first.
        let mut reclaimed_stale = false;
        for _ in 0..2000 {
            match claim(&pool).await.unwrap() {
                Some(job) => {
                    assert_ne!(
                        job.id, fresh_id,
                        "a running job updated 1 minute ago must not be reclaimed"
                    );
                    if job.id == stale_id {
                        reclaimed_stale = true;
                        break;
                    }
                }
                None => break,
            }
        }
        assert!(
            reclaimed_stale,
            "a running job updated 31 minutes ago must be claimable"
        );

        let fresh_attempts: i32 =
            sqlx::query_scalar("SELECT attempts FROM dj_ingest_jobs WHERE id=$1")
                .bind(fresh_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            fresh_attempts, 0,
            "the fresh running job's lease must be untouched"
        );
    }

    #[tokio::test]
    async fn finish_retries_errors_then_marks_done_and_lists_misses() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let t = track(&rand::random::<u64>().to_string());
        enqueue(&pool, &t, None, false, false).await.unwrap();
        let (sk, ak, alk) = job_keys(&t);
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM dj_ingest_jobs WHERE song_key=$1 AND artist_key=$2 AND album_key=$3",
        )
        .bind(&sk)
        .bind(&ak)
        .bind(&alk)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut results = Map::new();
        results.insert("wikipedia".into(), json!({"state": "no_match"}));
        results.insert(
            "genius".into(),
            json!({"state": "error", "message": "timeout"}),
        );
        let job = Job {
            id,
            track: t.clone(),
            attempts: 1,
            results: Map::new(),
        };
        finish(&pool, &job, results.clone()).await.unwrap();
        assert_eq!(status(&pool, id).await, "queued");
        finish(
            &pool,
            &Job {
                attempts: MAX_ATTEMPTS,
                ..job
            },
            results,
        )
        .await
        .unwrap();
        assert_eq!(status(&pool, id).await, "done");
        assert!(
            misses(&pool)
                .await
                .unwrap()
                .iter()
                .any(|m| m.song == t.song)
        );
        assert_eq!(
            enqueue(&pool, &t, None, true, false).await.unwrap(),
            Enqueue::RecentlyDone
        );
        assert_eq!(
            enqueue(&pool, &t, None, false, true).await.unwrap(),
            Enqueue::Queued
        );
        assert!(
            list(&pool, 10, None)
                .await
                .unwrap()
                .iter()
                .any(|j| j.id == id)
        );
    }

    #[tokio::test]
    async fn failed_jobs_where_every_source_errored_are_also_misses() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let t = track(&format!("failed-{}", rand::random::<u64>()));
        enqueue(&pool, &t, None, false, false).await.unwrap();
        let (sk, ak, alk) = job_keys(&t);
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM dj_ingest_jobs WHERE song_key=$1 AND artist_key=$2 AND album_key=$3",
        )
        .bind(&sk)
        .bind(&ak)
        .bind(&alk)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut results = Map::new();
        results.insert(
            "wikipedia".into(),
            json!({"state": "error", "message": "timeout"}),
        );
        let job = Job {
            id,
            track: t.clone(),
            attempts: MAX_ATTEMPTS,
            results: Map::new(),
        };
        finish(&pool, &job, results).await.unwrap();
        assert_eq!(status(&pool, id).await, "failed");
        assert!(
            misses(&pool)
                .await
                .unwrap()
                .iter()
                .any(|m| m.song == t.song),
            "a failed job (every source errored) is still a retry candidate"
        );
    }

    #[tokio::test]
    async fn auto_count_counts_only_auto_requests_today() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let (user, _) = user_with_token(&pool, false).await;
        enqueue(
            &pool,
            &track(&format!("a{}", rand::random::<u64>())),
            Some(user),
            true,
            false,
        )
        .await
        .unwrap();
        enqueue(
            &pool,
            &track(&format!("b{}", rand::random::<u64>())),
            Some(user),
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(auto_count_today(&pool, user).await.unwrap(), 1);
    }
}
