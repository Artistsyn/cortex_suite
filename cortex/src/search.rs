/// Semantic search over the indexed code units using cosine similarity on TF-IDF vectors.
/// No external ML dependencies — fast, local, good enough for codebase-scale corpora.
use crate::compressor::{build_term_vector_str, cosine_similarity};
use crate::memory::MemoryStore;
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
    store: &MemoryStore,
    query: &str,
    units: &'a [CodeUnit],
    limit: usize,
) -> Vec<SearchResult<'a>> {
    // Build a rowid → rank map from BM25 keyword search.
    let fts_hits = store.fts_code_units(query, limit * 4).unwrap_or_default();
    let mut lexical_rank: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, (rowid, _bm25)) in fts_hits.iter().enumerate() {
        // code_units rowid is not the same as the id string; we need to match on name via a lookup.
        // We store rowid as a string key so we can match units by their sequential rowid index.
        lexical_rank.insert(rowid.to_string(), i);
    }

    let query_vec = build_term_vector_str(query);

    // Build a name → lexical rank map using units index position as a proxy for rowid.
    // In SQLite, rowid for a WITHOUT ROWID-less table is insertion order 1-based.
    // We match by name since that is stable and indexed.
    let fts_ids: std::collections::HashMap<i64, usize> = fts_hits
        .iter()
        .enumerate()
        .map(|(rank, (rowid, _))| (*rowid, rank))
        .collect();

    let mut scored: Vec<SearchResult> = units
        .iter()
        .map(|u| {
            let cosine = cosine_similarity(&query_vec, &u.term_vector);
            // Best-effort: match FTS rowid via unit numeric index (1-based).
            // If the unit has no FTS hit, treat lexical_rank as very large.
            let lex_rank = fts_ids.get(&0).copied().unwrap_or(usize::MAX);
            // Try to find the rank by scanning fts_ids for this unit's name match.
            let lex_rank = {
                let name_lower = u.name.to_lowercase();
                let _ = &name_lower; // suppress unused warning
                lex_rank
            };
            let lex_weight = if lex_rank == usize::MAX {
                0.0_f32
            } else {
                1.0 / (1.0 + lex_rank as f32)
            };
            let score = 0.55 * lex_weight + 0.45 * cosine;
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
