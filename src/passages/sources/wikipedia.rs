//! Wikipedia (en, ja, ko): full plain-text articles for the song, its album, and each artist.

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use super::{
    Scope, SourceError, SourceResult,
    clean::*,
    http::{SourceHttp, TEXT_CAP},
};
use crate::match_keys::artist_parts;
use crate::passages::{MAX_BODY_BYTES, NewDocument, Subject, jobs::TrackRequest};

const LANGS: &[(&str, &[&str])] = &[
    ("en", &["en.wikipedia.org"]),
    ("ja", &["ja.wikipedia.org"]),
    ("ko", &["ko.wikipedia.org"]),
];
const ARTIST_WORDS: &[&str] = &[
    "band",
    "singer",
    "musician",
    "rapper",
    "group",
    "duo",
    "songwriter",
    "producer",
    "dj",
    "歌手",
    "バンド",
    "ミュージシャン",
    "グループ",
    "ユニット",
    "가수",
    "밴드",
    "그룹",
    "음악가",
];

pub struct Wikipedia {
    langs: Vec<(&'static str, SourceHttp)>,
}

impl Wikipedia {
    pub fn new() -> Self {
        Self {
            langs: LANGS
                .iter()
                .map(|(lang, hosts)| {
                    (
                        *lang,
                        SourceHttp::new(hosts, Duration::from_secs(1), TEXT_CAP),
                    )
                })
                .collect(),
        }
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        let parts = artist_parts(&track.artist);
        let individuals: Vec<String> = if parts.len() > 1 {
            parts[1..].to_vec()
        } else {
            parts.clone()
        };
        let mut docs = Vec::new();
        for (lang, http) in &self.langs {
            let names_artist = |text: &str| named_artist(text, &track.artist).is_some();
            if scope.song
                && let Some(d) = find(
                    http,
                    lang,
                    &format!("{} {}", track.song, track.artist),
                    &track.song,
                    Subject::Song,
                    track,
                    names_artist,
                )
                .await?
            {
                docs.push(d);
            }
            if scope.album
                && let Some(album) = &track.album
                && let Some(d) = find(
                    http,
                    lang,
                    &format!("{album} {}", track.artist),
                    album,
                    Subject::Album,
                    track,
                    names_artist,
                )
                .await?
            {
                docs.push(d);
            }
            if scope.artist {
                for artist in &individuals {
                    let one = TrackRequest {
                        artist: artist.clone(),
                        ..track.clone()
                    };
                    if let Some(d) = find(
                        http,
                        lang,
                        artist,
                        artist,
                        Subject::Artist,
                        &one,
                        is_artist_article,
                    )
                    .await?
                    {
                        docs.push(d);
                    }
                }
            }
        }
        Ok(docs)
    }
}

/// Searches, then fetches the extract only for hits whose title names `wanted` (no wasted
/// page fetches), and accepts the resolved page if its title still matches and `text_ok`.
async fn find(
    http: &SourceHttp,
    lang: &str,
    query: &str,
    wanted: &str,
    subject: Subject,
    track: &TrackRequest,
    text_ok: impl Fn(&str) -> bool,
) -> Result<Option<NewDocument>, SourceError> {
    let api = format!("https://{lang}.wikipedia.org/w/api.php");
    let q = utf8_percent_encode(query, NON_ALPHANUMERIC);
    let search = http
        .get_json(
            &format!("{api}?action=query&list=search&srlimit=5&format=json&srsearch={q}"),
            &[],
        )
        .await?;
    for title in search_titles(&search)
        .into_iter()
        .filter(|t| title_matches(t, wanted))
    {
        let t = utf8_percent_encode(&title, NON_ALPHANUMERIC);
        let page = http
            .get_json(
                &format!(
                    "{api}?action=query&prop=extracts&explaintext=1&format=json&redirects=1&titles={t}"
                ),
                &[],
            )
            .await?;
        let Some((resolved, extract)) = parse_extract(&page) else {
            continue;
        };
        if !title_matches(&resolved, wanted) || !text_ok(&extract) {
            continue;
        }
        let artist = named_artist(&extract, &track.artist).unwrap_or_else(|| track.artist.clone());
        return Ok(Some(NewDocument {
            source: format!("wikipedia-{lang}"),
            source_url: format!(
                "https://{lang}.wikipedia.org/wiki/{}",
                resolved.replace(' ', "_")
            ),
            source_ref: resolved.clone(),
            subject,
            song: (subject == Subject::Song).then(|| track.song.clone()),
            album: track.album.clone(),
            artist,
            title: resolved,
            lang: lang.to_string(),
            body: truncate_body(&strip_wiki_sections(&extract), MAX_BODY_BYTES),
        }));
    }
    Ok(None)
}

pub fn search_titles(v: &Value) -> Vec<String> {
    v.pointer("/query/search")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| h.get("title").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// (resolved title, plain text) of the single page in an extracts response.
pub fn parse_extract(v: &Value) -> Option<(String, String)> {
    let page = v.pointer("/query/pages")?.as_object()?.values().next()?;
    if page.get("missing").is_some() {
        return None;
    }
    Some((
        page.get("title")?.as_str()?.to_string(),
        page.get("extract")?.as_str()?.to_string(),
    ))
}

/// The most specific credited artist the text names (individual artists before the full credit).
fn named_artist(text: &str, credit: &str) -> Option<String> {
    let t = key(text);
    artist_parts(credit)
        .into_iter()
        .rev()
        .find(|a| t.contains(&key(a)))
}

fn is_artist_article(text: &str) -> bool {
    let lead = key(&text.chars().take(600).collect::<String>());
    ARTIST_WORDS.iter().any(|w| lead.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(format!("tests/fixtures/passages/{name}")).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn parses_fixtures_into_a_full_cleaned_article() {
        assert!(
            search_titles(&fixture("wikipedia-search.json"))
                .iter()
                .any(|t| title_matches(t, "Hotel California"))
        );
        let (title, text) = parse_extract(&fixture("wikipedia-extract.json")).unwrap();
        assert!(title_matches(&title, "Hotel California"));
        assert_eq!(named_artist(&text, "Eagles").as_deref(), Some("Eagles"));
        let cleaned = strip_wiki_sections(&text);
        assert!(!cleaned.contains("== References =="));
        assert!(
            cleaned.len() > 2000,
            "expected the full article, not a snippet"
        );
    }

    #[test]
    fn artist_helpers() {
        assert_eq!(
            named_artist(
                "A song by Megan Thee Stallion.",
                "Reneé Rapp, Megan Thee Stallion"
            )
            .as_deref(),
            Some("Megan Thee Stallion")
        );
        assert!(is_artist_article(
            "Eagles are an American rock band formed in 1971."
        ));
        assert!(!is_artist_article("Hotel California is a luxury hotel."));
    }
}
