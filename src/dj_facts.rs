use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    dj_fact_sources::{self, fold_dashes},
    errors::ApiError,
    handlers::authenticate_with_headers,
    state::AppContext,
};

#[derive(Deserialize)]
pub struct FactQuery {
    song: String,
    artist: String,
}

#[derive(Deserialize)]
pub struct Contribution {
    song: String,
    artist: String,
    claim: String,
    source: String,
    source_id: String,
    source_url: String,
    evidence: String,
}

#[derive(Serialize)]
pub struct Fact {
    id: i64,
    fingerprint: String,
    song: String,
    artist: String,
    claim: String,
    source: String,
    source_id: String,
    source_url: String,
    evidence: String,
}

#[derive(Serialize)]
pub struct FactResponse {
    fact: Option<Fact>,
}

#[derive(Deserialize)]
pub struct Played {
    event_id: String,
    played_at: DateTime<Utc>,
}

fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn identity_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Key stored in song_key/artist_key and used for lookup: identity_key with dash variants folded.
/// Fingerprints keep plain identity_key so already-stored facts still deduplicate.
fn match_key(value: &str) -> String {
    fold_dashes(&identity_key(value))
}

/// Words that mark a trailing "(...)"/"[...]" as a release qualifier rather than part of the title.
const TITLE_QUALIFIERS: &[&str] = &[
    "feat",
    "feat.",
    "ft",
    "ft.",
    "featuring",
    "with",
    "from",
    "remaster",
    "remastered",
    "version",
    "live",
    "remix",
    "edit",
    "mix",
    "mono",
    "stereo",
    "acoustic",
    "demo",
    "instrumental",
    "explicit",
    "ver",
    "ver.",
];

/// Lookup keys for a player-supplied title: the title as given, plus the title with release
/// qualifiers removed ("American Woman - 2024 Remaster", "Song (feat. X)"), since facts are
/// stored under the source's canonical song title.
fn song_keys(title: &str) -> Vec<String> {
    let mut base = title.split(" - ").next().unwrap_or(title).trim_end();
    while let Some(close) = base.chars().last().filter(|c| matches!(c, ')' | ']')) {
        let open = if close == ')' { '(' } else { '[' };
        let Some(start) = base.rfind(open) else { break };
        let inner = base[start + 1..base.len() - 1].to_lowercase();
        let qualifier = inner.trim().chars().all(|c| c.is_ascii_digit())
            || inner
                .split(|c: char| !c.is_alphanumeric() && c != '.')
                .any(|word| TITLE_QUALIFIERS.contains(&word));
        if !qualifier || start == 0 {
            break;
        }
        base = base[..start].trim_end();
    }
    let mut keys = vec![match_key(title)];
    if !base.is_empty() && !keys.contains(&match_key(base)) {
        keys.push(match_key(base));
    }
    keys
}

/// Lookup keys for a player-supplied artist: the string as given plus each credited artist of a
/// multi-artist credit ("A, B & C"), since facts are stored under the source's credited artist.
fn artist_keys(artist: &str) -> Vec<String> {
    let mut parts = vec![artist.to_string()];
    for sep in [",", "&", "、", " feat. ", " ft. ", " featuring "] {
        parts = parts
            .iter()
            .flat_map(|part| part.split(sep).map(str::to_string).collect::<Vec<_>>())
            .collect();
    }
    let mut keys = vec![match_key(artist)];
    for key in parts.iter().map(|p| match_key(p)) {
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.truncate(16);
    keys
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

#[cfg(test)]
fn recovery_score(days: f64) -> Option<f64> {
    if days < 30.0 {
        None
    } else {
        Some(((days - 30.0).ln_1p() / 366.0_f64.ln()).min(1.0))
    }
}

pub async fn lookup(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(query): Query<FactQuery>,
) -> Result<Json<FactResponse>, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    if !bounded(&query.song, 200) || !bounded(&query.artist, 200) {
        return Err(ApiError::bad_request("invalid song or artist".into()));
    }
    let row = sqlx::query_as::<_, (i64, String, String, String, String, String, String, String, String)>(
        r#"SELECT f.id, f.fingerprint, f.song, f.artist, f.claim, f.source, f.source_id, f.source_url, f.evidence
           FROM dj_facts f
           LEFT JOIN LATERAL (
               SELECT MAX(played_at) AS last_used_at FROM dj_fact_plays
               WHERE user_id = $3 AND fact_id = f.id
           ) usage ON TRUE
           WHERE f.song_key = ANY($1) AND f.artist_key = ANY($2)
             AND (usage.last_used_at IS NULL OR usage.last_used_at <= NOW() - INTERVAL '30 days')
           ORDER BY (usage.last_used_at IS NULL) DESC,
             LEAST(1.0, LN(1 + EXTRACT(EPOCH FROM (NOW() - usage.last_used_at)) / 86400 - 30) / LN(366)) DESC NULLS LAST,
             f.verified_at DESC, f.id ASC
           LIMIT 1"#,
    )
    .bind(song_keys(&query.song)).bind(artist_keys(&query.artist)).bind(user.id)
    .fetch_optional(&state.pool).await
    .map_err(|err| { tracing::error!(%err, "fact lookup failed"); ApiError::internal("fact lookup failed".into()) })?;
    Ok(Json(FactResponse {
        fact: row.map(|r| Fact {
            id: r.0,
            fingerprint: r.1,
            song: r.2,
            artist: r.3,
            claim: r.4,
            source: r.5,
            source_id: r.6,
            source_url: r.7,
            evidence: r.8,
        }),
    }))
}

pub async fn contribute(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Json(input): Json<Contribution>,
) -> Result<(StatusCode, Json<Fact>), ApiError> {
    authenticate_with_headers(&state, &headers).await?;
    if !bounded(&input.song, 200)
        || !bounded(&input.artist, 200)
        || !bounded(&input.claim, 500)
        || !bounded(&input.evidence, 1000)
        || !bounded(&input.source_id, 160)
        || !bounded(&input.source_url, 300)
    {
        return Err(ApiError::bad_request("invalid fact fields".into()));
    }
    let source_url = dj_fact_sources::verify(
        &input.source,
        &input.source_id,
        &input.song,
        &input.artist,
        &input.evidence,
        &input.claim,
    )
    .await?;
    if input.source_url != source_url {
        return Err(ApiError::bad_request(
            "source URL does not match source identity".into(),
        ));
    }
    let normalized = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.source,
        input.source_id,
        identity_key(&input.song),
        identity_key(&input.artist),
        identity_key(&input.claim),
        identity_key(&input.evidence)
    );
    let fingerprint = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    let row = sqlx::query_as::<_, (i64, String, String, String, String, String, String, String, String)>(
        r#"INSERT INTO dj_facts (song_key, artist_key, song, artist, claim, source, source_id, source_url, evidence, fingerprint)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
           ON CONFLICT (fingerprint) DO UPDATE SET fingerprint = dj_facts.fingerprint
           RETURNING id, fingerprint, song, artist, claim, source, source_id, source_url, evidence"#,
    )
    .bind(match_key(&input.song)).bind(match_key(&input.artist))
    .bind(&input.song).bind(&input.artist).bind(&input.claim).bind(&input.source)
    .bind(&input.source_id).bind(&source_url).bind(&input.evidence).bind(&fingerprint)
    .fetch_one(&state.pool).await
    .map_err(|err| { tracing::error!(%err, "fact insert failed"); ApiError::internal("fact insert failed".into()) })?;
    Ok((
        StatusCode::CREATED,
        Json(Fact {
            id: row.0,
            fingerprint: row.1,
            song: row.2,
            artist: row.3,
            claim: row.4,
            source: row.5,
            source_id: row.6,
            source_url: row.7,
            evidence: row.8,
        }),
    ))
}

pub async fn played(
    State(state): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<Played>,
) -> Result<StatusCode, ApiError> {
    let user = authenticate_with_headers(&state, &headers).await?;
    if id <= 0
        || !valid_uuid(&input.event_id)
        || input.played_at > Utc::now() + chrono::Duration::minutes(5)
    {
        return Err(ApiError::bad_request("invalid played event".into()));
    }
    let inserted = sqlx::query(
        r#"INSERT INTO dj_fact_plays (user_id, event_id, fact_id, played_at)
           SELECT $1, $2, id, $4 FROM dj_facts WHERE id = $3
           ON CONFLICT (user_id, event_id) DO NOTHING"#,
    )
    .bind(user.id)
    .bind(&input.event_id)
    .bind(id)
    .bind(input.played_at)
    .execute(&state.pool)
    .await
    .map_err(|err| {
        tracing::error!(%err, "played event failed");
        ApiError::internal("played event failed".into())
    })?;
    if inserted.rows_affected() == 0 {
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT fact_id FROM dj_fact_plays WHERE user_id = $1 AND event_id = $2",
        )
        .bind(user.id)
        .bind(&input.event_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| ApiError::internal("played event lookup failed".into()))?;
        if existing != Some(id) {
            return Err(ApiError::not_found(
                "fact not found or event ID used for another fact".into(),
            ));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{create_token, create_user, ensure_schema},
        state::DjVoiceCatalog,
    };
    use axum::{
        extract::State,
        http::{HeaderValue, header},
    };

    #[test]
    fn song_keys_add_title_without_release_qualifiers() {
        assert_eq!(song_keys("Creep"), vec!["creep"]);
        assert_eq!(
            song_keys("American Woman - 2024 Remaster"),
            vec!["american woman - 2024 remaster", "american woman"]
        );
        assert_eq!(
            song_keys("The Game Of Love (feat. Michelle Branch)"),
            vec![
                "the game of love (feat. michelle branch)",
                "the game of love"
            ]
        );
        assert_eq!(
            song_keys("Show Me The Meaning Of Being Lonely ( 1999 ) - Backstreet Boys")[1],
            "show me the meaning of being lonely"
        );
        // Leading or non-qualifier parentheses are part of the title.
        assert_eq!(song_keys("(Going Down) Love In An Elevator").len(), 1);
        assert_eq!(song_keys("Sweet Dreams (Are Made of This)").len(), 1);
    }

    #[test]
    fn keys_fold_dashes_and_strip_version_markers() {
        assert_eq!(
            artist_keys("Bachman\u{2013}Turner Overdrive"),
            vec!["bachman-turner overdrive"]
        );
        assert_eq!(
            song_keys("Seven (feat. Latto) (Explicit Ver.)"),
            vec!["seven (feat. latto) (explicit ver.)", "seven"]
        );
    }

    #[test]
    fn artist_keys_add_each_credited_artist() {
        assert_eq!(artist_keys("TWICE"), vec!["twice"]);
        assert_eq!(
            artist_keys("Reneé Rapp, Megan Thee Stallion"),
            vec![
                "reneé rapp, megan thee stallion",
                "reneé rapp",
                "megan thee stallion"
            ]
        );
        assert_eq!(
            artist_keys("George Thorogood & The Destroyers"),
            vec![
                "george thorogood & the destroyers",
                "george thorogood",
                "the destroyers"
            ]
        );
    }

    #[test]
    fn recovery_begins_only_after_thirty_days() {
        assert_eq!(recovery_score(29.999), None);
        assert_eq!(recovery_score(30.0), Some(0.0));
        assert_eq!(recovery_score(31.0), Some(1.0_f64.ln_1p() / 366.0_f64.ln()));
        assert_eq!(recovery_score(395.0), Some(1.0));
    }

    #[tokio::test]
    async fn lookup_and_played_are_account_private_and_idempotent() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("test database");
        ensure_schema(&pool).await.expect("schema");
        let suffix = format!("{}", rand::random::<u64>());
        let first = create_user(&pool, &format!("fact-test-{suffix}-1"), false)
            .await
            .expect("first user");
        let second = create_user(&pool, &format!("fact-test-{suffix}-2"), false)
            .await
            .expect("second user");
        let first_token = create_token(&pool, first.id, None)
            .await
            .expect("first token")
            .3;
        let second_token = create_token(&pool, second.id, None)
            .await
            .expect("second token")
            .3;
        let fact_id: i64 = sqlx::query_scalar(
            "INSERT INTO dj_facts (song_key, artist_key, song, artist, claim, source, source_id, source_url, evidence, fingerprint) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id"
        ).bind(&suffix).bind("test artist").bind(&suffix).bind("Test Artist")
            .bind("Test claim").bind("wikipedia").bind(&suffix).bind("https://en.wikipedia.org/wiki/Test")
            .bind("Test evidence").bind(&suffix).fetch_one(&pool).await.expect("fact");
        let state = Arc::new(AppContext::new(
            pool.clone(),
            None,
            DjVoiceCatalog::default(),
        ));
        let headers = |token: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).expect("header"),
            );
            headers
        };
        let query = || FactQuery {
            song: suffix.clone(),
            artist: "Test Artist".into(),
        };
        assert_eq!(
            lookup(State(state.clone()), headers(&first_token), Query(query()))
                .await
                .unwrap()
                .0
                .fact
                .unwrap()
                .id,
            fact_id
        );
        let event_id = "12345678-1234-1234-1234-123456789abc";
        let played_at = Utc::now();
        let post = || {
            played(
                State(state.clone()),
                headers(&first_token),
                Path(fact_id),
                Json(Played {
                    event_id: event_id.into(),
                    played_at,
                }),
            )
        };
        let (a, b) = tokio::join!(post(), post());
        assert_eq!(a.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(b.unwrap(), StatusCode::NO_CONTENT);
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dj_fact_plays WHERE user_id=$1 AND fact_id=$2",
        )
        .bind(first.id)
        .bind(fact_id)
        .fetch_one(&pool)
        .await
        .expect("play count");
        assert_eq!(count, 1);
        assert!(
            lookup(State(state.clone()), headers(&first_token), Query(query()))
                .await
                .unwrap()
                .0
                .fact
                .is_none()
        );
        assert_eq!(
            lookup(State(state.clone()), headers(&second_token), Query(query()))
                .await
                .unwrap()
                .0
                .fact
                .unwrap()
                .id,
            fact_id
        );
        let qualified = FactQuery {
            song: format!("{suffix} - 2024 Remaster"),
            artist: "Test Artist, Someone Else".into(),
        };
        assert_eq!(
            lookup(
                State(state.clone()),
                headers(&second_token),
                Query(qualified)
            )
            .await
            .unwrap()
            .0
            .fact
            .unwrap()
            .id,
            fact_id
        );
        sqlx::query(
            "UPDATE dj_fact_plays SET played_at = NOW() - INTERVAL '30 days' WHERE user_id=$1",
        )
        .bind(first.id)
        .execute(&pool)
        .await
        .expect("boundary");
        assert_eq!(
            lookup(State(state.clone()), headers(&first_token), Query(query()))
                .await
                .unwrap()
                .0
                .fact
                .unwrap()
                .id,
            fact_id
        );
        sqlx::query("DELETE FROM users WHERE id IN ($1,$2)")
            .bind(first.id)
            .bind(second.id)
            .execute(&pool)
            .await
            .expect("cleanup users");
        sqlx::query("DELETE FROM dj_facts WHERE id=$1")
            .bind(fact_id)
            .execute(&pool)
            .await
            .expect("cleanup fact");
    }
}
