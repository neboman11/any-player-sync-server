//! Normalized song/artist keys shared by the fact store and the passage store. Lookups try
//! every key a player-supplied title or credit can reduce to; stored rows use `match_key`.

use crate::dj_fact_sources::fold_dashes;

pub(crate) fn identity_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Key stored in song_key/artist_key columns and used for lookup: identity_key with dash
/// variants folded.
pub fn match_key(value: &str) -> String {
    fold_dashes(&identity_key(value))
}

/// Words that mark a trailing "(...)"/"[...]" as a release qualifier rather than part of the title.
const TITLE_QUALIFIERS: &[&str] = &[
    "feat",
    "feat.",
    "ft",
    "ft.",
    "featuring",
    "with",
    "from",
    "remaster",
    "remastered",
    "version",
    "live",
    "remix",
    "edit",
    "mix",
    "mono",
    "stereo",
    "acoustic",
    "demo",
    "instrumental",
    "explicit",
    "ver",
    "ver.",
];

/// The title with release qualifiers removed ("American Woman - 2024 Remaster" -> "American
/// Woman", "Song (feat. X)" -> "Song"). Returns the input unchanged when nothing strips.
pub fn base_title(title: &str) -> &str {
    let mut base = title.split(" - ").next().unwrap_or(title).trim_end();
    while let Some(close) = base.chars().last().filter(|c| matches!(c, ')' | ']')) {
        let open = if close == ')' { '(' } else { '[' };
        let Some(start) = base.rfind(open) else { break };
        let inner = base[start + 1..base.len() - 1].to_lowercase();
        let qualifier = inner.trim().chars().all(|c| c.is_ascii_digit())
            || inner
                .split(|c: char| !c.is_alphanumeric() && c != '.')
                .any(|word| TITLE_QUALIFIERS.contains(&word));
        if !qualifier || start == 0 {
            break;
        }
        base = base[..start].trim_end();
    }
    if base.is_empty() { title } else { base }
}

/// Lookup keys for a player-supplied title: as given, plus without release qualifiers.
pub(crate) fn song_keys(title: &str) -> Vec<String> {
    let mut keys = vec![match_key(title)];
    let base = match_key(base_title(title));
    if !keys.contains(&base) {
        keys.push(base);
    }
    keys
}

/// The credit as given, then each credited artist of a multi-artist credit ("A, B & C").
pub fn artist_parts(artist: &str) -> Vec<String> {
    let mut parts = vec![artist.to_string()];
    for sep in [",", "&", "、", " feat. ", " ft. ", " featuring "] {
        parts = parts
            .iter()
            .flat_map(|part| part.split(sep).map(str::to_string).collect::<Vec<_>>())
            .collect();
    }
    let mut out = vec![artist.trim().to_string()];
    for part in parts.iter().map(|p| p.trim()) {
        if !part.is_empty() && !out.iter().any(|o| match_key(o) == match_key(part)) {
            out.push(part.to_string());
        }
    }
    out.truncate(16);
    out
}

/// Lookup keys for a player-supplied artist credit.
pub(crate) fn artist_keys(artist: &str) -> Vec<String> {
    artist_parts(artist).iter().map(|p| match_key(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_keys_add_title_without_release_qualifiers() {
        assert_eq!(song_keys("Creep"), vec!["creep"]);
        assert_eq!(
            song_keys("American Woman - 2024 Remaster"),
            vec!["american woman - 2024 remaster", "american woman"]
        );
        assert_eq!(
            song_keys("The Game Of Love (feat. Michelle Branch)"),
            vec![
                "the game of love (feat. michelle branch)",
                "the game of love"
            ]
        );
        assert_eq!(
            song_keys("Show Me The Meaning Of Being Lonely ( 1999 ) - Backstreet Boys")[1],
            "show me the meaning of being lonely"
        );
        assert_eq!(song_keys("(Going Down) Love In An Elevator").len(), 1);
        assert_eq!(song_keys("Sweet Dreams (Are Made of This)").len(), 1);
    }

    #[test]
    fn keys_fold_dashes_and_strip_version_markers() {
        assert_eq!(
            artist_keys("Bachman\u{2013}Turner Overdrive"),
            vec!["bachman-turner overdrive"]
        );
        assert_eq!(
            song_keys("Seven (feat. Latto) (Explicit Ver.)"),
            vec!["seven (feat. latto) (explicit ver.)", "seven"]
        );
    }

    #[test]
    fn artist_keys_add_each_credited_artist() {
        assert_eq!(artist_keys("TWICE"), vec!["twice"]);
        assert_eq!(
            artist_keys("Reneé Rapp, Megan Thee Stallion"),
            vec![
                "reneé rapp, megan thee stallion",
                "reneé rapp",
                "megan thee stallion"
            ]
        );
        assert_eq!(
            artist_parts("George Thorogood & The Destroyers"),
            vec![
                "George Thorogood & The Destroyers",
                "George Thorogood",
                "The Destroyers"
            ]
        );
    }

    #[test]
    fn base_title_keeps_non_qualifier_parentheses() {
        assert_eq!(
            base_title("Sweet Dreams (Are Made of This)"),
            "Sweet Dreams (Are Made of This)"
        );
        assert_eq!(base_title("Seven (feat. Latto) (Explicit Ver.)"), "Seven");
    }
}
