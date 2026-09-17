/// Semantic search over the indexed code units using cosine similarity on TF-IDF vectors.
/// No external ML dependencies — fast, local, good enough for codebase-scale corpora.
use crate::compressor::{build_term_vector_str, cosine_similarity};
use crate::memory::Store;
use crate::model::CodeUnit;

pub struct SearchResult<'a> {
    pub unit: &'a CodeUnit,
    pub score: f32,
}

/// Search `units` for entries semantically similar to `query`.
/// Returns up to `limit` results sorted by descending similarity.
pub fn semantic_search<'a>(
    query: &str,
    units: &'a [CodeUnit],
    limit: usize,
) -> Vec<SearchResult<'a>> {
    let query_vec = build_term_vector_str(query);

    let mut scored: Vec<SearchResult> = units
        .iter()
        .map(|u| SearchResult {
            unit: u,
            score: cosine_similarity(&query_vec, &u.term_vector),
        })
        .filter(|r| r.score > 0.0)
        .collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// How much of the gap to a perfect score the best keyword hit can close.
const LEXICAL_WEIGHT: f32 = 0.55;

/// Hybrid BM25 + cosine search.
///
/// Each unit keeps its cosine similarity, and a keyword hit closes part of the
/// remaining gap to 1.0 -- less the further down the BM25 ranking it sits:
///
///   score = cosine + (1 − cosine) × 0.55 / (1 + lexical_rank)
///
/// So a keyword match can only raise a score, never lower it. The first version
/// blended the two (`0.55 / (1 + rank) + 0.45 × cosine`), under which a unit with
/// cosine 0.9 that was also the second keyword hit fell to 0.68 -- below the
/// very same unit with no keyword match at all.
pub fn hybrid_search<'a>(
    store: &Store,
    query: &str,
    units: &'a [CodeUnit],
    limit: usize,
) -> Vec<SearchResult<'a>> {
    // Build a unit-id -> rank map from BM25 keyword search.
    let fts_hits = store.fts_code_units(query, limit * 4).unwrap_or_default();
    let lexical_rank: std::collections::HashMap<&str, usize> = fts_hits
        .iter()
        .enumerate()
        .map(|(rank, (unit_id, _))| (unit_id.as_str(), rank))
        .collect();

    let query_vec = build_term_vector_str(query);

    let mut scored: Vec<SearchResult> = units
        .iter()
        .map(|u| {
            let cosine = cosine_similarity(&query_vec, &u.term_vector);
            let score = match lexical_rank.get(u.id.as_str()) {
                Some(&rank) => {
                    cosine + (1.0 - cosine).max(0.0) * LEXICAL_WEIGHT / (1.0 + rank as f32)
                }
                None => cosine,
            };
            SearchResult { unit: u, score }
        })
        .filter(|r| r.score > 0.0)
        .collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// Keyword search — name and compressed text substring match.
/// Complement to semantic search for exact lookups.
pub fn keyword_search<'a>(
    query: &str,
    units: &'a [CodeUnit],
) -> Vec<&'a CodeUnit> {
    let q = query.to_lowercase();
    units
        .iter()
        .filter(|u| {
            u.name.to_lowercase().contains(&q)
                || u.compressed.to_lowercase().contains(&q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_term_vector_str, hybrid_search, SearchResult};
    use crate::model::CodeUnit;
    use crate::test_support::TempStore;

    fn unit(
        id: &str,
        name: &str,
        summary: &str,
        compressed: &str,
        term_vector: Vec<(String, f32)>,
    ) -> CodeUnit {
        CodeUnit {
            id: id.to_string(),
            kind: "fn".to_string(),
            name: name.to_string(),
            module_path: "test::mod".to_string(),
            summary: summary.to_string(),
            compressed: compressed.to_string(),
            term_vector,
            indexed_at: chrono::Utc::now(),
        }
    }

    fn score_of(results: &[SearchResult], id: &str) -> f32 {
        results.iter().find(|r| r.unit.id == id).map(|r| r.score)
            .unwrap_or_else(|| panic!("{id} missing from results"))
    }

    #[test]
    fn hybrid_search_uses_lexical_rank_when_cosine_is_zero() {
        let store = TempStore::new("hybrid_lexical_only").unwrap();

        let lexical_hit = unit(
            "test::alpha_hit",
            "alpha_hit",
            "contains alpha token",
            "fn alpha_hit() { let alpha = 1; }",
            vec![],
        );
        let no_hit = unit(
            "test::no_hit",
            "no_hit",
            "no such token",
            "fn no_hit() { let beta = 2; }",
            vec![],
        );

        store.upsert_unit(&lexical_hit).unwrap();
        store.upsert_unit(&no_hit).unwrap();

        let units = vec![no_hit.clone(), lexical_hit.clone()];
        let results = hybrid_search(&store, "alpha", &units, 5);

        assert!(!results.is_empty(), "lexical hit should surface even with zero cosine");
        assert_eq!(results[0].unit.id, lexical_hit.id);
        assert!(results[0].score > 0.0);
    }

    /// The unit under test is only the SECOND keyword hit, which is where the
    /// original blend went wrong: it scored below an identical unit that did
    /// not match the keywords at all.
    #[test]
    fn a_keyword_hit_never_lowers_a_score() {
        let store = TempStore::new("hybrid_monotonic").unwrap();

        let top = unit(
            "test::spawn_plugin", "spawn_plugin", "spawn plugin",
            "fn spawn_plugin() { spawn plugin }", vec![],
        );
        let second = unit(
            "test::second", "second", "",
            "fn second() { spawn }", build_term_vector_str("spawn"),
        );
        let twin = unit(
            "test::twin", "twin", "",
            "fn twin() {}", build_term_vector_str("spawn"),
        );
        for u in [&top, &second, &twin] {
            store.upsert_unit(u).unwrap();
        }

        let units = vec![twin.clone(), second.clone(), top.clone()];
        let results = hybrid_search(&store, "spawn plugin", &units, 5);

        let (second_score, twin_score) = (score_of(&results, "test::second"), score_of(&results, "test::twin"));
        assert!(
            second_score > twin_score,
            "a keyword match lowered the score: {second_score} with the hit vs {twin_score} without"
        );
    }

    /// Requiring every query word in one unit meant only a big unit could match
    /// a multi-word query: the live `canvas::core::Canvas` (~1,450 tokens) was
    /// the only unit containing both "spawn" and "plugin", so it came first and
    /// every unit actually about spawning was not a keyword hit at all.
    #[test]
    fn keyword_search_matches_any_term() {
        let store = TempStore::new("fts_any_term").unwrap();
        let both = unit("test::Canvas", "Canvas", "", "fn draw() { spawn(); plugin(); }", vec![]);
        let one = unit("test::spawn", "spawn", "spawn an object", "fn spawn(obj)", vec![]);
        for u in [&both, &one] {
            store.upsert_unit(u).unwrap();
        }

        let hits = store.fts_code_units("spawn plugin", 5).unwrap();
        assert!(
            hits.iter().any(|(id, _)| id == "test::spawn"),
            "a unit matching one of the query words was not a keyword hit: {hits:?}"
        );
        assert!(
            store.fts_code_units("spawn or not", 5).is_ok(),
            "a query word that is also an FTS5 operator broke the search"
        );
    }

    /// Two units of the same size holding the same word: the one NAMED for it
    /// must rank first, not tie with one that only uses it in its body.
    #[test]
    fn a_name_match_outranks_the_same_word_in_a_body() {
        let store = TempStore::new("fts_name_weight").unwrap();
        let in_body = unit("test::go", "go", "", "fn go() { spawn }", vec![]);
        let named = unit("test::spawn", "spawn", "", "fn spawn() { go }", vec![]);
        for u in [&in_body, &named] {
            store.upsert_unit(u).unwrap();
        }

        let hits = store.fts_code_units("spawn", 5).unwrap();
        assert_eq!(hits.first().map(|(id, _)| id.as_str()), Some("test::spawn"), "{hits:?}");
    }

    #[test]
    fn hybrid_search_preserves_semantic_score_without_lexical_hits() {
        let store = TempStore::new("hybrid_semantic_only").unwrap();
        let query = "Action::SetCollisionLayer";

        let semantic_only = unit(
            "test::semantic_only",
            "semantic_only",
            "semantic vector match only",
            "fn semantic_only() {}",
            build_term_vector_str(query),
        );

        store.upsert_unit(&semantic_only).unwrap();

        let units = vec![semantic_only.clone()];
        let results = hybrid_search(&store, query, &units, 5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].unit.id, semantic_only.id);
        assert!(
            results[0].score > 0.9,
            "semantic-only matches should retain high confidence when no lexical hit exists"
        );
    }
}
