//! Splits a document body into retrieval chunks of roughly TARGET..=MAX tokens along
//! paragraph and sentence boundaries. Each chunk after the first opens with the previous
//! chunk's last sentence, so a story spanning the boundary keeps its context.

pub const TARGET_TOKENS: usize = 150;
pub const MAX_TOKENS: usize = 250;

fn sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = paragraph.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        let cjk_end = matches!(c, '。' | '！' | '？');
        let latin_end =
            matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|n| n.is_whitespace());
        if cjk_end || latin_end {
            let s = current.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Splits one sentence longer than MAX_TOKENS on whitespace (or characters when unspaced).
/// If a single whitespace-delimited unit itself exceeds MAX_TOKENS, splits it by characters.
fn split_oversized(sentence: &str, count: &dyn Fn(&str) -> usize) -> Vec<String> {
    let units: Vec<String> = if sentence.contains(' ') {
        sentence
            .split_whitespace()
            .map(|w| format!("{w} "))
            .collect()
    } else {
        sentence.chars().map(String::from).collect()
    };
    let mut out = Vec::new();
    let mut current = String::new();
    for unit in units {
        // If the unit itself exceeds MAX_TOKENS, split it by characters
        if count(&unit) > MAX_TOKENS {
            // Emit current chunk first
            if !current.is_empty() {
                out.push(current.trim().to_string());
                current.clear();
            }
            // Split the oversized unit by characters
            for char_unit in unit.chars().map(String::from) {
                if !current.is_empty() && count(&format!("{current}{char_unit}")) > MAX_TOKENS {
                    out.push(current.trim().to_string());
                    current.clear();
                }
                current.push_str(&char_unit);
            }
        } else {
            if !current.is_empty() && count(&format!("{current}{unit}")) > MAX_TOKENS {
                out.push(current.trim().to_string());
                current.clear();
            }
            current.push_str(&unit);
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

pub fn chunk(body: &str, count_tokens: &dyn Fn(&str) -> usize) -> Vec<String> {
    let mut pieces = Vec::new();
    for paragraph in body.split('\n').map(str::trim).filter(|p| !p.is_empty()) {
        for sentence in sentences(paragraph) {
            if count_tokens(&sentence) > MAX_TOKENS {
                pieces.extend(split_oversized(&sentence, count_tokens));
            } else {
                pieces.push(sentence);
            }
        }
    }
    let joined = |parts: &[String], next: &str| {
        if parts.is_empty() {
            next.to_string()
        } else {
            format!("{} {next}", parts.join(" "))
        }
    };
    let mut chunks = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut added = 0; // sentences added since the last emitted chunk (excludes the overlap)
    for piece in pieces {
        if added > 0 && count_tokens(&joined(&current, &piece)) > MAX_TOKENS {
            chunks.push(current.join(" "));
            current = current.last().cloned().into_iter().collect();
            added = 0;
        }
        if added == 0 && count_tokens(&joined(&current, &piece)) > MAX_TOKENS {
            current.clear(); // the overlap sentence does not fit with this piece
        }
        current.push(piece);
        added += 1;
        if count_tokens(&current.join(" ")) >= TARGET_TOKENS {
            chunks.push(current.join(" "));
            current = current.last().cloned().into_iter().collect();
            added = 0;
        }
    }
    if added > 0 {
        chunks.push(current.join(" "));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> usize {
        s.split_whitespace()
            .count()
            .max(s.chars().filter(|c| !c.is_ascii()).count() / 2)
    }

    #[test]
    fn splits_long_text_into_bounded_overlapping_chunks() {
        let body: String = (0..60)
            .map(|i| {
                format!("Sentence number {i} tells part of how the band recorded this track. ")
            })
            .collect();
        let chunks = chunk(&body, &words);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| words(c) <= MAX_TOKENS));
        // The last sentence of each chunk opens the next one.
        let last = chunks[0].rsplit("Sentence").next().unwrap();
        assert!(chunks[1].starts_with(&format!("Sentence{last}").trim_end().to_string()));
    }

    #[test]
    fn keeps_short_paragraphs_together_and_splits_on_japanese_full_stop() {
        let chunks = chunk("短い段落です。二文目です。\n\n次の段落です。", &words);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("二文目です。") && chunks[0].contains("次の段落です。"));
    }

    #[test]
    fn hard_splits_a_single_oversized_sentence_without_duplicates() {
        let chunks = chunk(&"word ".repeat(600), &words);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| words(c) <= MAX_TOKENS));
        assert_eq!(chunks.iter().map(|c| words(c)).sum::<usize>(), 600);
    }

    #[test]
    fn empty_body_has_no_chunks() {
        assert!(chunk("  \n\n ", &words).is_empty());
    }

    #[test]
    fn handles_over_long_unspaced_runs_within_250_tokens() {
        fn token_count(s: &str) -> usize {
            s.split_whitespace()
                .map(|w| if w.is_ascii() { 1 } else { w.chars().count() })
                .sum()
        }

        let body = format!("Intro {}{} outro.", "音".repeat(2000), "");
        let chunks = chunk(&body, &token_count);

        // All chunks must stay within MAX_TOKENS
        assert!(
            chunks.iter().all(|c| token_count(c) <= MAX_TOKENS),
            "Found chunk with {} tokens, exceeds MAX_TOKENS ({})",
            chunks.iter().map(|c| token_count(c)).max().unwrap_or(0),
            MAX_TOKENS
        );

        // Verify no characters are lost or duplicated
        let original_chars: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let reconstructed: String = chunks
            .iter()
            .flat_map(|c| c.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            original_chars,
            reconstructed,
            "Character mismatch: expected {} chars, got {}",
            original_chars.len(),
            reconstructed.len()
        );
    }
}
