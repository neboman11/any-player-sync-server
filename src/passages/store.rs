//! Persistence for passage documents, chunks, and per-account passage plays.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::match_keys::{base_title, match_key};
use crate::passages::{
    NewDocument, Subject,
    chunk::chunk,
    embed::{Embed, EmbedKind},
    rank::Candidate,
};

pub async fn upsert_document(
    pool: &PgPool,
    embedder: &Arc<dyn Embed>,
    doc: &NewDocument,
    added_by: Option<i64>,
) -> anyhow::Result<i64> {
    let hash = format!("{:x}", Sha256::digest(doc.body.as_bytes()));
    let existing: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, content_hash FROM dj_documents WHERE source=$1 AND source_ref=$2",
    )
    .bind(&doc.source)
    .bind(&doc.source_ref)
    .fetch_optional(pool)
    .await?;
    if let Some((id, existing_hash)) = &existing
        && *existing_hash == hash
    {
        sqlx::query("UPDATE dj_documents SET fetched_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
        return Ok(*id);
    }
    let e = embedder.clone();
    let body = doc.body.clone();
    let (texts, counts, vectors) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let texts = chunk(&body, &|t| e.count_tokens(t));
        let counts: Vec<usize> = texts.iter().map(|t| e.count_tokens(t)).collect();
        let vectors = e.embed(&texts, EmbedKind::Passage)?;
        Ok((texts, counts, vectors))
    })
    .await??;

    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO dj_documents (source, source_ref, source_url, subject, song_key, album_key,
               artist_key, title, lang, body, content_hash, added_by)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
           ON CONFLICT (source, source_ref) DO UPDATE SET
               source_url = EXCLUDED.source_url, subject = EXCLUDED.subject,
               song_key = EXCLUDED.song_key, album_key = EXCLUDED.album_key,
               artist_key = EXCLUDED.artist_key, title = EXCLUDED.title, lang = EXCLUDED.lang,
               body = EXCLUDED.body, content_hash = EXCLUDED.content_hash, fetched_at = NOW()
           RETURNING id"#,
    )
    .bind(&doc.source)
    .bind(&doc.source_ref)
    .bind(&doc.source_url)
    .bind(doc.subject.as_str())
    .bind(doc.song.as_deref().map(|s| match_key(base_title(s))))
    .bind(doc.album.as_deref().map(|a| match_key(base_title(a))))
    .bind(match_key(&doc.artist))
    .bind(&doc.title)
    .bind(&doc.lang)
    .bind(&doc.body)
    .bind(&hash)
    .bind(added_by)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM dj_chunks WHERE document_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for (ord, ((text, vector), count)) in texts.iter().zip(vectors).zip(counts).enumerate() {
        sqlx::query("INSERT INTO dj_chunks (document_id, ord, text, token_count, embedding) VALUES ($1,$2,$3,$4,$5)")
            .bind(id)
            .bind(ord as i32)
            .bind(text)
            .bind(count as i32)
            .bind(Vector::from(vector))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn delete_document(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM dj_documents WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

type CandidateRow = (
    i64,
    i64,
    String,
    i32,
    Vector,
    String,
    String,
    String,
    String,
    String,
    Option<DateTime<Utc>>,
);

pub async fn candidates(
    pool: &PgPool,
    song_keys: &[String],
    artist_keys: &[String],
    album_key: Option<&str>,
    user_id: i64,
) -> Result<Vec<Candidate>, sqlx::Error> {
    let rows: Vec<CandidateRow> = sqlx::query_as(
        r#"SELECT c.id, c.document_id, c.text, c.token_count, c.embedding, d.source, d.source_url,
                  d.title, d.subject, d.lang,
                  (SELECT MAX(p.played_at) FROM dj_passage_plays p
                    WHERE p.user_id = $4 AND p.chunk_id = c.id) AS last_played
           FROM dj_chunks c JOIN dj_documents d ON d.id = c.document_id
           WHERE (d.subject = 'song' AND d.song_key = ANY($1) AND d.artist_key = ANY($2))
              OR (d.subject = 'album' AND $3::text IS NOT NULL AND d.album_key = $3 AND d.artist_key = ANY($2))
              OR (d.subject = 'artist' AND d.artist_key = ANY($2))
           ORDER BY c.id
           LIMIT 3000"#,
    )
    .bind(song_keys)
    .bind(artist_keys)
    .bind(album_key)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            chunk_id: r.0,
            document_id: r.1,
            text: r.2,
            token_count: r.3.max(0) as usize,
            embedding: r.4.to_vec(),
            source: r.5,
            source_url: r.6,
            title: r.7,
            subject: match r.8.as_str() {
                "song" => Subject::Song,
                "album" => Subject::Album,
                _ => Subject::Artist,
            },
            lang: r.9,
            last_played: r.10,
        })
        .collect())
}

pub async fn record_plays(
    pool: &PgPool,
    user_id: i64,
    event_id: &str,
    chunk_ids: &[i64],
    played_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO dj_passage_plays (user_id, event_id, chunk_id, played_at)
           SELECT $1, $2, id, $4 FROM dj_chunks WHERE id = ANY($3)
           ON CONFLICT (user_id, event_id, chunk_id) DO NOTHING"#,
    )
    .bind(user_id)
    .bind(event_id)
    .bind(chunk_ids)
    .bind(played_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// True if a document for this subject/key, credited to one of `artist_keys`, was fetched from
/// `source_prefix` within the last 30 days. `artist_keys` also gates song and artist subjects
/// (every document row carries a non-null `artist_key`), so a caller with a single known artist
/// key just passes a one-element slice.
pub async fn fresh(
    pool: &PgPool,
    source_prefix: &str,
    subject: Subject,
    key: &str,
    artist_keys: &[String],
) -> Result<bool, sqlx::Error> {
    let column = match subject {
        Subject::Song => "song_key",
        Subject::Album => "album_key",
        Subject::Artist => "artist_key",
    };
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM dj_documents WHERE source LIKE $1 || '%' AND subject = $2 \
         AND {column} = $3 AND artist_key = ANY($4) AND fetched_at > NOW() - INTERVAL '30 days')"
    ))
    .bind(source_prefix)
    .bind(subject.as_str())
    .bind(key)
    .bind(artist_keys)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passages::{embed::HashEmbedder, testutil::*};

    fn doc(source_ref: &str, body: &str) -> NewDocument {
        NewDocument {
            source: "wikipedia-en".into(),
            source_ref: source_ref.into(),
            source_url: format!("https://en.wikipedia.org/wiki/{source_ref}"),
            subject: Subject::Song,
            song: Some("Store Test Song".into()),
            album: None,
            artist: "Store Test Artist".into(),
            title: source_ref.into(),
            lang: "en".into(),
            body: body.into(),
        }
    }

    async fn chunk_ids(pool: &PgPool, id: i64) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM dj_chunks WHERE document_id=$1 ORDER BY id")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upsert_rebuilds_chunks_only_when_body_changes() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let e: Arc<dyn Embed> = Arc::new(HashEmbedder);
        let r = format!("store-test-{}", rand::random::<u64>());
        let id = upsert_document(
            &pool,
            &e,
            &doc(&r, "First version. It has two sentences."),
            None,
        )
        .await
        .unwrap();
        let first = chunk_ids(&pool, id).await;
        assert!(!first.is_empty());
        assert_eq!(
            upsert_document(
                &pool,
                &e,
                &doc(&r, "First version. It has two sentences."),
                None
            )
            .await
            .unwrap(),
            id
        );
        assert_eq!(chunk_ids(&pool, id).await, first);
        assert_eq!(
            upsert_document(&pool, &e, &doc(&r, "Second version entirely."), None)
                .await
                .unwrap(),
            id
        );
        assert!(
            chunk_ids(&pool, id)
                .await
                .iter()
                .all(|c| !first.contains(c))
        );
        assert!(delete_document(&pool, id).await.unwrap());
        assert!(!delete_document(&pool, id).await.unwrap());
    }

    #[tokio::test]
    async fn candidates_match_subject_keys_and_carry_last_play() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let e: Arc<dyn Embed> = Arc::new(HashEmbedder);
        let r = format!("cand-test-{}", rand::random::<u64>());
        let id = upsert_document(&pool, &e, &doc(&r, "A story about the recording."), None)
            .await
            .unwrap();
        let (user, _) = user_with_token(&pool, false).await;
        let songs = vec!["store test song".to_string()];
        let artists = vec!["store test artist".to_string()];
        let found = candidates(&pool, &songs, &artists, None, user)
            .await
            .unwrap();
        let mine = found.iter().find(|c| c.document_id == id).unwrap().clone();
        assert!(mine.last_played.is_none());
        let event = "12345678-1234-1234-1234-123456789abc";
        record_plays(&pool, user, event, &[mine.chunk_id], Utc::now())
            .await
            .unwrap();
        record_plays(&pool, user, event, &[mine.chunk_id], Utc::now())
            .await
            .unwrap();
        let plays: i64 =
            sqlx::query_scalar("SELECT count(*) FROM dj_passage_plays WHERE user_id=$1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(plays, 1);
        let again = candidates(&pool, &songs, &artists, None, user)
            .await
            .unwrap();
        assert!(
            again
                .iter()
                .find(|c| c.chunk_id == mine.chunk_id)
                .unwrap()
                .last_played
                .is_some()
        );
        let other = candidates(&pool, &["other".into()], &artists, None, user)
            .await
            .unwrap();
        assert!(other.iter().all(|c| c.document_id != id));
        assert!(
            fresh(
                &pool,
                "wikipedia",
                Subject::Song,
                "store test song",
                &["store test artist".to_string()]
            )
            .await
            .unwrap()
        );
        delete_document(&pool, id).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_album_requires_a_matching_artist() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let e: Arc<dyn Embed> = Arc::new(HashEmbedder);
        let tag = rand::random::<u64>();
        let album = format!("Greatest Hits {tag}");
        let artist_a = format!("Artist A {tag}");
        let artist_b = format!("Artist B {tag}");
        let mut d = doc(&format!("album-{tag}"), "Liner notes about the album.");
        d.subject = Subject::Album;
        d.song = None;
        d.album = Some(album.clone());
        d.artist = artist_a.clone();
        let id = upsert_document(&pool, &e, &d, None).await.unwrap();

        let album_key = match_key(base_title(&album));
        // Same album, same artist: fresh.
        assert!(
            fresh(
                &pool,
                "wikipedia",
                Subject::Album,
                &album_key,
                &[match_key(&artist_a)]
            )
            .await
            .unwrap()
        );
        // Same album title, a different artist: must not be treated as fresh (the bug this
        // regression test guards against: one artist's "Greatest Hits" suppressing every other
        // artist's album of the same name for 30 days).
        assert!(
            !fresh(
                &pool,
                "wikipedia",
                Subject::Album,
                &album_key,
                &[match_key(&artist_b)]
            )
            .await
            .unwrap()
        );
        // fresh()==false path: right artist, but no document from this source at all.
        assert!(
            !fresh(
                &pool,
                "songfacts",
                Subject::Album,
                &album_key,
                &[match_key(&artist_a)]
            )
            .await
            .unwrap()
        );
        delete_document(&pool, id).await.unwrap();
    }
}
