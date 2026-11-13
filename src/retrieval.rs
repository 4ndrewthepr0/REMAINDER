/// Query-adaptive fusion, selected during development evaluation. No corpus,
/// answer labels or dataset identifiers participate in the runtime policy.
/// Numeric settings preserve the explicit fixed-weight configuration.
pub fn hybrid_dense_weight(query: &str, setting: &str) -> f64 {
    if setting != "adaptive" {
        return setting.parse::<f64>().ok()
            .filter(|value| value.is_finite() && *value >= 0.0).unwrap_or(1.0);
    }
    let quoted_or_numeric = query.chars().any(|c| c.is_ascii_digit() || matches!(c, '"' | '“' | '`'));
    let camel_case = query.chars().zip(query.chars().skip(1))
        .any(|(a, b)| a.is_ascii_lowercase() && b.is_ascii_uppercase());
    let identifier = query.split(|c: char| !c.is_alphanumeric() && c != '_').any(|token| {
        let internal_underscore = token.match_indices('_').any(|(at, _)| at > 0 && at + 1 < token.len());
        let acronym = token.chars().count() > 1 && token.chars().all(char::is_alphabetic)
            && token.chars().any(char::is_uppercase) && token == token.to_uppercase();
        internal_underscore || acronym
    });
    // Explicit identifiers benefit from semantic disambiguation of their
    // surrounding context; prose queries keep a stronger lexical anchor.
    if quoted_or_numeric || camel_case || identifier { 2.0 } else { 0.5 }
}

/// Locates a passage after the indexer has collapsed source line breaks to spaces.
/// The returned offset counts Unicode characters in the original source.
pub fn locate_normalized_passage(raw: &str, passage: &str) -> Option<usize> {
    let mut needle = String::new();
    for word in passage.split_whitespace() {
        if !needle.is_empty() {
            needle.push(' ');
        }
        needle.push_str(word);
    }
    if needle.chars().count() < 16 {
        return None;
    }
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    // KMP over the normalized character stream avoids building another copy
    // of a multi-megabyte source and a raw-offset entry for every UTF-8 byte.
    let mut prefix = vec![0; needle.len()];
    for index in 1..needle.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] { matched += 1; }
        prefix[index] = matched;
    }
    let mut offsets = std::collections::VecDeque::with_capacity(needle.len());
    let mut matched = 0;
    let mut feed = |character: char, raw_index: usize| {
        if offsets.len() == needle.len() { offsets.pop_front(); }
        offsets.push_back(raw_index);
        while matched > 0 && character != needle[matched] {
            matched = prefix[matched - 1];
        }
        if character == needle[matched] { matched += 1; }
        if matched == needle.len() { offsets.front().copied() } else { None }
    };
    let mut previous_space = false;
    let mut has_content = false;
    for (raw_index, character) in raw.chars().enumerate() {
        if character.is_whitespace() {
            if !previous_space && has_content {
                if let Some(offset) = feed(' ', raw_index) { return Some(offset); }
            }
            previous_space = true;
        } else {
            for lowered in character.to_lowercase() {
                if let Some(offset) = feed(lowered, raw_index) { return Some(offset); }
            }
            previous_space = false;
            has_content = true;
        }
    }
    None
}

/// Keeps content-bearing words in natural language questions. The original
/// query remains available to callers; this is an optional retrieval variant.
pub fn focus_query(query: &str) -> String {
    const FILLER: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "based", "be", "by", "can", "could",
        "do", "does", "for", "from", "given", "how", "in", "is", "it", "its",
        "may", "of", "on", "or", "our", "the", "their", "there", "these", "this",
        "to", "was", "were", "what", "when", "where", "which", "who", "why",
        "with", "would", "you", "your", "according", "article", "author",
        "document", "following", "above", "below", "question", "answer",
        "proposed", "described", "please", "tell", "about", "information",
    ];

    let mut seen = std::collections::HashSet::new();
    let mut focused = Vec::new();
    for word in query.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.is_empty() { continue; }
        let lower = word.to_lowercase();
        if (lower.chars().count() < 3 && !lower.chars().any(|c| c.is_numeric()))
            || FILLER.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower) {
            focused.push(word);
        }
    }
    if focused.len() < 2 { query.to_string() } else { focused.join(" ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_fusion_uses_query_syntax_without_changing_explicit_weights() {
        for query in ["How does get_source resolve a path?", "Explain FooBar", "What happened in 2025?", "Why choose the GPU?", "Why use \"SQLite\"?"] {
            assert_eq!(hybrid_dense_weight(query, "adaptive"), 2.0);
        }
        assert_eq!(hybrid_dense_weight("Why was the caching decision reversed?", "adaptive"), 0.5);
        assert_eq!(hybrid_dense_weight("get_source", "0.75"), 0.75);
    }

    #[test]
    fn fusion_weight_rejects_nonfinite_and_negative_overrides() {
        for setting in ["NaN", "inf", "-1", "invalid"] {
            assert_eq!(hybrid_dense_weight("query", setting), 1.0);
        }
        assert_eq!(hybrid_dense_weight("query", "0"), 0.0);
    }

    #[test]
    fn locates_chunk_after_line_breaks_were_collapsed() {
        let raw = "header\n\nfn test_eval(self):\n    # GH 16363\n    df = DataFrame({'x': [0]})\nfooter";
        let chunk = "fn test_eval(self): # GH 16363 df = DataFrame({'x': [0]})";
        assert_eq!(locate_normalized_passage(raw, chunk), Some(8));
    }

    #[test]
    fn unicode_offsets_are_original_character_positions() {
        let raw = "🚀 prefix\nİstanbul  --  Report on the city\nend";
        let chunk = "İstanbul -- Report on the city";
        assert_eq!(locate_normalized_passage(raw, chunk), Some(9));
    }

    #[test]
    fn avoids_matching_short_unreliable_fragments() {
        assert_eq!(locate_normalized_passage("some text", "text"), None);
    }

    #[test]
    fn repeated_prefix_and_mixed_whitespace_find_the_right_passage() {
        let raw = "  alpha alpha alpha alphabet\n\tunique marker at the end";
        assert_eq!(locate_normalized_passage(raw, "alpha alphabet unique marker at the end"), Some(14));
    }

    #[test]
    fn repeated_boilerplate_prefix_uses_the_full_passage() {
        let prefix = "Shared report boilerplate and common introductory sentences. ".repeat(3);
        let first = format!("{}First section-specific conclusion.", prefix);
        let second = format!("{}Second section-specific conclusion.", prefix);
        let raw = format!("{}\n\n{}", first, second);
        assert_eq!(locate_normalized_passage(&raw, &second), Some(first.chars().count() + 2));
    }

    #[test]
    fn focus_keeps_domain_terms_and_code_identifiers() {
        let q = "In this article, based on what features, the author proposed a new stratification method and what is the clinical meaning?";
        let focus = focus_query(q);
        assert!(focus.contains("features") && focus.contains("stratification") && focus.contains("clinical"));
        assert!(!focus.contains("article") && !focus.contains("author"));
        assert_eq!(focus_query("AUTH"), "AUTH");
        assert!(focus_query("How does pandas.eval work in TestEval?").contains("pandas"));
    }
}
