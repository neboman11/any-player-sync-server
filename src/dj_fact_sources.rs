use crate::errors::ApiError;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::time::Duration;

fn invalid(message: &str) -> ApiError {
    ApiError::bad_request(message.into())
}

fn key(value: &str) -> String {
    value
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn wiki_title_url(id: &str) -> String {
    id.replace(' ', "_")
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"_-.*".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn canonical(source: &str, id: &str) -> Option<String> {
    match source {
        "wikipedia"
            if !id.is_empty()
                && id.len() <= 160
                && id
                    .chars()
                    .all(|c| c.is_alphanumeric() || " _-(),.'".contains(c)) =>
        {
            Some(format!(
                "https://en.wikipedia.org/wiki/{}",
                wiki_title_url(id)
            ))
        }
        "wikidata" if valid_qid(id) => Some(format!("https://www.wikidata.org/wiki/{id}")),
        "musicbrainz" if valid_uuid(id) => Some(format!("https://musicbrainz.org/recording/{id}")),
        _ => None,
    }
}

fn valid_qid(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 16
        && id.starts_with('Q')
        && id[1..].chars().all(|c| c.is_ascii_digit())
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        })
}

fn exact_structured(evidence: &str, claim: &str, supported: &[String]) -> Result<(), ApiError> {
    if evidence == claim && supported.iter().any(|fact| fact == claim) {
        Ok(())
    } else {
        Err(invalid("claim is not supported by source values"))
    }
}

fn check_wikipedia(
    page: &Value,
    id: &str,
    song: &str,
    artist: &str,
    evidence: &str,
    claim: &str,
) -> Result<(), ApiError> {
    if page.pointer("/query/redirects").is_some() {
        return Err(invalid("source page redirects"));
    }
    let pages = page
        .pointer("/query/pages")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("source page missing"))?;
    if pages.len() != 1 {
        return Err(invalid("ambiguous source page"));
    }
    let entry = pages.values().next().unwrap();
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("source title missing"))?;
    let extract = entry
        .get("extract")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("source passage missing"))?;
    let title_key = key(title);
    let song_key = key(song);
    let title_matches_song = title_key == song_key
        || (title_key.starts_with(&format!("{song_key} (")) && title_key.ends_with(')'));
    if entry.get("missing").is_some()
        || title_key != key(id)
        || !title_matches_song
        || evidence.is_empty()
        || claim.is_empty()
        || !key(evidence).contains(&key(song))
        || !key(evidence).contains(&key(artist))
        || !extract.contains(evidence)
        || evidence != claim
    {
        return Err(invalid("source does not support song, artist and claim"));
    }
    Ok(())
}

fn check_musicbrainz(
    page: &Value,
    id: &str,
    song: &str,
    artist: &str,
    evidence: &str,
    claim: &str,
) -> Result<(), ApiError> {
    if page.get("id").and_then(Value::as_str) != Some(id)
        || page
            .get("title")
            .and_then(Value::as_str)
            .map(key)
            .as_deref()
            != Some(key(song).as_str())
    {
        return Err(invalid("recording identity does not match"));
    }
    let credited = page
        .get("artist-credit")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("artist credit missing"))?;
    if !credited.iter().any(|credit| {
        let Some(entity) = credit.get("artist") else {
            return false;
        };
        ["name", "sort-name"].iter().any(|field| {
            entity
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|name| key(name) == key(artist))
        }) || entity
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| {
                aliases.iter().any(|alias| {
                    alias
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| key(name) == key(artist))
                })
            })
    }) {
        return Err(invalid("artist credit does not match"));
    }
    let mut supported = vec![format!("{song} is credited to {artist}.")];
    if let Some(date) = page.get("first-release-date").and_then(Value::as_str)
        && !date.is_empty()
    {
        supported.push(format!("{song} was released on {date}."));
    }
    exact_structured(evidence, claim, &supported)
}

fn check_wikidata(
    page: &Value,
    linked_artist: &Value,
    id: &str,
    song: &str,
    artist: &str,
    evidence: &str,
    claim: &str,
) -> Result<(), ApiError> {
    let entity = page
        .pointer(&format!("/entities/{id}"))
        .ok_or_else(|| invalid("song entity missing"))?;
    if entity.get("id").and_then(Value::as_str) != Some(id)
        || entity
            .pointer("/labels/en/value")
            .and_then(Value::as_str)
            .map(key)
            .as_deref()
            != Some(key(song).as_str())
    {
        return Err(invalid("song entity identity does not match"));
    }
    let performers = entity
        .pointer("/claims/P175")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("performer missing"))?;
    let matched = performers
        .iter()
        .filter_map(|v| {
            v.pointer("/mainsnak/datavalue/value/id")
                .and_then(Value::as_str)
        })
        .any(|performer_id| {
            valid_qid(performer_id)
                && linked_artist
                    .pointer(&format!("/entities/{performer_id}/id"))
                    .and_then(Value::as_str)
                    == Some(performer_id)
                && linked_artist
                    .pointer(&format!("/entities/{performer_id}/labels/en/value"))
                    .and_then(Value::as_str)
                    .map(key)
                    .as_deref()
                    == Some(key(artist).as_str())
        });
    if !matched {
        return Err(invalid("performer identity does not match"));
    }
    exact_structured(
        evidence,
        claim,
        &[format!("{song} is performed by {artist}.")],
    )
}

async fn fetch(client: &reqwest::Client, url: String) -> Result<Value, ApiError> {
    let mut retries = 0;
    let mut response = loop {
        let response = client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| ApiError::internal("source unavailable".into()))?;
        if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE || retries == 2 {
            break response;
        }
        drop(response);
        tokio::time::sleep(Duration::from_millis(250_u64 << retries)).await;
        retries += 1;
    };
    if !response.status().is_success() {
        if response.status().is_server_error()
            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(ApiError::internal("source unavailable".into()));
        }
        return Err(invalid("source record unavailable"));
    }
    if response.content_length().is_some_and(|len| len > 512_000) {
        return Err(invalid("source response too large"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::internal("source read failed".into()))?
    {
        if body.len() + chunk.len() > 512_000 {
            return Err(invalid("source response too large"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| invalid("invalid source response"))
}

pub async fn verify(
    source: &str,
    source_id: &str,
    song: &str,
    artist: &str,
    evidence: &str,
    claim: &str,
) -> Result<String, ApiError> {
    let url = canonical(source, source_id).ok_or_else(|| invalid("unsupported source identity"))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .user_agent("any-player-sync-server/1.0 (fact verification)")
        .build()
        .map_err(|_| ApiError::internal("source client unavailable".into()))?;
    let encoded = utf8_percent_encode(source_id, NON_ALPHANUMERIC);
    match source {
        "wikipedia" => {
            let page = fetch(&client, format!("https://en.wikipedia.org/w/api.php?action=query&prop=extracts&explaintext=1&format=json&titles={encoded}")).await?;
            check_wikipedia(&page, source_id, song, artist, evidence, claim)?;
        }
        "musicbrainz" => {
            let page = fetch(
                &client,
                format!("https://musicbrainz.org/ws/2/recording/{source_id}?inc=artists&fmt=json"),
            )
            .await?;
            check_musicbrainz(&page, source_id, song, artist, evidence, claim)?;
        }
        "wikidata" => {
            let page = fetch(&client, format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={source_id}&props=labels%7Cclaims&languages=en&format=json")).await?;
            let entity = page.pointer(&format!("/entities/{source_id}"));
            let performers = entity
                .and_then(|v| v.pointer("/claims/P175"))
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("performer missing"))?;
            let ids: Vec<&str> = performers
                .iter()
                .filter_map(|v| {
                    v.pointer("/mainsnak/datavalue/value/id")
                        .and_then(Value::as_str)
                })
                .filter(|id| valid_qid(id))
                .take(20)
                .collect();
            if ids.is_empty() {
                return Err(invalid("performer missing"));
            }
            let linked = fetch(&client, format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=labels&languages=en&format=json", ids.join("%7C"))).await?;
            check_wikidata(&page, &linked, source_id, song, artist, evidence, claim)?;
        }
        _ => unreachable!(),
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn source_fetch_retries_503() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let calls = observed.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "{}")
                    } else {
                        (axum::http::StatusCode::OK, r#"{"ok":true}"#)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = fetch(&reqwest::Client::new(), url).await.unwrap();

        server.abort();
        assert_eq!(result["ok"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    const WIKIPEDIA: &str = r#"{"query":{"pages":{"42":{"title":"My Song","extract":"My Song was recorded by An Artist in 1998."}}}}"#;
    const MUSICBRAINZ: &str = r#"{"id":"12345678-1234-1234-1234-123456789abc","title":"My Song","artist-credit":[{"name":"An Artist","artist":{"id":"87654321-4321-4321-4321-cba987654321","name":"An Artist"}}],"first-release-date":"1998-03-04"}"#;
    const WIKIDATA: &str = r#"{"entities":{"Q12":{"id":"Q12","labels":{"en":{"value":"My Song"}},"claims":{"P175":[{"mainsnak":{"datavalue":{"value":{"id":"Q34"}}}}]}}}}"#;
    const ARTIST: &str =
        r#"{"entities":{"Q34":{"id":"Q34","labels":{"en":{"value":"An Artist"}}}}}"#;

    #[test]
    fn wikipedia_requires_exact_passage_and_claim() {
        let page: Value = serde_json::from_str(WIKIPEDIA).unwrap();
        let fact = "My Song was recorded by An Artist in 1998.";
        assert!(check_wikipedia(&page, "My Song", "My Song", "An Artist", fact, fact).is_ok());
        assert!(
            check_wikipedia(
                &page,
                "My Song",
                "My Song",
                "An Artist",
                fact,
                "released in 2001"
            )
            .is_err()
        );
        assert!(check_wikipedia(&page, "My Song", "Other Song", "An Artist", fact, fact).is_err());
        assert!(check_wikipedia(&page, "My Song", "My Song", "Other Artist", fact, fact).is_err());
        assert!(
            check_wikipedia(
                &page,
                "My Song",
                "My Song",
                "An Artist",
                "My Song was not released in 1990 by An Artist.",
                "released in 1990 by An Artist."
            )
            .is_err()
        );
        let numbered: Value = serde_json::from_str(r#"{"query":{"pages":{"42":{"title":"My Song 2","extract":"My Song 2 was recorded by An Artist."}}}}"#).unwrap();
        let unrelated = "My Song 2 was recorded by An Artist.";
        assert!(
            check_wikipedia(
                &numbered,
                "My Song 2",
                "My Song",
                "An Artist",
                unrelated,
                unrelated
            )
            .is_err()
        );
    }

    #[test]
    fn musicbrainz_accepts_only_structured_values() {
        let page: Value = serde_json::from_str(MUSICBRAINZ).unwrap();
        assert!(
            check_musicbrainz(
                &page,
                "12345678-1234-1234-1234-123456789abc",
                "My Song",
                "An Artist",
                "My Song is credited to An Artist.",
                "My Song is credited to An Artist."
            )
            .is_ok()
        );
        assert!(
            check_musicbrainz(
                &page,
                "12345678-1234-1234-1234-123456789abc",
                "My Song",
                "An Artist",
                "My Song was released on 1998-03-04.",
                "My Song was released on 1998-03-04."
            )
            .is_ok()
        );
        assert!(
            check_musicbrainz(
                &page,
                "12345678-1234-1234-1234-123456789abc",
                "My Song",
                "An Artist",
                "My Song was released on 2001-01-01.",
                "My Song was released on 2001-01-01."
            )
            .is_err()
        );
    }

    #[test]
    fn musicbrainz_accepts_verified_artist_alias() {
        let page: Value = serde_json::from_str(r#"{"id":"12345678-1234-1234-1234-123456789abc","title":"My Song","artist-credit":[{"artist":{"name":"別名","sort-name":"An Artist"}}]}"#).unwrap();
        let claim = "My Song is credited to An Artist.";
        assert!(
            check_musicbrainz(
                &page,
                "12345678-1234-1234-1234-123456789abc",
                "My Song",
                "An Artist",
                claim,
                claim
            )
            .is_ok()
        );
        assert!(
            check_musicbrainz(
                &page,
                "12345678-1234-1234-1234-123456789abc",
                "My Song",
                "Other Artist",
                claim,
                claim
            )
            .is_err()
        );
    }

    #[test]
    fn wikidata_performer_must_match_linked_entity() {
        let page: Value = serde_json::from_str(WIKIDATA).unwrap();
        let artist: Value = serde_json::from_str(ARTIST).unwrap();
        assert!(
            check_wikidata(
                &page,
                &artist,
                "Q12",
                "My Song",
                "An Artist",
                "My Song is performed by An Artist.",
                "My Song is performed by An Artist."
            )
            .is_ok()
        );
        assert!(
            check_wikidata(
                &page,
                &artist,
                "Q12",
                "My Song",
                "Other Artist",
                "My Song is performed by Other Artist.",
                "My Song is performed by Other Artist."
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_source_identifiers_are_rejected() {
        assert_eq!(
            canonical("wikipedia", "Song (Artist song)"),
            Some("https://en.wikipedia.org/wiki/Song_%28Artist_song%29".into())
        );
        assert!(canonical("wikipedia", "https://evil.test/").is_none());
        assert!(canonical("wikipedia", "Song#redirect").is_none());
        assert!(canonical("wikidata", "Q12/evil").is_none());
        assert!(canonical("musicbrainz", "not-a-uuid").is_none());
    }

    #[test]
    fn redirected_wikipedia_page_is_rejected() {
        let page: Value = serde_json::from_str(r#"{"query":{"redirects":[{"from":"My Song","to":"Other Song"}],"pages":{"42":{"title":"My Song","extract":"My Song was recorded by An Artist in 1998."}}}}"#).unwrap();
        let fact = "My Song was recorded by An Artist in 1998.";
        assert!(check_wikipedia(&page, "My Song", "My Song", "An Artist", fact, fact).is_err());
    }
}
