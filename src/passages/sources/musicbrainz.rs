//! MusicBrainz recording relationships rendered as plain sentences.

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use super::{
    Scope, SourceResult,
    clean::*,
    http::{JSON_CAP, SourceHttp},
};
use crate::match_keys::{artist_parts, base_title};
use crate::passages::{NewDocument, Subject, jobs::TrackRequest};

pub struct MusicBrainz {
    http: SourceHttp,
}

impl MusicBrainz {
    pub fn new() -> Self {
        Self {
            http: SourceHttp::new(&["musicbrainz.org"], Duration::from_secs(1), JSON_CAP),
        }
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        if !scope.song {
            return Ok(vec![]);
        }
        let song = base_title(&track.song);
        for artist in artist_parts(&track.artist) {
            let q = utf8_percent_encode(
                &format!("recording:\"{song}\" AND artist:\"{artist}\""),
                NON_ALPHANUMERIC,
            )
            .to_string();
            let found = self
                .http
                .get_json(
                    &format!("https://musicbrainz.org/ws/2/recording?fmt=json&limit=25&query={q}"),
                    &[],
                )
                .await?;
            for id in candidate_ids(&found, song) {
                let rec = self
                    .http
                    .get_json(
                        &format!(
                            "https://musicbrainz.org/ws/2/recording/{id}?fmt=json&inc=artists+aliases+artist-rels+place-rels+work-rels+recording-rels+release-groups"
                        ),
                        &[],
                    )
                    .await?;
                if !credited(&rec, &artist) {
                    continue;
                }
                let mut body = render(&rec, song, &artist);
                for work in work_ids(&rec) {
                    let w = self
                        .http
                        .get_json(
                            &format!(
                                "https://musicbrainz.org/ws/2/work/{work}?fmt=json&inc=artist-rels"
                            ),
                            &[],
                        )
                        .await?;
                    body.push_str(&render_work(&w, song));
                }
                return Ok(vec![NewDocument {
                    source: "musicbrainz".into(),
                    source_ref: id.clone(),
                    source_url: format!("https://musicbrainz.org/recording/{id}"),
                    subject: Subject::Song,
                    song: Some(track.song.clone()),
                    album: track.album.clone(),
                    artist,
                    title: song.to_string(),
                    lang: "en".into(),
                    body,
                }]);
            }
        }
        Ok(vec![])
    }
}

/// Up to 5 matching recording ids, earliest first release first (usually the original).
pub fn candidate_ids(v: &Value, song: &str) -> Vec<String> {
    let mut recs: Vec<&Value> = v["recordings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["title"].as_str().is_some_and(|t| key(t) == key(song)))
        .collect();
    recs.sort_by_key(|r| {
        r["first-release-date"]
            .as_str()
            .filter(|d| !d.is_empty())
            .unwrap_or("9999")
            .to_string()
    });
    recs.iter()
        .take(5)
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

pub fn credited(rec: &Value, artist: &str) -> bool {
    rec["artist-credit"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| {
            let a = &c["artist"];
            [a["name"].as_str(), a["sort-name"].as_str()]
                .into_iter()
                .flatten()
                .any(|n| key(n) == key(artist))
                || a["aliases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|al| al["name"].as_str().is_some_and(|n| key(n) == key(artist)))
        })
}

fn work_ids(rec: &Value) -> Vec<String> {
    rec["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["type"] == "performance")
        .filter_map(|r| {
            r.pointer("/work/id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .take(2)
        .collect()
}

fn names(relations: &Value, types: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in relations.as_array().into_iter().flatten() {
        if !r["type"].as_str().is_some_and(|t| types.contains(&t)) {
            continue;
        }
        let name = r
            .pointer("/artist/name")
            .or_else(|| r.pointer("/place/name"))
            .or_else(|| r.pointer("/recording/title"));
        if let Some(n) = name.and_then(Value::as_str)
            && !out.iter().any(|o| o == n)
        {
            out.push(n.to_string());
        }
    }
    out
}

pub fn render(rec: &Value, song: &str, artist: &str) -> String {
    let mut s = format!("{song} is a recording credited to {artist}.");
    if let Some(date) = rec["first-release-date"].as_str().filter(|d| !d.is_empty()) {
        s.push_str(&format!(" It was first released on {date}."));
    }
    if let Some(group) = rec["release-groups"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|g| g["title"].as_str())
    {
        s.push_str(&format!(" It appears on {group}."));
    }
    for (types, phrase) in [
        (&["producer"][..], "Produced by"),
        (&["engineer", "recording", "audio"][..], "Engineered by"),
        (&["mix"][..], "Mixed by"),
        (&["recorded at"][..], "Recorded at"),
        (&["samples material"][..], "It samples"),
        (&["instrument", "vocal"][..], "Performers include"),
    ] {
        let n = names(&rec["relations"], types);
        if !n.is_empty() {
            s.push_str(&format!(" {phrase} {}.", n.join(", ")));
        }
    }
    s
}

pub fn render_work(work: &Value, song: &str) -> String {
    let mut s = String::new();
    for (types, phrase) in [
        (&["composer"][..], "composed by"),
        (&["lyricist"][..], "given lyrics by"),
        (&["writer"][..], "written by"),
    ] {
        let n = names(&work["relations"], types);
        if !n.is_empty() {
            s.push_str(&format!(" {song} was {phrase} {}.", n.join(", ")));
        }
    }
    s
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
    fn renders_relationships_from_fixture() {
        let rec = fixture("musicbrainz-recording.json");
        assert_eq!(
            candidate_ids(&fixture("musicbrainz-search.json"), "Hotel California")[0],
            rec["id"].as_str().unwrap(),
            "recording fixture should be the id candidate_ids() ranks first"
        );
        assert!(credited(&rec, "Eagles"));
        let text = render(&rec, "Hotel California", "Eagles");
        assert!(text.starts_with("Hotel California is a recording credited to Eagles."));
        assert!(
            text.contains("It was first released on"),
            "expected a first-release-date sentence, got: {text}"
        );
        let work = serde_json::json!({"relations":[{"type":"composer","artist":{"name":"Don Felder"}},{"type":"composer","artist":{"name":"Don Felder"}}]});
        assert_eq!(
            render_work(&work, "Hotel California"),
            " Hotel California was composed by Don Felder."
        );
    }
}
