//! Background ingest: claims one job at a time, fetches every pending source concurrently,
//! stores what they return, and records a per-source outcome on the job.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::join_all;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::match_keys::{artist_parts, base_title, match_key};
use crate::passages::{
    Subject,
    embed::Embed,
    jobs::{self, Job},
    sources::{Scope, SourceError, SourceResult, Sources},
    store,
};

pub async fn run(pool: PgPool, embedder: Arc<dyn Embed>, sources: Arc<Sources>) {
    loop {
        match jobs::claim(&pool).await {
            Ok(Some(job)) => {
                let id = job.id;
                if let Err(err) = process(&pool, &embedder, &sources, job).await {
                    tracing::error!(job = id, %err, "DJ ingest job failed");
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(5)).await,
            Err(err) => {
                tracing::error!(%err, "DJ ingest claim failed");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

pub fn outcome_for(result: &SourceResult) -> Value {
    match result {
        Ok(docs) if docs.is_empty() => json!({"state": "no_match"}),
        Ok(docs) => json!({"state": "found", "documents": docs.len()}),
        Err(SourceError::Permanent(_)) => json!({"state": "no_match"}),
        Err(SourceError::Transient(message)) => json!({"state": "error", "message": message}),
    }
}

pub async fn store_results(
    pool: &PgPool,
    embedder: &Arc<dyn Embed>,
    result: SourceResult,
) -> Value {
    let Ok(docs) = &result else {
        return outcome_for(&result);
    };
    for doc in docs {
        if let Err(err) = store::upsert_document(pool, embedder, doc, None).await {
            return json!({"state": "error", "message": format!("store: {err}")});
        }
    }
    outcome_for(&result)
}

async fn scope_for(pool: &PgPool, source: &str, job: &Job) -> Scope {
    // Every credited artist's own match key, so album/artist freshness never crosses artists
    // (e.g. one artist's "Greatest Hits" must not suppress another artist's album of the same
    // name).
    let credited_keys: Vec<String> = crate::match_keys::artist_keys(&job.track.artist);
    let mut artist_fresh = true;
    for artist in artist_parts(&job.track.artist) {
        if !store::fresh(
            pool,
            source,
            Subject::Artist,
            &match_key(&artist),
            &credited_keys,
        )
        .await
        .unwrap_or(false)
        {
            artist_fresh = false;
        }
    }
    let album_fresh = match &job.track.album {
        Some(album) => store::fresh(
            pool,
            source,
            Subject::Album,
            &match_key(base_title(album)),
            &credited_keys,
        )
        .await
        .unwrap_or(false),
        None => true,
    };
    Scope {
        song: true,
        album: !album_fresh,
        artist: !artist_fresh,
    }
}

pub async fn process(
    pool: &PgPool,
    embedder: &Arc<dyn Embed>,
    sources: &Sources,
    job: Job,
) -> anyhow::Result<()> {
    let pending: Vec<&'static str> = sources
        .names()
        .into_iter()
        .filter(|name| {
            !matches!(
                job.results.get(*name).and_then(|v| v["state"].as_str()),
                Some("found" | "no_match")
            )
        })
        .collect();
    let job_ref = &job;
    let outcomes = join_all(pending.iter().map(|name| async move {
        let scope = scope_for(pool, name, job_ref).await;
        let result = AssertUnwindSafe(sources.fetch(name, &job_ref.track, scope))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(SourceError::Transient("source panicked".into())));
        (*name, store_results(pool, embedder, result).await)
    }))
    .await;
    let mut results = job.results.clone();
    for (name, outcome) in outcomes {
        results.insert(name.to_string(), outcome);
    }
    jobs::finish(pool, &job, results).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passages::{NewDocument, Subject, embed::HashEmbedder, testutil::test_pool};

    #[test]
    fn outcome_maps_results_to_job_states() {
        assert_eq!(outcome_for(&Ok(vec![])), json!({"state": "no_match"}));
        assert_eq!(
            outcome_for(&Err(SourceError::Transient("t".into())))["state"],
            "error"
        );
        // A permanent error (404, disallowed host) is recorded as no match, retried after 30 days.
        assert_eq!(
            outcome_for(&Err(SourceError::Permanent("HTTP 404".into())))["state"],
            "no_match"
        );
    }

    #[tokio::test]
    async fn store_results_upserts_documents_and_counts_them() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let e: Arc<dyn Embed> = Arc::new(HashEmbedder);
        let tag = rand::random::<u64>();
        let doc = NewDocument {
            source: "songfacts".into(),
            source_ref: format!("/facts/worker/{tag}"),
            source_url: "https://www.songfacts.com/x".into(),
            subject: Subject::Song,
            song: Some(format!("Worker Song {tag}")),
            album: None,
            artist: "Worker Artist".into(),
            title: "Worker Song".into(),
            lang: "en".into(),
            body: "A story. Another sentence.".into(),
        };
        let outcome = store_results(&pool, &e, Ok(vec![doc])).await;
        assert_eq!(outcome, json!({"state": "found", "documents": 1}));
    }
}
