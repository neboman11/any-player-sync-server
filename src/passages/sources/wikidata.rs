//! Wikidata song entity: selected statements rendered as sentences with en/ja/ko labels.

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use super::{
    Scope, SourceError, SourceResult,
    clean::*,
    http::{JSON_CAP, SourceHttp},
};
use crate::match_keys::{artist_parts, base_title};
use crate::passages::{NewDocument, Subject, jobs::TrackRequest};

const PROPS: &[(&str, &str)] = &[
    ("P86", "It was composed by"),
    ("P676", "Its lyrics were written by"),
    ("P162", "It was produced by"),
    ("P264", "It was released on the label"),
    ("P361", "It is part of"),
    ("P136", "Its genre is"),
    ("P1411", "It was nominated for"),
    ("P166", "It received"),
];

pub struct Wikidata {
    http: SourceHttp,
}

impl Wikidata {
    pub fn new() -> Self {
        Self {
            http: SourceHttp::new(&["www.wikidata.org"], Duration::from_secs(1), JSON_CAP),
        }
    }

    async fn entities(&self, ids: &[String]) -> Result<Value, SourceError> {
        let url = format!(
            "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=labels%7Cclaims&languages=en%7Cja%7Cko&format=json",
            ids.join("%7C")
        );
        self.http.get_json(&url, &[]).await
    }

    pub async fn fetch(&self, track: &TrackRequest, scope: Scope) -> SourceResult {
        if !scope.song {
            return Ok(vec![]);
        }
        let song = base_title(&track.song);
        let q = utf8_percent_encode(song, NON_ALPHANUMERIC);
        let search = self
            .http
            .get_json(
                &format!(
                    "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={q}&language=en&type=item&limit=10&format=json"
                ),
                &[],
            )
            .await?;
        let ids: Vec<String> = search["search"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|h| h["label"].as_str().is_some_and(|l| key(l) == key(song)))
            .filter_map(|h| h["id"].as_str().map(str::to_string))
            .take(5)
            .collect();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let entities = self.entities(&ids).await?;
        for id in &ids {
            let entity = &entities["entities"][id.as_str()];
            let performers = claim_ids(entity, "P175");
            if performers.is_empty() {
                continue;
            }
            let referenced: Vec<String> = PROPS
                .iter()
                .flat_map(|(p, _)| claim_ids(entity, p))
                .chain(performers.clone())
                .take(45)
                .collect();
            let labels = self.entities(&referenced).await?;
            let Some(artist) = artist_parts(&track.artist).into_iter().find(|a| {
                performers
                    .iter()
                    .any(|p| label(&labels, p).is_some_and(|l| key(&l) == key(a)))
            }) else {
                continue;
            };
            return Ok(vec![NewDocument {
                source: "wikidata".into(),
                source_ref: id.clone(),
                source_url: format!("https://www.wikidata.org/wiki/{id}"),
                subject: Subject::Song,
                song: Some(track.song.clone()),
                album: track.album.clone(),
                body: render(entity, &labels, song, &artist),
                artist,
                title: song.to_string(),
                lang: "en".into(),
            }]);
        }
        Ok(vec![])
    }
}

pub fn claim_ids(entity: &Value, prop: &str) -> Vec<String> {
    entity["claims"][prop]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c.pointer("/mainsnak/datavalue/value/id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn label(labels: &Value, id: &str) -> Option<String> {
    let l = &labels["entities"][id]["labels"];
    ["en", "ja", "ko"]
        .iter()
        .find_map(|lang| l[*lang]["value"].as_str().map(str::to_string))
}

pub fn render(entity: &Value, labels: &Value, song: &str, artist: &str) -> String {
    let mut s = format!("{song} is performed by {artist}.");
    if let Some(time) = entity["claims"]["P577"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|c| c.pointer("/mainsnak/datavalue/value/time"))
        .and_then(Value::as_str)
    {
        let date = time
            .trim_start_matches('+')
            .split('T')
            .next()
            .unwrap_or(time);
        s.push_str(&format!(" It was published on {date}."));
    }
    for (prop, phrase) in PROPS {
        let names: Vec<String> = claim_ids(entity, prop)
            .iter()
            .filter_map(|id| label(labels, id))
            .collect();
        if !names.is_empty() {
            s.push_str(&format!(" {phrase} {}.", names.join(", ")));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_statements_with_labels() {
        let entity = serde_json::json!({"claims":{
            "P86":[{"mainsnak":{"datavalue":{"value":{"id":"Q1"}}}}],
            "P577":[{"mainsnak":{"datavalue":{"value":{"time":"+1977-02-22T00:00:00Z"}}}}]}});
        let labels =
            serde_json::json!({"entities":{"Q1":{"labels":{"en":{"value":"Don Felder"}}}}});
        assert_eq!(
            render(&entity, &labels, "Hotel California", "Eagles"),
            "Hotel California is performed by Eagles. It was published on 1977-02-22. It was composed by Don Felder."
        );
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string("tests/fixtures/passages/wikidata-entity.json").unwrap(),
        )
        .unwrap();
        assert!(!claim_ids(&fixture["entities"]["Q780394"], "P175").is_empty());
    }
}
