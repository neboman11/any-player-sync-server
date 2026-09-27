//! Genius song description and annotation bodies (text_format=plain). The referent
//! `fragment` (the annotated lyric) is never read into a document.

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use super::{
    Scope, SourceError, SourceResult,
    clean::*,
    http::{JSON_CAP, SourceHttp},
};
use crate::match_keys::{artist_parts, base_title};
use crate::passages::{MAX_BODY_BYTES, NewDocument, Subject, jobs::TrackRequest};

pub struct Genius {
    http: SourceHttp,
    auth: String,
}

pub struct Hit {
    pub song_id: i64,
    pub artist_id: i64,
    pub artist: String,
    pub url: String,
}

impl Genius {
    pub fn new(token: String) -> Self {
        Self {
            http: SourceHttp::new(&["api.genius.com"], Duration::from_secs(1), JSON_CAP),
            auth: format!("Bearer {token}"),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, SourceError> {
        self.http
            .get_json(
                &format!("https://api.genius.com{path}"),
                &[("Authorization", &self.auth)],
            )
            .await
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        if !scope.song && !scope.artist {
            return Ok(vec![]);
        }
        let song = base_title(&track.song);
        let query = format!("{song} {}", track.artist);
        let q = utf8_percent_encode(&query, NON_ALPHANUMERIC);
        let Some(hit) = pick_hit(
            &self.get(&format!("/search?q={q}")).await?,
            song,
            &track.artist,
        ) else {
            return Ok(vec![]);
        };
        let mut docs = Vec::new();
        if scope.song {
            let song_v = self
                .get(&format!("/songs/{}?text_format=plain", hit.song_id))
                .await?;
            let refs = self
                .get(&format!(
                    "/referents?song_id={}&text_format=plain&per_page=50",
                    hit.song_id
                ))
                .await?;
            let body = song_body(&song_v, &refs);
            if !body.is_empty() {
                docs.push(NewDocument {
                    source: "genius".into(),
                    source_ref: format!("song:{}", hit.song_id),
                    source_url: hit.url.clone(),
                    subject: Subject::Song,
                    song: Some(track.song.clone()),
                    album: track.album.clone(),
                    artist: hit.artist.clone(),
                    title: song.to_string(),
                    lang: "en".into(),
                    body: truncate_body(&body, MAX_BODY_BYTES),
                });
            }
        }
        if scope.artist {
            let a = self
                .get(&format!("/artists/{}?text_format=plain", hit.artist_id))
                .await?;
            if let Some(body) = description(&a["response"]["artist"]) {
                docs.push(NewDocument {
                    source: "genius".into(),
                    source_ref: format!("artist:{}", hit.artist_id),
                    source_url: a
                        .pointer("/response/artist/url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    subject: Subject::Artist,
                    song: None,
                    album: None,
                    artist: hit.artist.clone(),
                    title: hit.artist.clone(),
                    lang: "en".into(),
                    body: truncate_body(&body, MAX_BODY_BYTES),
                });
            }
        }
        Ok(docs)
    }
}

pub fn pick_hit(search: &Value, song: &str, credit: &str) -> Option<Hit> {
    search
        .pointer("/response/hits")?
        .as_array()?
        .iter()
        .find_map(|h| {
            let r = &h["result"];
            let primary = r.pointer("/primary_artist/name")?.as_str()?;
            let artist = artist_parts(credit)
                .into_iter()
                .find(|a| key(a) == key(primary))?;
            title_matches(r["title"].as_str()?, song).then(|| Hit {
                song_id: r["id"].as_i64().unwrap_or_default(),
                artist_id: r
                    .pointer("/primary_artist/id")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                artist,
                url: r["url"].as_str().unwrap_or_default().to_string(),
            })
        })
}

fn description(v: &Value) -> Option<String> {
    v.pointer("/description/plain")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "?")
        .map(str::to_string)
}

/// Drops any paragraph (split on blank lines) that quotes a referent fragment (lyrics)
/// verbatim, keeping the rest. Returns `None` if nothing is left.
fn strip_lyric_paragraphs(text: &str, fragments: &[&str]) -> Option<String> {
    let kept: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty() && !fragments.iter().any(|f| p.contains(f)))
        .collect();
    (!kept.is_empty()).then(|| kept.join("\n\n"))
}

/// Song description plus every annotation body. Referent fragments (lyrics) are not read, and a
/// description paragraph or annotation that quotes a fragment verbatim (annotators sometimes
/// cite the line they're explaining, or another line elsewhere in the song, and "About"
/// descriptions sometimes open with a quoted lyric) is dropped rather than stored.
pub fn song_body(song: &Value, referents: &Value) -> String {
    let refs: Vec<&Value> = referents
        .pointer("/response/referents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let fragments: Vec<&str> = refs
        .iter()
        .filter_map(|r| r["fragment"].as_str())
        .filter(|f| !f.is_empty())
        .collect();
    let mut parts: Vec<String> = description(&song["response"]["song"])
        .and_then(|d| strip_lyric_paragraphs(&d, &fragments))
        .into_iter()
        .collect();
    for r in &refs {
        for a in r["annotations"].as_array().into_iter().flatten() {
            if let Some(text) = a
                .pointer("/body/plain")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                && !fragments.iter().any(|f| text.contains(f))
            {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(n: &str) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(format!("tests/fixtures/passages/{n}")).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn keeps_descriptions_and_annotations_but_never_lyrics() {
        assert_eq!(
            pick_hit(&fixture("genius-search.json"), "Hotel California", "Eagles")
                .unwrap()
                .artist,
            "Eagles"
        );
        let refs = fixture("genius-referents.json");
        let body = song_body(&fixture("genius-song.json"), &refs);
        assert!(!body.is_empty());
        for r in refs
            .pointer("/response/referents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(fragment) = r["fragment"].as_str().filter(|f| f.len() > 12) {
                assert!(
                    !body.contains(fragment),
                    "lyric fragment leaked: {fragment}"
                );
            }
        }
    }

    #[test]
    fn description_paragraphs_quoting_a_fragment_are_dropped_but_others_kept() {
        let song = serde_json::json!({"response":{"song":{"description":{"plain":
            "Intro paragraph, clean.\n\nOn a dark desert highway, cool wind in my hair.\n\nAnother clean paragraph."
        }}}});
        let referents = serde_json::json!({"response":{"referents":[
            {"fragment":"On a dark desert highway, cool wind in my hair","annotations":[]}
        ]}});
        let body = song_body(&song, &referents);
        assert!(body.contains("Intro paragraph, clean."));
        assert!(body.contains("Another clean paragraph."));
        assert!(!body.contains("On a dark desert highway"));
    }
}
