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

/// Hybrid BM25 + cosine search.
///
/// Lexical rank (0-indexed BM25 position) and cosine similarity are combined:
///   score = 0.55 × (1 / (1 + lexical_rank)) + 0.45 × cosine_similarity
///
/// This matches the formula verified in rta-smriti-brain and outperforms either
/// signal alone on codebase corpora.
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
            let lex_rank = lexical_rank.get(u.id.as_str()).copied().unwrap_or(usize::MAX);
            let score = if lex_rank == usize::MAX {
                // No lexical evidence: keep pure semantic behavior instead of
                // suppressing match confidence by a constant factor.
                cosine
            } else {
                let lex_weight = 1.0 / (1.0 + lex_rank as f32);
                0.55 * lex_weight + 0.45 * cosine
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
    use super::{build_term_vector_str, hybrid_search};
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
            "no alpha token",
            "fn no_hit() { let beta = 2; }",
            vec![],
        );

        store.upsert_unit(&lexical_hit).unwrap();
        store.upsert_unit(&no_hit).unwrap();
        store.rebuild_fts().unwrap();

        let units = vec![lexical_hit.clone(), no_hit.clone()];
        let results = hybrid_search(&store, "alpha", &units, 5);

        assert!(!results.is_empty(), "lexical hit should surface even with zero cosine");
        assert_eq!(results[0].unit.id, lexical_hit.id);
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn hybrid_search_blends_lexical_and_semantic_signals() {
        let store = TempStore::new("hybrid_blend").unwrap();
        let query = "spawn plugin";

        let lexical_only = unit(
            "test::lexical_only",
            "lexical_only",
            "contains query text for lexical match",
            "fn lexical_only() { // spawn plugin }",
            build_term_vector_str(query),
        );
        let semantic_only = unit(
            "test::semantic_only",
            "semantic_only",
            "semantic vector only",
            "fn semantic_only() { }",
            build_term_vector_str(query),
        );

        store.upsert_unit(&lexical_only).unwrap();
        store.upsert_unit(&semantic_only).unwrap();
        store.rebuild_fts().unwrap();

        let units = vec![lexical_only.clone(), semantic_only.clone()];
        let results = hybrid_search(&store, query, &units, 5);

        assert_eq!(results.len(), 2, "both lexical and semantic candidates should appear");
        assert_eq!(results[0].unit.id, lexical_only.id);
        assert!(results.iter().any(|r| r.unit.id == semantic_only.id));
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
        store.rebuild_fts().unwrap();

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
