//! Computed helpers shared by the MCP tools.
//!
//! This module used to also hold hand-curated Quartz knowledge — a trait matrix,
//! borrow notes, engine constants, performance characteristics and intent→Action
//! suggestions. All of it was Quartz-specific judgment that could only be updated
//! by editing Rust and recompiling, so it migrated to cortex's database, where it
//! is queryable through `get_anti_patterns`, `list_patterns` and `recall` and can
//! be extended from a live session without a rebuild.
//!
//! What remains is derived from parsed source and therefore works on any Rust
//! project.

use crate::model::ApiItem;

/// Rank indexed items by how well they match a free-text query.
///
/// Scores an exact name match highest, then a partial name, then module path,
/// then member names, then doc text — enough signal for "what else is related to
/// this?" without an embedding model. The previous version returned unranked
/// matches, so a doc-comment mention could outrank the type actually named.
pub fn find_related_apis(query: &str, items: &[ApiItem]) -> Vec<ApiItem> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(i32, &ApiItem)> = items
        .iter()
        .filter_map(|item| {
            let name = item.name.to_lowercase();
            let mut score = 0;

            if name == q {
                score += 100;
            } else if name.contains(&q) {
                score += 50;
            }
            if item.module_str().to_lowercase().contains(&q) {
                score += 20;
            }
            if item.methods.iter().any(|m| m.name.to_lowercase().contains(&q)) {
                score += 15;
            }
            if item.variants.iter().any(|v| {
                v.name.to_lowercase().contains(&q) || v.doc.to_lowercase().contains(&q)
            }) {
                score += 15;
            }
            if item.doc.to_lowercase().contains(&q) {
                score += 10;
            }

            (score > 0).then_some((score, item))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, i)| i.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, ItemKind, Visibility};

    fn item(name: &str, doc: &str) -> ApiItem {
        ApiItem {
            kind: ItemKind::Struct,
            name: name.to_string(),
            doc: doc.to_string(),
            signature: String::new(),
            module_path: vec!["m".into()],
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: String::new(),
            traits_impl: vec![],
            origin: String::new(),
            visibility: Visibility::Public,
            span: None,
            confidence: Confidence::Resolved,
            language: "rust".to_string(),
            calls: Vec::new(),
        }
    }

    /// The type actually named must outrank one that merely mentions it in a doc.
    #[test]
    fn exact_name_outranks_a_doc_mention() {
        let items = vec![item("Scene", "holds the camera"), item("Camera", "")];
        let got = find_related_apis("camera", &items);
        assert_eq!(got[0].name, "Camera");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn ties_break_deterministically_and_empty_query_returns_nothing() {
        let items = vec![item("Bravo", "x"), item("Alpha", "x")];
        assert_eq!(find_related_apis("x", &items)[0].name, "Alpha");
        assert!(find_related_apis("", &items).is_empty());
        assert!(find_related_apis("   ", &items).is_empty());
    }
}
