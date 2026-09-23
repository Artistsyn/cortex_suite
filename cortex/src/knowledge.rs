//! Does a stored lesson still describe the code it talks about?
//!
//! Patterns and anti-patterns name code: `Canvas::run`, `SceneRenderer`,
//! `infer_edges`. When that code is renamed, removed or reshaped after the
//! entry was written, the entry may now be wrong - and nothing said so. A
//! `pattern_unit_refs` table was meant to carry the links; it was never written
//! and its one reader was never called, so every entry was served with the same
//! authority however far the code had moved.
//!
//! This computes the links when an entry is SHOWN rather than storing them: the
//! entry's text and `uses` list are scanned for names the index knows, and the
//! change journal (`api_changes`, written by every index run) is asked what
//! happened to those names after the entry was written. Nothing to backfill,
//! nothing to go stale, and the cost is a few indexed lookups for the handful
//! of entries a call expands.
//!
//! It is deliberately conservative. A name counts only if the index knows it
//! (as a live unit or as one the journal saw removed), and only changes AFTER
//! the entry's date are reported - the journal starts empty, so an old entry is
//! flagged only for changes from here on, never for guesses about the past.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

/// A code reference found in an entry: `Type`, or `Type::member`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ref {
    pub name: String,
    pub member: Option<String>,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Distinctive enough to be a code name outside backticks: CamelCase with at
/// least two capitals (`SceneRenderer`, not `The`), or snake_case (`infer_edges`).
fn looks_like_code(word: &str) -> bool {
    if !is_ident(word) || word.len() < 4 {
        return false;
    }
    let caps = word.chars().filter(|c| c.is_uppercase()).count();
    let camel = word.chars().next().is_some_and(|c| c.is_uppercase())
        && caps >= 2
        && word.chars().any(|c| c.is_lowercase());
    let snake = word.contains('_') && word.chars().all(|c| !c.is_uppercase());
    camel || snake
}

/// Turn one path-ish token (`a::B::c`, `B.c()`, `B`) into a Ref.
fn ref_from_path(token: &str) -> Option<Ref> {
    let token = token.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.'));
    let token = token.split('(').next().unwrap_or(token);
    let parts: Vec<&str> = token
        .split(|c| c == ':' || c == '.')
        .filter(|p| !p.is_empty())
        .collect();
    match parts.as_slice() {
        [] => None,
        [one] if is_ident(one) => Some(Ref { name: one.to_string(), member: None }),
        [.., owner, member] if is_ident(owner) && is_ident(member) => {
            // `Type::method` names a member of a type; `module::func` names an item.
            if owner.chars().next().is_some_and(|c| c.is_uppercase()) {
                Some(Ref { name: owner.to_string(), member: Some(member.to_string()) })
            } else {
                Some(Ref { name: member.to_string(), member: None })
            }
        }
        _ => None,
    }
}

/// Every candidate reference in an entry's text and `uses` list.
pub fn candidate_refs(texts: &[&str], uses: &[String]) -> BTreeSet<Ref> {
    let mut out = BTreeSet::new();
    for u in uses {
        // `uses` holds free text too ("cortex/src/cache.rs source_fingerprint/…").
        for tok in u.split(|c: char| c.is_whitespace() || c == '/' || c == ',') {
            if let Some(r) = ref_from_path(tok) {
                out.insert(r);
            }
        }
    }
    for text in texts {
        // Backticked spans are code by the author's own marking.
        for (n, seg) in text.split('`').enumerate() {
            if n % 2 == 1 {
                if let Some(r) = ref_from_path(seg.trim()) {
                    out.insert(r);
                }
                continue;
            }
            for word in seg.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
                if word.contains("::") {
                    if let Some(r) = ref_from_path(word) {
                        out.insert(r);
                    }
                } else if looks_like_code(word) {
                    out.insert(Ref { name: word.to_string(), member: None });
                }
            }
        }
    }
    out
}

fn live(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM code_units WHERE name = ?1 LIMIT 1", params![name], |r| {
        r.get::<_, i64>(0)
    })
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// Does any live unit named `name` still have a member `member`? Names are
/// ambiguous across roots (several `Canvas` types), so one losing a member says
/// nothing while another still has it.
fn live_member(conn: &Connection, name: &str, member: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM code_members m JOIN code_units u ON u.id = m.parent_id
          WHERE u.name = ?1 AND m.name = ?2 LIMIT 1",
        params![name, member],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// Does the index know this name - live, or as something the journal saw go?
fn known(conn: &Connection, name: &str) -> bool {
    let live: Option<i64> = conn
        .query_row("SELECT 1 FROM code_units WHERE name = ?1 LIMIT 1", params![name], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    if live.is_some() {
        return true;
    }
    conn.query_row("SELECT 1 FROM api_changes WHERE name = ?1 LIMIT 1", params![name], |r| {
        r.get::<_, i64>(0)
    })
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// One line saying what changed, after `written`, in the code an entry names -
/// or None when nothing it names has moved.
pub fn drift_note(
    conn: &Connection,
    texts: &[&str],
    uses: &[String],
    written: DateTime<Utc>,
) -> Option<String> {
    if crate::indexer::ensure_tables(conn).is_err() {
        return None;
    }
    let coarse = (written - chrono::Duration::seconds(1)).format("%Y-%m-%dT%H:%M:%S").to_string();
    let mut notes: Vec<String> = Vec::new();
    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    for r in candidate_refs(texts, uses) {
        if notes.len() >= 3 {
            break;
        }
        if !known(conn, &r.name) {
            continue;
        }
        // A bare name that still resolves to a live item is not "removed" just
        // because a same-named item elsewhere was; a member that some live
        // same-named type still has is not gone either.
        let name_live = live(conn, &r.name);
        let member_live = match &r.member {
            Some(m) if name_live => live_member(conn, &r.name, m),
            _ => false,
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT at, change, detail FROM api_changes WHERE name = ?1 AND at >= ?2 ORDER BY id",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map(params![r.name, coarse], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        }) else {
            continue;
        };
        let mut last: Option<(DateTime<Utc>, String)> = None;
        for row in rows.flatten() {
            let (at, change, detail) = row;
            let Ok(at) = DateTime::parse_from_rfc3339(&at) else { continue };
            let at = at.with_timezone(&Utc);
            if at <= written {
                continue;
            }
            let relevant = match (&r.member, change.as_str()) {
                (_, "removed") => !name_live,
                (Some(_), _) if member_live && change != "changed" => false,
                (None, "changed") => true,
                (Some(m), "changed") => detail.split(", ").any(|d| {
                    let d = d.trim_start_matches(|c| c == '+' || c == '-' || c == '~');
                    d.split_whitespace().nth(1) == Some(m.as_str())
                }),
                _ => false, // an addition does not invalidate what was written
            };
            if !relevant {
                continue;
            }
            let what = match (&r.member, change.as_str()) {
                (_, "removed") => "removed".to_string(),
                (Some(m), _) => {
                    let d: Vec<&str> = detail
                        .split(", ")
                        .filter(|d| {
                            d.trim_start_matches(|c| c == '+' || c == '-' || c == '~')
                                .split_whitespace()
                                .nth(1)
                                == Some(m.as_str())
                        })
                        .collect();
                    d.join(", ")
                }
                _ if detail.is_empty() => "changed".to_string(),
                _ => detail.clone(),
            };
            last = Some((at, what));
        }
        if let Some((at, what)) = last {
            let label = match &r.member {
                Some(m) => format!("{}::{m}", r.name),
                None => r.name.clone(),
            };
            if seen_names.insert(label.clone()) {
                notes.push(format!("`{label}` {what} ({})", at.format("%Y-%m-%d")));
            }
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("⚠ code it names changed since it was written: {} - verify before relying on it", notes.join("; ")))
    }
}

/// Every stored entry the journal says has drifted, as (kind, id, title, note).
/// For closeout's review block and `cortex knowledge-drift`.
pub fn drifted_entries(store: &crate::memory::Store) -> Vec<(&'static str, i64, String, String)> {
    let mut out = Vec::new();
    let conn = store.conn();
    let journaled: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_changes", [], |r| r.get(0))
        .unwrap_or(0);
    if journaled == 0 {
        return out;
    }
    if let Ok(patterns) = store.all_patterns() {
        for p in patterns {
            let Some(id) = p.id else { continue };
            if let Some(n) = drift_note(conn, &[&p.intent, &p.body], &p.uses, p.approved_at) {
                out.push(("pattern", id, p.name.clone(), n));
            }
        }
    }
    if let Ok(aps) = store.all_anti_patterns() {
        for a in aps {
            let Some(id) = a.id else { continue };
            if let Some(n) = drift_note(conn, &[&a.description, &a.wrong, &a.correct], &[], a.added_at) {
                out.push(("anti-pattern", id, a.description.chars().take(80).collect(), n));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE code_units (id TEXT, name TEXT);
             CREATE TABLE code_members (parent_id TEXT, kind TEXT, name TEXT);",
        )
        .unwrap();
        crate::indexer::ensure_tables(&c).unwrap();
        c
    }

    fn journal(c: &Connection, at: DateTime<Utc>, name: &str, change: &str, detail: &str) {
        c.execute(
            "INSERT INTO api_changes (at, source_root, unit_id, name, kind, change, detail)
             VALUES (?1, 'src', ?2, ?2, 'struct', ?3, ?4)",
            params![at.to_rfc3339(), name, change, detail],
        )
        .unwrap();
    }

    #[test]
    fn refs_come_from_backticks_paths_and_distinctive_words_only() {
        let r = candidate_refs(&["Call `Canvas::run` from SceneRenderer, not the old way"], &[]);
        assert!(r.contains(&Ref { name: "Canvas".into(), member: Some("run".into()) }));
        assert!(r.contains(&Ref { name: "SceneRenderer".into(), member: None }));
        assert!(!r.iter().any(|x| x.name == "Call" || x.name == "the"), "{r:?}");
    }

    #[test]
    fn a_change_after_the_entry_is_flagged_and_one_before_is_not() {
        let c = db();
        c.execute("INSERT INTO code_units VALUES ('m::Canvas','Canvas')", []).unwrap();
        let written = Utc::now() - chrono::Duration::hours(1);
        journal(&c, written - chrono::Duration::hours(1), "Canvas", "changed", "-method old");
        assert!(drift_note(&c, &["use `Canvas::old`"], &[], written).is_none());
        journal(&c, Utc::now(), "Canvas", "changed", "-method run, +field w");
        let n = drift_note(&c, &["use `Canvas::run`"], &[], written).unwrap();
        assert!(n.contains("`Canvas::run` -method run"), "{n}");
        assert!(!n.contains("field w"), "member ref reports only its member: {n}");
    }

    #[test]
    fn a_member_ref_ignores_changes_to_other_members() {
        let c = db();
        c.execute("INSERT INTO code_units VALUES ('m::Canvas','Canvas')", []).unwrap();
        let written = Utc::now() - chrono::Duration::hours(1);
        journal(&c, Utc::now(), "Canvas", "changed", "+field w");
        assert!(drift_note(&c, &["`Canvas::run` is safe"], &[], written).is_none());
    }

    #[test]
    fn removal_of_one_of_several_same_named_items_is_not_flagged() {
        let c = db();
        c.execute("INSERT INTO code_units VALUES ('b::Canvas','Canvas')", []).unwrap();
        let written = Utc::now() - chrono::Duration::hours(1);
        journal(&c, Utc::now(), "Canvas", "removed", "");
        assert!(drift_note(&c, &["see `Canvas`"], &[], written).is_none());
    }

    #[test]
    fn a_removed_item_is_flagged_even_though_it_is_no_longer_indexed() {
        let c = db();
        let written = Utc::now() - chrono::Duration::hours(1);
        journal(&c, Utc::now(), "OldThing", "removed", "");
        let n = drift_note(&c, &["prefer `OldThing`"], &[], written).unwrap();
        assert!(n.contains("`OldThing` removed"), "{n}");
    }

    #[test]
    fn names_the_index_never_knew_are_ignored() {
        let c = db();
        let written = Utc::now() - chrono::Duration::hours(1);
        assert!(drift_note(&c, &["glam `Mat4::from_cols_array`"], &[], written).is_none());
    }
}
