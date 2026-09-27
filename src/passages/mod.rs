//! AI DJ passage store: whole source documents, chunked and embedded for retrieval.

pub mod api;
pub mod chunk;
pub mod embed;
pub mod jobs;
pub mod rank;
pub mod schema;
pub mod sources;
pub mod store;
#[cfg(test)]
pub mod testutil;
pub mod worker;

/// Enables DJ passage retrieval and admin ingest; constructed at startup when pgvector and the
/// embedding model are both available.
pub struct Passages {
    pub embedder: std::sync::Arc<dyn embed::Embed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Subject {
    Song,
    Album,
    Artist,
}

impl Subject {
    pub fn as_str(self) -> &'static str {
        match self {
            Subject::Song => "song",
            Subject::Album => "album",
            Subject::Artist => "artist",
        }
    }
}

pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// A document ready to store. `song`/`album` are the track's names (keys are derived with
/// base_title); `artist` is the single credited artist that matched at the source.
#[derive(Clone, Debug)]
pub struct NewDocument {
    pub source: String,
    pub source_ref: String,
    pub source_url: String,
    pub subject: Subject,
    pub song: Option<String>,
    pub album: Option<String>,
    pub artist: String,
    pub title: String,
    pub lang: String,
    pub body: String,
}
