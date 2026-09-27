//! Songfacts: search, match "Title - Artist", then the song's facts list
//! (`ul.songfacts-results > li > div.inner`). Visitor comments are a separate list and excluded.

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use scraper::{Html, Selector};

use super::{
    Scope, SourceResult,
    clean::*,
    http::{SourceHttp, TEXT_CAP},
};
use crate::match_keys::{artist_parts, base_title};
use crate::passages::{MAX_BODY_BYTES, NewDocument, Subject, jobs::TrackRequest};

pub fn search_url(song: &str) -> String {
    let lower = song.to_lowercase();
    let q = utf8_percent_encode(&lower, NON_ALPHANUMERIC);
    format!("https://www.songfacts.com/search/songs/{q}")
}

pub struct Songfacts {
    http: SourceHttp,
}

impl Songfacts {
    pub fn new() -> Self {
        Self {
            http: SourceHttp::new(&["www.songfacts.com"], Duration::from_secs(5), TEXT_CAP),
        }
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        if !scope.song {
            return Ok(vec![]);
        }
        let song = base_title(&track.song);
        let search = self.http.get_text(&search_url(song)).await?;
        let Some((path, artist)) = find_result(&search, song, &track.artist) else {
            return Ok(vec![]);
        };
        let url = format!("https://www.songfacts.com{path}");
        let body = facts(&self.http.get_text(&url).await?);
        if body.is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![NewDocument {
            source: "songfacts".into(),
            source_ref: path,
            source_url: url,
            subject: Subject::Song,
            song: Some(track.song.clone()),
            album: track.album.clone(),
            artist,
            title: song.to_string(),
            lang: "en".into(),
            body: truncate_body(&body, MAX_BODY_BYTES),
        }])
    }
}

/// Search results render as `<a href="/facts/{artist}/{song}">Title</a> - Artist`.
pub fn find_result(html: &str, song: &str, credit: &str) -> Option<(String, String)> {
    let doc = Html::parse_document(html);
    let links = Selector::parse(r#"a[href^="/facts/"]"#).expect("selector");
    for a in doc.select(&links) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        if href.matches('/').count() != 3 {
            continue;
        }
        let title: String = a.text().collect();
        let listed = a
            .next_sibling()
            .and_then(|n| n.value().as_text().map(|t| t.to_string()))
            .unwrap_or_default();
        let listed = listed.trim().trim_start_matches('-').trim();
        if title_matches(&title, song)
            && let Some(artist) = artist_parts(credit)
                .into_iter()
                .find(|p| key(p) == key(listed))
        {
            return Some((href.to_string(), artist));
        }
    }
    None
}

pub fn facts(html: &str) -> String {
    let doc = Html::parse_document(html);
    let items = Selector::parse("ul.songfacts-results > li > div.inner").expect("selector");
    doc.select(&items)
        .map(|el| {
            el.text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_lowercases_title() {
        let url = search_url("Hotel California");
        assert_eq!(
            url,
            "https://www.songfacts.com/search/songs/hotel%20california"
        );
    }

    #[test]
    fn search_url_handles_unicode() {
        let url = search_url("星に願いを");
        assert!(url.starts_with("https://www.songfacts.com/search/songs/"));
        // Verify query part is percent-encoded (should contain % characters)
        let query = url
            .strip_prefix("https://www.songfacts.com/search/songs/")
            .unwrap();
        assert!(
            query.contains('%'),
            "Expected percent-encoded output but got: {}",
            query
        );
    }

    #[test]
    fn finds_result_and_extracts_facts_without_comments() {
        let search =
            std::fs::read_to_string("tests/fixtures/passages/songfacts-search.html").unwrap();
        assert_eq!(
            find_result(&search, "Hotel California", "Eagles"),
            Some((
                "/facts/eagles/hotel-california".to_string(),
                "Eagles".to_string()
            ))
        );
        let text =
            facts(&std::fs::read_to_string("tests/fixtures/passages/songfacts-page.html").unwrap());
        assert!(text.contains("Don Felder"));
        assert!(text.split("\n\n").count() >= 5);
    }

    /// Synthetic HTML (not a capture) exercising the "skip results that don't match the wanted
    /// title/artist" path, which the real single-result capture above can't exercise.
    #[test]
    fn skips_non_matching_results_before_the_real_one() {
        let synthetic = r#"<ul class="browse-list-orange space-bot">
            <li><a href="/facts/meytal/hotel-california-eagles-cover" target="_self">Hotel California (Eagles Cover)</a> - Meytal</li>
            <li><a href="/facts/eagles/hotel-california" target="_self">Hotel California</a> - Eagles</li>
        </ul>"#;
        assert_eq!(
            find_result(synthetic, "Hotel California", "Eagles"),
            Some((
                "/facts/eagles/hotel-california".to_string(),
                "Eagles".to_string()
            ))
        );
    }
}
