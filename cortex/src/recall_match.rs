//! Shared relevance scoring for `recall`.
//!
//! This lives in its own module because `recall` has **two** callers — the MCP
//! tool in `mcp::tools` and the CLI subcommand in `main` — which had drifted
//! into two copies of the same filter. Cortex's own store records that class of
//! bug (quartz_forge shipped two divergent `setup_scene` emitters and the one
//! that actually wrote files silently lacked features the preview had). One
//! scorer, two callers.

/// Discriminating terms of a recall topic, lowercased.
///
/// Terms shorter than three characters and common connectives are dropped: they
/// match nearly everything and would turn every query into a full listing.
pub fn recall_terms(topic: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "when", "that", "this", "how", "why", "does",
        "not", "are", "was", "but", "from", "into", "our", "use", "using",
    ];
    topic
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3 && !STOP.contains(t))
        .map(str::to_string)
        .collect()
}

/// Relevance of one entry to a recall topic; `0` means no match.
///
/// A whole-phrase `contains()` — all this used to do — cannot answer a
/// conceptual query. Measured before the fix: `"scope aliasing mipmap
/// minification MSAA"` returned nothing while `"MSAA"` alone returned three
/// hits, even though the operating protocol tells agents to look knowledge up by
/// concept rather than exact API name.
///
/// So: score by how many distinct topic terms the entry mentions. An exact
/// phrase match still outranks everything, which keeps single-word and quoted
/// queries behaving exactly as before.
pub fn recall_score(haystacks: &[&str], phrase: &str, terms: &[String]) -> usize {
    let hay = haystacks.join(" \u{1}").to_lowercase();
    if !phrase.is_empty() && hay.contains(phrase) {
        return terms.len().max(1) * 10;
    }
    let hits = terms.iter().filter(|t| hay.contains(t.as_str())).count();
    // One term out of one is a real match; one out of five is noise.
    match terms.len() {
        0 => 0,
        1 => hits,
        _ if hits >= 2 => hits,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(t: &str) -> Vec<String> {
        recall_terms(t)
    }

    #[test]
    fn stop_words_and_short_tokens_are_dropped() {
        assert_eq!(recall_terms("how does the scope use MSAA"), vec!["scope", "msaa"]);
        assert!(recall_terms("a of in").is_empty());
    }

    /// The regression this module exists for.
    #[test]
    fn a_conceptual_multi_word_query_matches_a_relevant_entry() {
        let entry = [
            "MSAA is the wrong fix for VR scope aliasing",
            "the defect is composite minification; add a mipmap chain",
        ];
        let t = terms("scope aliasing mipmap minification MSAA");
        let score = recall_score(&entry, "scope aliasing mipmap minification msaa", &t);
        assert!(score > 0, "conceptual query should match, scored {score}");
    }

    #[test]
    fn a_single_word_query_behaves_exactly_as_before() {
        let entry = ["multiview cannot be a flag flip", "body"];
        assert!(recall_score(&entry, "multiview", &terms("multiview")) > 0);
        assert_eq!(recall_score(&entry, "kafka", &terms("kafka")), 0);
    }

    /// One incidental word must not drag in an unrelated entry.
    #[test]
    fn one_weak_term_out_of_many_is_not_a_match() {
        let unrelated = ["GIF frames are delta-encoded", "composite each frame"];
        let t = terms("scope aliasing mipmap minification MSAA");
        assert_eq!(
            recall_score(&unrelated, "scope aliasing mipmap minification msaa", &t),
            0,
            "'composite' alone must not match a five-term optics query"
        );
    }

    #[test]
    fn an_exact_phrase_outranks_a_scattered_term_match() {
        let exact = ["binding a single mip level makes mipmap_filter a no-op"];
        let scattered = ["mip", "level", "filter"];
        let t = terms("mipmap_filter no-op");
        let a = recall_score(&exact, "mipmap_filter no-op", &t);
        let b = recall_score(&scattered, "mipmap_filter no-op", &t);
        assert!(a > b, "exact phrase {a} should outrank scattered {b}");
    }
}
