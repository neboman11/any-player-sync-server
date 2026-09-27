//! Last.fm wiki text for track, album and artist.

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

pub struct LastFm {
    http: SourceHttp,
    api_key: String,
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

impl LastFm {
    pub fn new(api_key: String) -> Self {
        Self {
            http: SourceHttp::new(&["ws.audioscrobbler.com"], Duration::from_secs(1), JSON_CAP),
            api_key,
        }
    }

    async fn call(&self, method: &str, params: &str) -> Result<Value, SourceError> {
        let url = format!(
            "https://ws.audioscrobbler.com/2.0/?method={method}&format=json&autocorrect=1&api_key={}&{params}",
            self.api_key
        );
        self.http.get_json(&url, &[]).await
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        let mut docs = Vec::new();
        for artist in artist_parts(&track.artist) {
            if scope.song {
                let v = self
                    .call(
                        "track.getInfo",
                        &format!(
                            "artist={}&track={}",
                            enc(&artist),
                            enc(base_title(&track.song))
                        ),
                    )
                    .await?;
                docs.extend(parse(&v, Subject::Song, track, &artist));
            }
            if scope.album
                && let Some(album) = &track.album
            {
                let v = self
                    .call(
                        "album.getInfo",
                        &format!("artist={}&album={}", enc(&artist), enc(album)),
                    )
                    .await?;
                docs.extend(parse(&v, Subject::Album, track, &artist));
            }
            if scope.artist {
                let v = self
                    .call("artist.getInfo", &format!("artist={}", enc(&artist)))
                    .await?;
                docs.extend(parse(&v, Subject::Artist, track, &artist));
            }
            if !docs.is_empty() {
                break;
            }
        }
        Ok(docs)
    }
}

/// Validates identity (autocorrect may return a different item) and extracts the wiki text.
pub fn parse(
    v: &Value,
    subject: Subject,
    track: &TrackRequest,
    artist: &str,
) -> Option<NewDocument> {
    let (root, content) = match subject {
        Subject::Song => ("/track", "/track/wiki/content"),
        Subject::Album => ("/album", "/album/wiki/content"),
        Subject::Artist => ("/artist", "/artist/bio/content"),
    };
    let name = v.pointer(&format!("{root}/name"))?.as_str()?;
    let identity_ok = match subject {
        Subject::Song => {
            title_matches(name, &track.song)
                && v.pointer("/track/artist/name")
                    .and_then(Value::as_str)
                    .is_some_and(|a| key(a) == key(artist))
        }
        Subject::Album => {
            title_matches(name, track.album.as_deref()?)
                && v.pointer("/album/artist")
                    .and_then(Value::as_str)
                    .is_some_and(|a| key(a) == key(artist))
        }
        Subject::Artist => key(name) == key(artist),
    };
    if !identity_ok {
        return None;
    }
    let body = strip_lastfm_suffix(v.pointer(content)?.as_str()?);
    if body.is_empty() {
        return None;
    }
    let url = v
        .pointer(&format!("{root}/url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(NewDocument {
        source: "lastfm".into(),
        source_ref: format!("{}:{url}", subject.as_str()),
        source_url: url,
        subject,
        song: (subject == Subject::Song).then(|| track.song.clone()),
        album: track.album.clone(),
        artist: artist.to_string(),
        title: name.to_string(),
        lang: "en".into(),
        body: truncate_body(&body, MAX_BODY_BYTES),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_track_wiki_strips_suffix_and_rejects_empty_or_wrong_identity() {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string("tests/fixtures/passages/lastfm-track.json").unwrap(),
        )
        .unwrap();
        let t = TrackRequest {
            song: "Hotel California".into(),
            artist: "Eagles".into(),
            album: None,
        };
        let d = parse(&v, Subject::Song, &t, "Eagles").unwrap();
        assert!(!d.body.contains("Read more on Last.fm") && !d.body.is_empty());
        let empty = serde_json::json!({"track":{"name":"Hotel California","artist":{"name":"Eagles"},"wiki":{"content":" "}}});
        assert!(parse(&empty, Subject::Song, &t, "Eagles").is_none());
        assert!(parse(&v, Subject::Song, &t, "Someone Else").is_none());
    }
}
