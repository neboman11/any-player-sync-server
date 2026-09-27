//! Pure passage ranking for one lookup: exclusion window, scoring, variety, token budget.

use chrono::{DateTime, Utc};

use crate::match_keys::match_key;
use crate::passages::{Subject, embed::cosine};

pub const BUDGET_TOKENS: usize = 2500;
const EXCLUSION_DAYS: f64 = 30.0;
const NEAR_DUPLICATE: f32 = 0.92;
const PER_DOCUMENT: usize = 2;
const TITLE_BOOST: f32 = 0.05;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub chunk_id: i64,
    pub document_id: i64,
    pub text: String,
    pub token_count: usize,
    pub embedding: Vec<f32>,
    pub source: String,
    pub source_url: String,
    pub title: String,
    pub subject: Subject,
    pub lang: String,
    pub last_played: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq)]
pub enum Selection {
    Chosen(Vec<i64>),
    AllExcluded,
    Empty,
}

fn weight(subject: Subject) -> f32 {
    match subject {
        Subject::Song => 1.0,
        Subject::Album => 0.85,
        Subject::Artist => 0.7,
    }
}

/// None while inside the exclusion window; 1.0 when never played.
fn recovery(last_played: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<f32> {
    let Some(at) = last_played else {
        return Some(1.0);
    };
    let days = (now - at).num_seconds() as f64 / 86_400.0;
    if days < EXCLUSION_DAYS {
        return None;
    }
    Some(((days - EXCLUSION_DAYS).ln_1p() / 366f64.ln()).min(1.0) as f32)
}

pub fn select(
    candidates: &[Candidate],
    query: &[f32],
    song_key: &str,
    now: DateTime<Utc>,
    budget: usize,
) -> Selection {
    if candidates.is_empty() {
        return Selection::Empty;
    }
    let mut scored: Vec<(bool, f32, &Candidate)> = candidates
        .iter()
        .filter_map(|c| {
            let recovery = recovery(c.last_played, now)?;
            let boost = if !song_key.is_empty() && match_key(&c.text).contains(song_key) {
                TITLE_BOOST
            } else {
                0.0
            };
            let score = (cosine(query, &c.embedding) + boost) * weight(c.subject) * recovery;
            Some((c.last_played.is_none(), score, c))
        })
        .collect();
    if scored.is_empty() {
        return Selection::AllExcluded;
    }
    // Never-played first, then score, then chunk id so ties are stable.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.chunk_id.cmp(&b.2.chunk_id))
    });
    let mut chosen: Vec<&Candidate> = Vec::new();
    let mut used = 0;
    for (_, _, c) in scored {
        if chosen
            .iter()
            .filter(|x| x.document_id == c.document_id)
            .count()
            >= PER_DOCUMENT
        {
            continue;
        }
        if chosen
            .iter()
            .any(|x| cosine(&x.embedding, &c.embedding) > NEAR_DUPLICATE)
        {
            continue;
        }
        if !chosen.is_empty() && used + c.token_count > budget {
            break;
        }
        used += c.token_count;
        chosen.push(c);
    }
    Selection::Chosen(chosen.iter().map(|c| c.chunk_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn v(x: &[f32]) -> Vec<f32> {
        let mut out = x.to_vec();
        out.resize(4, 0.0);
        out
    }

    fn cand(id: i64, doc: i64, subject: Subject, embedding: Vec<f32>, tokens: usize) -> Candidate {
        Candidate {
            chunk_id: id,
            document_id: doc,
            text: format!("chunk {id}"),
            token_count: tokens,
            embedding,
            source: "wikipedia-en".into(),
            source_url: String::new(),
            title: String::new(),
            subject,
            lang: "en".into(),
            last_played: None,
        }
    }

    #[test]
    fn prefers_song_over_artist_and_caps_two_per_document() {
        let q = v(&[1.0]);
        let cs = vec![
            cand(1, 10, Subject::Artist, v(&[0.7, 0.0, 0.0, 0.714]), 10), // 0.7 * 0.7 = 0.49
            cand(2, 20, Subject::Song, v(&[1.0]), 10),                    // 1.0
            cand(3, 20, Subject::Song, v(&[0.9, 0.436]), 10),             // 0.9, cosine to #2 = 0.9
            cand(4, 20, Subject::Song, v(&[0.8, 0.0, 0.6]), 10),          // 0.8, third from doc 20
        ];
        assert_eq!(
            select(&cs, &q, "x", Utc::now(), 2500),
            Selection::Chosen(vec![2, 3, 1])
        );
    }

    #[test]
    fn skips_near_duplicates() {
        let cs = vec![
            cand(1, 10, Subject::Song, v(&[1.0]), 10),
            cand(2, 20, Subject::Song, v(&[0.95, 0.312]), 10), // cosine to #1 = 0.95 > 0.92
        ];
        assert_eq!(
            select(&cs, &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::Chosen(vec![1])
        );
    }

    #[test]
    fn excludes_recent_plays_and_reports_all_excluded() {
        let mut c = cand(1, 10, Subject::Song, v(&[1.0]), 10);
        c.last_played = Some(Utc::now() - Duration::days(29));
        assert_eq!(
            select(&[c.clone()], &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::AllExcluded
        );
        c.last_played = Some(Utc::now() - Duration::days(31));
        assert_eq!(
            select(&[c], &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::Chosen(vec![1])
        );
        assert_eq!(
            select(&[], &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::Empty
        );
    }

    #[test]
    fn never_played_outranks_recovering_and_song_title_boosts() {
        let now = Utc::now();
        let mut old = cand(1, 10, Subject::Song, v(&[1.0]), 10);
        old.last_played = Some(now - Duration::days(40));
        let fresh = cand(2, 20, Subject::Song, v(&[0.6, 0.8]), 10);
        assert_eq!(
            select(&[old, fresh], &v(&[1.0]), "x", now, 2500),
            Selection::Chosen(vec![2, 1])
        );

        let mut named = cand(3, 30, Subject::Song, v(&[0.9, 0.0, 0.436]), 10); // 0.9 + 0.05
        named.text = "Hotel California was recorded in 1976".into();
        let plain = cand(4, 40, Subject::Song, v(&[0.93, 0.0, 0.0, 0.367]), 10); // 0.93
        assert_eq!(
            select(&[plain, named], &v(&[1.0]), "hotel california", now, 2500),
            Selection::Chosen(vec![3, 4])
        );
    }

    #[test]
    fn stops_at_budget_but_returns_at_least_one() {
        let cs = vec![
            cand(1, 10, Subject::Song, v(&[1.0]), 3000),
            cand(2, 20, Subject::Song, v(&[0.0, 1.0]), 10),
        ];
        assert_eq!(
            select(&cs, &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::Chosen(vec![1])
        );
        let cs = vec![
            cand(1, 10, Subject::Song, v(&[1.0]), 2000),
            cand(2, 20, Subject::Song, v(&[0.5, 0.866]), 600),
            cand(3, 30, Subject::Song, v(&[0.4, 0.0, 0.917]), 400),
        ];
        assert_eq!(
            select(&cs, &v(&[1.0]), "x", Utc::now(), 2500),
            Selection::Chosen(vec![1])
        );
    }
}
