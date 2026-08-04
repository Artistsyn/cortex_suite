/// Semantic search over the indexed code units using cosine similarity on TF-IDF vectors.
/// No external ML dependencies — fast, local, good enough for codebase-scale corpora.
use crate::compressor::{build_term_vector_str, cosine_similarity};
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
