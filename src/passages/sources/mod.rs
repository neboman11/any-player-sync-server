//! Ingest sources. Each fetches song/album/artist documents for one track from fixed hosts.

pub mod clean;
pub mod genius;
pub mod http;
pub mod lastfm;
pub mod musicbrainz;
pub mod songfacts;
pub mod wikidata;
pub mod wikipedia;

use crate::passages::NewDocument;
use crate::passages::jobs::TrackRequest;

#[derive(Debug)]
pub enum SourceError {
    /// Worth retrying: network failure, 429, 5xx.
    Transient(String),
    /// Not worth retrying soon: 404, disallowed host, malformed response.
    #[allow(dead_code)] // message kept for Debug; worker::outcome_for discards it (see there)
    Permanent(String),
}

/// Which subjects to fetch; artist/album documents fetched in the last 30 days are skipped.
#[derive(Clone, Copy)]
pub struct Scope {
    pub song: bool,
    pub album: bool,
    pub artist: bool,
}

pub type SourceResult = Result<Vec<NewDocument>, SourceError>;

pub struct Sources {
    pub wikipedia: wikipedia::Wikipedia,
    pub musicbrainz: musicbrainz::MusicBrainz,
    pub wikidata: wikidata::Wikidata,
    pub songfacts: songfacts::Songfacts,
    pub lastfm: Option<lastfm::LastFm>,
    pub genius: Option<genius::Genius>,
}

impl Sources {
    pub fn from_config(lastfm_key: Option<String>, genius_token: Option<String>) -> Self {
        if lastfm_key.is_none() {
            tracing::warn!("LASTFM_API_KEY not set; Last.fm DJ source disabled");
        }
        if genius_token.is_none() {
            tracing::warn!("GENIUS_TOKEN not set; Genius DJ source disabled");
        }
        Self {
            wikipedia: wikipedia::Wikipedia::new(),
            musicbrainz: musicbrainz::MusicBrainz::new(),
            wikidata: wikidata::Wikidata::new(),
            songfacts: songfacts::Songfacts::new(),
            lastfm: lastfm_key.map(lastfm::LastFm::new),
            genius: genius_token.map(genius::Genius::new),
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names = vec!["wikipedia", "musicbrainz", "wikidata", "songfacts"];
        if self.lastfm.is_some() {
            names.push("lastfm");
        }
        if self.genius.is_some() {
            names.push("genius");
        }
        names
    }

    pub async fn fetch(&self, name: &str, track: &TrackRequest, scope: Scope) -> SourceResult {
        match name {
            "wikipedia" => self.wikipedia.fetch(track, scope).await,
            "musicbrainz" => self.musicbrainz.fetch(track, scope).await,
            "wikidata" => self.wikidata.fetch(track, scope).await,
            "songfacts" => self.songfacts.fetch(track, scope).await,
            "lastfm" => match &self.lastfm {
                Some(source) => source.fetch(track, scope).await,
                None => Ok(vec![]),
            },
            "genius" => match &self.genius {
                Some(source) => source.fetch(track, scope).await,
                None => Ok(vec![]),
            },
            other => Err(SourceError::Permanent(format!("unknown source {other}"))),
        }
    }
}
