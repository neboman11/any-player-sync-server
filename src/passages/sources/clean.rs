//! Text cleanup and identity matching shared by ingest sources.

use crate::match_keys::base_title;
pub use crate::match_keys::match_key as key;

/// Section headings after which Wikipedia plain text is lists, credits or citations.
const TRAILING_SECTIONS: &[&str] = &[
    "References",
    "Notes",
    "Footnotes",
    "Citations",
    "Sources",
    "External links",
    "See also",
    "Further reading",
    "Track listing",
    "Track listings",
    "Formats and track listings",
    "Personnel",
    "Credits and personnel",
    "Charts",
    "Weekly charts",
    "Year-end charts",
    "Certifications",
    "Certifications and sales",
    "Release history",
    "脚注",
    "出典",
    "注釈",
    "参考文献",
    "関連項目",
    "外部リンク",
    "収録曲",
    "参加ミュージシャン",
    "チャート",
    "각주",
    "외부 링크",
    "같이 보기",
    "참고 문헌",
    "트랙 목록",
    "차트",
];

pub fn strip_wiki_sections(text: &str) -> String {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("==")
            && TRAILING_SECTIONS.contains(&trimmed.trim_matches('=').trim())
        {
            return text[..offset].trim_end().to_string();
        }
        offset += line.len();
    }
    text.trim_end().to_string()
}

pub fn strip_lastfm_suffix(text: &str) -> String {
    let cut = ["<a href", "Read more on Last.fm", "User-contributed text"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len());
    text[..cut].trim().to_string()
}

pub fn truncate_body(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').unwrap_or(end);
    text[..cut].trim_end().to_string()
}

/// A source title names the wanted song/album/artist if its key equals the wanted base title's
/// key, or is that key followed by a parenthesised disambiguation ("X (Y song)", "X (Yの曲)").
pub fn title_matches(candidate: &str, wanted: &str) -> bool {
    let c = key(candidate);
    let w = key(base_title(wanted));
    c == w || (c.starts_with(&format!("{w} (")) && c.ends_with(')'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_wiki_sections_in_all_languages() {
        let en =
            "Intro.\n\n== Background ==\nStory.\n\n== References ==\nx\n== External links ==\ny";
        assert_eq!(
            strip_wiki_sections(en),
            "Intro.\n\n== Background ==\nStory."
        );
        assert!(
            !strip_wiki_sections("導入。\n\n== 背景 ==\n話。\n\n== 脚注 ==\nx").contains("脚注")
        );
        assert!(!strip_wiki_sections("소개.\n\n== 각주 ==\nx").contains("각주"));
        assert!(
            !strip_wiki_sections("Intro.\n\n=== Track listing ===\n1. A").contains("Track listing")
        );
    }

    #[test]
    fn lastfm_suffix_is_removed() {
        let s = "Great song. <a href=\"https://www.last.fm/music/X\">Read more on Last.fm</a>. User-contributed text is available under the Creative Commons By-SA License";
        assert_eq!(strip_lastfm_suffix(s), "Great song.");
    }

    #[test]
    fn truncate_cuts_at_paragraph_boundary() {
        assert_eq!(truncate_body("aaa\nbbb\nccc", 9), "aaa\nbbb");
        assert_eq!(truncate_body("short", 100), "short");
    }

    #[test]
    fn title_matching() {
        assert!(title_matches(
            "Hotel California (Eagles song)",
            "Hotel California - 2013 Remaster"
        ));
        assert!(title_matches("シルエット (KANA-BOONの曲)", "シルエット"));
        assert!(!title_matches("Hotel California 2", "Hotel California"));
    }
}
