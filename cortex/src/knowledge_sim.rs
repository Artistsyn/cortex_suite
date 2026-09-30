//! Which stored entry is a new one closest to? (docs/self-learning-loop-2026-09-30.md §6.2)
//!
//! Local TF-IDF over the live patterns and anti-patterns: no tokens, a few
//! milliseconds. Backtested on the store's own history (744 writes, 12 real
//! supersedes):
//!
//! - "same pattern name, or cosine >= 0.9" flagged 5 writes, and all 5 were
//!   true supersedes, so it is safe to merge those automatically;
//! - the real corrections scored 0.02-0.34 against the entry they replaced,
//!   so no threshold separates a contradiction from a related entry. A 30-pair
//!   sample of nearest neighbours at 0.25-0.9 held 5 duplicates, 3 refinements,
//!   1 contradiction and 21 compatible entries.
//!
//! So similarity merges near-identical entries and shows an author the nearest
//! older entry. It never raises a conflict alarm by itself.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::memory::Store;

/// At or above this, the newer entry supersedes the older automatically.
pub const DUPLICATE_COSINE: f32 = 0.9;
/// At or above this (and below `DUPLICATE_COSINE`), the nearest older entry is
/// worth showing to whoever writes or uses the new one.
pub const RELATED_COSINE: f32 = 0.25;

const STOP: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "been", "this", "that", "with", "from", "not",
    "but", "then", "than", "into", "its", "it's", "use", "uses", "using", "when", "which",
    "what", "how", "all", "any", "can", "cannot", "does", "don't", "only", "one", "two", "more",
    "most", "also", "must", "should", "would", "will", "just", "same", "each", "every", "other",
    "there", "their", "they", "them", "our", "you", "your", "has", "have", "had", "after",
    "before", "about", "over", "under", "per", "via",
];

/// Lowercased words and identifiers of three or more characters: a letter or
/// underscore, then letters, digits, `_`, `:` or `.` (so `Canvas::run` and
/// `index-sources.json`'s parts survive as tokens).
pub fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.chars().count() >= 3 && !STOP.contains(&cur.as_str()) {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in lower.chars() {
        let starts = c.is_ascii_lowercase() || c == '_';
        let continues = starts || c.is_ascii_digit() || c == ':' || c == '.';
        if cur.is_empty() {
            if starts {
                cur.push(c);
            }
        } else if continues {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

#[derive(Debug, Clone)]
pub struct Doc {
    pub table: &'static str,
    pub id: i64,
    pub label: String,
    pub text: String,
    /// When it was written (added_at / approved_at), as stored.
    pub at: String,
    /// `None` while live.
    pub superseded_by: Option<i64>,
}

/// Every live pattern and anti-pattern, as the text similarity is measured on.
pub fn live_docs(store: &Store) -> Result<Vec<Doc>> {
    let mut docs = Vec::new();
    docs.extend(all_docs(store)?.into_iter().filter(|d| d.superseded_by.is_none()));
    Ok(docs)
}

/// Every pattern and anti-pattern, retired ones included, oldest first: what a
/// backtest over the store's history replays.
pub fn all_docs(store: &Store) -> Result<Vec<Doc>> {
    let mut docs = Vec::new();
    let mut stmt = store.conn().prepare(
        "SELECT id, description, wrong, correct, tags, added_at, superseded_by FROM anti_patterns",
    )?;
    let rows = stmt.query_map([], |r| {
        let (id, d, w, c, t, at): (i64, String, String, String, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
        Ok(Doc { table: "anti_patterns", id, label: d.clone(), text: format!("{d} {w} {c} {t}"), at, superseded_by: r.get(6)? })
    })?;
    docs.extend(rows.filter_map(|r| r.ok()));
    let mut stmt = store.conn().prepare(
        "SELECT id, name, intent, body, tags, approved_at, superseded_by FROM patterns",
    )?;
    let rows = stmt.query_map([], |r| {
        let (id, n, i, b, t, at): (i64, String, String, String, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
        Ok(Doc { table: "patterns", id, label: n.clone(), text: format!("{n} {i} {b} {t}"), at, superseded_by: r.get(6)? })
    })?;
    docs.extend(rows.filter_map(|r| r.ok()));
    docs.sort_by(|a, b| a.at.get(..19).cmp(&b.at.get(..19)));
    Ok(docs)
}

/// Every document's vector computed once, for comparisons within the corpus
/// (the backtest and seeding): IDF over the documents themselves, as in the
/// Python backtest the thresholds were calibrated on.
pub struct Corpus {
    pub docs: Vec<Doc>,
    vecs: Vec<HashMap<String, f32>>,
}

impl Corpus {
    pub fn build(docs: Vec<Doc>) -> Self {
        let toks: Vec<Vec<String>> = docs.iter().map(|d| tokens(&d.text)).collect();
        let mut df: HashMap<&str, f32> = HashMap::new();
        for t in &toks {
            let unique: HashSet<&str> = t.iter().map(String::as_str).collect();
            for w in unique {
                *df.entry(w).or_default() += 1.0;
            }
        }
        let n = docs.len().max(1) as f32;
        let vecs = toks
            .iter()
            .map(|t| {
                let mut tf: HashMap<&str, f32> = HashMap::new();
                for w in t {
                    *tf.entry(w.as_str()).or_default() += 1.0;
                }
                let mut v: HashMap<String, f32> = tf
                    .into_iter()
                    .map(|(w, c)| (w.to_string(), (1.0 + c.ln()) * (n / df.get(w).copied().unwrap_or(1.0)).ln()))
                    .collect();
                let norm = v.values().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.values_mut() {
                        *x /= norm;
                    }
                }
                v
            })
            .collect();
        Self { docs, vecs }
    }

    pub fn cosine(&self, a: usize, b: usize) -> f32 {
        let (x, y) = (&self.vecs[a], &self.vecs[b]);
        let (small, large) = if x.len() <= y.len() { (x, y) } else { (y, x) };
        small.iter().map(|(w, v)| v * large.get(w).copied().unwrap_or(0.0)).sum()
    }

    /// The closest document to `i` among those `keep` allows.
    pub fn nearest_to(&self, i: usize, keep: impl Fn(usize, &Doc) -> bool) -> Option<(usize, f32)> {
        (0..self.docs.len())
            .filter(|&j| j != i && keep(j, &self.docs[j]))
            .map(|j| (j, self.cosine(i, j)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
}

#[derive(Debug, Clone)]
pub struct Nearest {
    pub table: &'static str,
    pub id: i64,
    pub label: String,
    pub cosine: f32,
}

/// The live entry of `table` closest to `text`, skipping `exclude` (usually the
/// new entry itself).
pub fn nearest(store: &Store, table: &str, text: &str, exclude: Option<i64>) -> Result<Option<Nearest>> {
    Ok(nearest_in(&live_docs(store)?, table, text, exclude))
}

pub fn nearest_in(docs: &[Doc], table: &str, text: &str, exclude: Option<i64>) -> Option<Nearest> {
    let doc_toks: Vec<Vec<String>> = docs.iter().map(|d| tokens(&d.text)).collect();
    let query = tokens(text);
    if query.is_empty() {
        return None;
    }
    let mut df: HashMap<&str, f32> = HashMap::new();
    for toks in doc_toks.iter().chain(std::iter::once(&query)) {
        let unique: HashSet<&str> = toks.iter().map(String::as_str).collect();
        for w in unique {
            *df.entry(w).or_default() += 1.0;
        }
    }
    let n = (docs.len() + 1) as f32;
    let vector = |toks: &[String]| -> HashMap<String, f32> {
        let mut tf: HashMap<&str, f32> = HashMap::new();
        for t in toks {
            *tf.entry(t.as_str()).or_default() += 1.0;
        }
        let mut v: HashMap<String, f32> = tf
            .into_iter()
            .map(|(w, c)| (w.to_string(), (1.0 + c.ln()) * (n / df.get(w).copied().unwrap_or(1.0)).ln()))
            .collect();
        let norm = v.values().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.values_mut() {
                *x /= norm;
            }
        }
        v
    };
    let q = vector(&query);
    let mut best: Option<Nearest> = None;
    for (doc, toks) in docs.iter().zip(&doc_toks) {
        if doc.table != table || Some(doc.id) == exclude {
            continue;
        }
        let d = vector(toks);
        let (small, large) = if q.len() <= d.len() { (&q, &d) } else { (&d, &q) };
        let cos: f32 = small.iter().map(|(w, x)| x * large.get(w).copied().unwrap_or(0.0)).sum();
        if best.as_ref().map_or(true, |b| cos > b.cosine) {
            best = Some(Nearest { table: doc.table, id: doc.id, label: doc.label.clone(), cosine: cos });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(table: &'static str, id: i64, text: &str) -> Doc {
        Doc { table, id, label: text.chars().take(30).collect(), text: text.into(), at: String::new(), superseded_by: None }
    }

    #[test]
    fn tokens_keep_identifiers_and_drop_short_and_stop_words() {
        let t = tokens("Calling Canvas::run() on the EditorObject in index-sources.json is wrong");
        assert!(t.contains(&"canvas::run".to_string()), "{t:?}");
        assert!(t.contains(&"editorobject".to_string()));
        assert!(t.contains(&"sources.json".to_string()), "{t:?}");
        assert!(!t.iter().any(|w| w == "the" || w == "on" || w == "is"));
    }

    #[test]
    fn a_restatement_is_a_duplicate_and_an_unrelated_entry_is_not() {
        let docs = vec![
            doc("anti_patterns", 1, "design_canvas pan or zoom mutates scene.canvas after closures capture an immutable borrow of pan_x pan_y"),
            doc("anti_patterns", 2, "WGSL shader strings are validated by naga at pipeline creation, not by cargo build"),
            doc("patterns", 3, "design_canvas pan zoom borrow"),
        ];
        let same = "design_canvas pan or zoom mutates scene.canvas after closures capture an immutable borrow of pan_x pan_y";
        let hit = nearest_in(&docs, "anti_patterns", same, None).unwrap();
        assert_eq!(hit.id, 1);
        assert!(hit.cosine >= DUPLICATE_COSINE, "{}", hit.cosine);
        // Only the same table is compared, and the entry itself can be excluded.
        let other = nearest_in(&docs, "anti_patterns", same, Some(1)).unwrap();
        assert_eq!(other.id, 2);
        assert!(other.cosine < RELATED_COSINE, "{}", other.cosine);
    }

    #[test]
    fn empty_text_has_no_neighbour() {
        let docs = vec![doc("patterns", 1, "anything at all")];
        assert!(nearest_in(&docs, "patterns", "a an", None).is_none());
    }
}
