//! Compare at write, remind at use, dispute on events
//! (docs/self-learning-loop-2026-09-30.md, L2).
//!
//! Similarity finds restatements, not contradictions: replayed over the store's
//! history, the real corrections scored cosine 0.02-0.34 against what they
//! replaced, and a 30-pair sample of related neighbours held one contradiction
//! among 21 compatible pairs. So similarity never raises an alarm here:
//!
//! - A new entry whose nearest older entry reads alike (cosine 0.25-0.9) opens a
//!   PAIR. The author hears of it on the next prompt, while they still know why
//!   they wrote it, and anyone served either entry sees one line until someone
//!   says which it is: duplicate, refinement, conflict or compatible.
//! - An entry is DISPUTED by events: a user_right challenge naming it, a wall
//!   it cites moving, its failure coming back after its fix was delivered. A
//!   disputed entry is still served, with a warning, never hidden.
//! - A disagreement is settled by a fact -- a measurement, or a vendor document
//!   or paper with its date -- exactly as walls are. Merging a duplicate needs
//!   none.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};

use crate::knowledge_sim::{self, Corpus, DUPLICATE_COSINE, RELATED_COSINE};
use crate::memory::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS loop_pairs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    new_id      INTEGER NOT NULL,
    old_id      INTEGER NOT NULL,
    cosine      REAL NOT NULL,
    status      TEXT NOT NULL DEFAULT 'open',
    verdict     TEXT NOT NULL DEFAULT '',
    notified_at INTEGER,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    resolved_at INTEGER,
    UNIQUE(kind, new_id, old_id)
);
CREATE TABLE IF NOT EXISTS loop_disputes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    entry_id    INTEGER NOT NULL,
    reason      TEXT NOT NULL,
    source      TEXT NOT NULL,
    raised_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    settled_at  INTEGER,
    settlement  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_loop_disputes_entry ON loop_disputes(kind, entry_id);
";

/// Pair notices riding on one prompt.
pub const NOTICES_PER_PROMPT: i64 = 2;
/// Reconciliation lines added to one retrieval answer.
pub const SERVING_LINES: usize = 3;
/// A pair older than this is no longer news to its author.
const NOTICE_WINDOW_SECS: i64 = 6 * 3600;

fn check_kind(kind: &str) -> Result<()> {
    if !matches!(kind, "patterns" | "anti_patterns") {
        bail!("unknown knowledge table `{kind}` (patterns or anti_patterns)");
    }
    Ok(())
}

fn short(kind: &str) -> &'static str {
    if kind == "patterns" { "pattern" } else { "ap" }
}

fn label(store: &Store, kind: &str, id: i64) -> String {
    let col = if kind == "patterns" { "name" } else { "description" };
    let text: String = store
        .conn()
        .query_row(&format!("SELECT {col} FROM {kind} WHERE id = ?1"), params![id], |r| r.get(0))
        .unwrap_or_default();
    let mut t: String = text.chars().take(60).collect();
    if text.chars().count() > 60 {
        t.push('…');
    }
    t
}

fn written(store: &Store, kind: &str, id: i64) -> String {
    let col = if kind == "patterns" { "approved_at" } else { "added_at" };
    store
        .conn()
        .query_row(&format!("SELECT substr({col}, 1, 10) FROM {kind} WHERE id = ?1"), params![id], |r| r.get(0))
        .unwrap_or_default()
}

// ── pairs ────────────────────────────────────────────────────────────────────

/// Open a pair when a new entry's nearest older entry reads alike without
/// restating it. Returns the pair id.
pub fn open_pair(store: &Store, kind: &str, new_id: i64, old_id: i64, cosine: f32, notified: bool) -> Result<Option<i64>> {
    check_kind(kind)?;
    let n = store.conn().execute(
        "INSERT OR IGNORE INTO loop_pairs (kind, new_id, old_id, cosine, notified_at)
         VALUES (?1, ?2, ?3, ?4, CASE WHEN ?5 THEN unixepoch() END)",
        params![kind, new_id, old_id, cosine as f64, notified],
    )?;
    Ok((n > 0).then(|| store.conn().last_insert_rowid()))
}

/// Called after a new pattern or anti-pattern is stored: pair it with its
/// nearest older live entry if that one reads alike.
pub fn pair_new_entry(store: &Store, kind: &str, new_id: i64, text: &str) -> Option<i64> {
    let near = knowledge_sim::nearest(store, kind, text, Some(new_id)).ok()??;
    if near.cosine < RELATED_COSINE || near.cosine >= DUPLICATE_COSINE {
        return None;
    }
    open_pair(store, kind, new_id, near.id, near.cosine, false).ok()?
}

/// The notices for pairs opened in the last few hours that nobody has heard
/// about yet, marked as delivered. Rides on the next UserPromptSubmit.
pub fn pending_notices(store: &Store) -> String {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT id, kind, new_id, old_id, cosine FROM loop_pairs
         WHERE status = 'open' AND notified_at IS NULL AND created_at >= unixepoch() - ?1
         ORDER BY id LIMIT ?2",
    ) else {
        return String::new();
    };
    let rows: Vec<(i64, String, i64, i64, f64)> = stmt
        .query_map(params![NOTICE_WINDOW_SECS, NOTICES_PER_PROMPT], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (pair, kind, new_id, old_id, cos) in rows {
        let _ = store.conn().execute("UPDATE loop_pairs SET notified_at = unixepoch() WHERE id = ?1", params![pair]);
        let s = short(&kind);
        out.push(format!(
            "[cortex] Your new {s}:{new_id} (\"{}\") reads close to {s}:{old_id} (\"{}\", {}; cosine {cos:.2}). \
             Say which it is: resolve_pair(kind=\"{kind}\", new_id={new_id}, old_id={old_id}, verdict=duplicate|refinement|conflict|compatible).",
            label(store, &kind, new_id),
            label(store, &kind, old_id),
            written(store, &kind, old_id),
        ));
    }
    out.join("\n")
}

/// Record a verdict on a pair. Duplicates and refinements retire the older
/// entry; a conflict disputes both until a fact names the winner; compatible
/// closes the pair.
pub fn resolve_pair(
    store: &Store,
    kind: &str,
    new_id: i64,
    old_id: i64,
    verdict: &str,
    winner: Option<i64>,
    fact: Option<&str>,
) -> Result<String> {
    check_kind(kind)?;
    let s = short(kind);
    let close = |v: &str| -> Result<()> {
        store.conn().execute(
            "UPDATE loop_pairs SET status = 'resolved', verdict = ?4, resolved_at = unixepoch()
             WHERE kind = ?1 AND ((new_id = ?2 AND old_id = ?3) OR (new_id = ?3 AND old_id = ?2))",
            params![kind, new_id, old_id, v],
        )?;
        Ok(())
    };
    match verdict {
        "duplicate" | "refinement" => {
            store.supersede(kind, old_id, new_id)?;
            crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
                class: "duplicate",
                target: &format!("{kind}:{old_id}"),
                before: "live",
                after: &format!("superseded by {new_id}"),
                evidence: &format!("resolve_pair: {verdict}"),
                ..Default::default()
            })?;
            close(verdict)?;
            Ok(format!("{s}:{old_id} is retired in favour of {s}:{new_id} ({verdict}); `cortex knowledge restore {s}:{old_id}` undoes it."))
        }
        "compatible" => {
            close(verdict)?;
            Ok(format!("{s}:{new_id} and {s}:{old_id} both stand; the pair is closed."))
        }
        "conflict" => match (winner, fact) {
            (Some(w), Some(f)) => {
                if w != new_id && w != old_id {
                    bail!("the winner must be {new_id} or {old_id}");
                }
                let evidence = check_fact(f)?;
                let loser = if w == new_id { old_id } else { new_id };
                store.supersede(kind, loser, w)?;
                crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
                    class: "conflict",
                    target: &format!("{kind}:{loser}"),
                    before: "live",
                    after: &format!("superseded by {w}"),
                    evidence: &evidence,
                    ..Default::default()
                })?;
                settle_all(store, kind, loser, &format!("lost a conflict to {s}:{w}: {evidence}"))?;
                settle_all(store, kind, w, &format!("won a conflict with {s}:{loser}: {evidence}"))?;
                close("conflict")?;
                Ok(format!("{s}:{loser} is retired; {s}:{w} stands on: {evidence}"))
            }
            _ => {
                for (a, b) in [(new_id, old_id), (old_id, new_id)] {
                    raise_dispute(store, kind, a, &format!("conflicts with {s}:{b}"), "conflict")?;
                }
                store.conn().execute(
                    "UPDATE loop_pairs SET verdict = 'conflict' WHERE kind = ?1 AND new_id = ?2 AND old_id = ?3",
                    params![kind, new_id, old_id],
                )?;
                Ok(format!(
                    "Both are now served as disputed. Settle it with a fact: resolve_pair(kind=\"{kind}\", \
                     new_id={new_id}, old_id={old_id}, verdict=\"conflict\", winner=<id>, \
                     fact=\"measured: <what> @ <where> @ <date>\")."
                ))
            }
        },
        other => bail!("unknown verdict `{other}` (duplicate, refinement, conflict or compatible)"),
    }
}

/// `kind: text @ source @ date`, and it must be a fact: measured, or a dated
/// vendor-doc or paper. Returns it normalised.
fn check_fact(line: &str) -> Result<String> {
    let Some(e) = crate::walls::parse_evidence_line(line) else {
        bail!("a fact is written `measured: <what> @ <where> @ <date>` (or vendor-doc / paper with a date)");
    };
    if !crate::walls::is_fact(&e) {
        bail!("`{}` evidence is not a fact: only a measurement, or a vendor-doc or paper with its date, can settle this", e.kind);
    }
    Ok(format!("{}: {} @ {} @ {}", e.kind, e.text, e.source, e.date))
}

// ── disputes ─────────────────────────────────────────────────────────────────

/// Mark an entry disputed. One open dispute per entry and reason.
pub fn raise_dispute(store: &Store, kind: &str, id: i64, reason: &str, source: &str) -> Result<Option<i64>> {
    check_kind(kind)?;
    let open: bool = store.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM loop_disputes WHERE kind = ?1 AND entry_id = ?2 AND reason = ?3 AND settled_at IS NULL)",
        params![kind, id, reason],
        |r| r.get(0),
    )?;
    if open {
        return Ok(None);
    }
    store.conn().execute(
        "INSERT INTO loop_disputes (kind, entry_id, reason, source) VALUES (?1, ?2, ?3, ?4)",
        params![kind, id, reason, source],
    )?;
    let dispute = store.conn().last_insert_rowid();
    crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
        class: "dispute",
        target: &format!("{kind}:{id}"),
        after: reason,
        evidence: source,
        ..Default::default()
    })?;
    Ok(Some(dispute))
}

fn settle_all(store: &Store, kind: &str, id: i64, settlement: &str) -> Result<usize> {
    Ok(store.conn().execute(
        "UPDATE loop_disputes SET settled_at = unixepoch(), settlement = ?3
         WHERE kind = ?1 AND entry_id = ?2 AND settled_at IS NULL",
        params![kind, id, settlement],
    )?)
}

pub fn open_disputes(store: &Store, kind: &str, id: i64) -> Vec<String> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT reason FROM loop_disputes WHERE kind = ?1 AND entry_id = ?2 AND settled_at IS NULL ORDER BY id",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![kind, id], |r| r.get(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Settle every open dispute on an entry with a fact: it `holds`, or it was
/// `wrong` and is taken out of service.
pub fn settle_dispute(store: &Store, kind: &str, id: i64, verdict: &str, fact: &str) -> Result<String> {
    check_kind(kind)?;
    let evidence = check_fact(fact)?;
    let s = short(kind);
    if open_disputes(store, kind, id).is_empty() {
        bail!("{s}:{id} has no open dispute");
    }
    match verdict {
        "holds" => {
            let n = settle_all(store, kind, id, &format!("holds: {evidence}"))?;
            Ok(format!("{s}:{id} holds; {n} dispute(s) settled on: {evidence}"))
        }
        "wrong" => {
            crate::loop_ledger::retract(store, kind, id, &format!("dispute settled wrong: {evidence}"), "")?;
            let n = settle_all(store, kind, id, &format!("wrong: {evidence}"))?;
            Ok(format!("{s}:{id} is retracted; {n} dispute(s) settled on: {evidence}"))
        }
        other => bail!("unknown verdict `{other}` (holds or wrong)"),
    }
}

/// A moved or retired wall disputes every live entry that cites it.
pub fn dispute_entries_citing_wall(store: &Store, wall_id: i64, status: &str) -> Result<usize> {
    let mut n = 0;
    for (kind, text_cols) in [("anti_patterns", "description || ' ' || wrong || ' ' || correct"), ("patterns", "name || ' ' || intent || ' ' || body")] {
        let ids: Vec<i64> = {
            let mut stmt = store.conn().prepare(&format!(
                "SELECT id FROM {kind} WHERE superseded_by IS NULL
                   AND (lower({text_cols}) LIKE ?1 OR lower({text_cols}) LIKE ?2)"
            ))?;
            let rows = stmt.query_map(params![format!("%wall #{wall_id}%"), format!("%wall {wall_id} %")], |r| r.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for id in ids {
            if raise_dispute(store, kind, id, &format!("it cites wall #{wall_id}, which is now {status}"), "wall")?.is_some() {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// A trap delivered for a failure in an EARLIER session, and the same failure
/// is back: its fix did not hold, or was not the fix. Disputes the trap and
/// returns a line for the delivery. Same-session repeats are the ordinary
/// iterate-until-fixed loop and prove nothing.
pub fn recurrence_after_delivery(store: &Store, session_id: &str, key: &str, ap_id: i64) -> Option<String> {
    let earlier: i64 = store
        .conn()
        .query_row(
            "SELECT pushed_at FROM push_log
             WHERE mechanism = 'failure_recall' AND key = ?1 AND anti_pattern_id = ?2
               AND session_id != ?3 AND pushed_at < unixepoch() - 3600
             ORDER BY pushed_at DESC LIMIT 1",
            params![key, ap_id, session_id],
            |r| r.get(0),
        )
        .ok()?;
    let date = chrono::DateTime::from_timestamp(earlier, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    raise_dispute(
        store,
        "anti_patterns",
        ap_id,
        &format!("its fix was delivered for this failure on {date} and the failure came back"),
        "recurrence",
    )
    .ok()?;
    Some(format!(
        "⚠ This trap was delivered for the same failure in an earlier session ({date}), and the failure came back: \
         its fix may be wrong or incomplete. Verify it, then settle_dispute(kind=\"anti_patterns\", id={ap_id}, \
         verdict=holds|wrong, fact=\"measured: ... @ ... @ <date>\")."
    ))
}

// ── serving ──────────────────────────────────────────────────────────────────

/// Lines for the entries a retrieval answer expanded: open disputes first, then
/// unreconciled pairs. At most `SERVING_LINES`; empty when there is nothing.
pub fn serving_notes(store: &Store, kind: &str, ids: &[i64]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    let s = short(kind);
    let mut lines = Vec::new();
    for &id in ids {
        for reason in open_disputes(store, kind, id) {
            lines.push(format!(
                "- ⚠ {s}:{id} is disputed ({reason}): verify it before relying on it; settle_dispute(kind=\"{kind}\", id={id}, verdict=holds|wrong, fact=...)"
            ));
        }
    }
    for &id in ids {
        let pairs: Vec<(i64, i64)> = store
            .conn()
            .prepare("SELECT new_id, old_id FROM loop_pairs WHERE kind = ?1 AND status = 'open' AND (new_id = ?2 OR old_id = ?2)")
            .and_then(|mut stmt| {
                let rows = stmt.query_map(params![kind, id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        for (new_id, old_id) in pairs {
            let other = if new_id == id { old_id } else { new_id };
            lines.push(format!(
                "- ↔ {s}:{id} reads close to {s}:{other} (\"{}\") and they were never reconciled: if they disagree, verify, then resolve_pair(kind=\"{kind}\", new_id={new_id}, old_id={old_id}, verdict=...)",
                label(store, kind, other)
            ));
        }
    }
    lines.dedup();
    if lines.is_empty() {
        return String::new();
    }
    lines.truncate(SERVING_LINES);
    format!("\n## Not yet reconciled\n{}\n", lines.join("\n"))
}

// ── the history: seeding and the backtest ────────────────────────────────────

/// Pair every live entry with its nearest OLDER live entry where they read
/// alike: the pairs the loop would have opened had it been running. Marked as
/// notified, so the backlog is shown where the entries are used, not pushed.
pub fn seed_pairs(store: &Store) -> Result<usize> {
    let corpus = Corpus::build(knowledge_sim::live_docs(store)?);
    let mut n = 0;
    for i in 0..corpus.docs.len() {
        let me = &corpus.docs[i];
        let Some((j, cos)) = corpus.nearest_to(i, |_, d| d.table == me.table && d.at < me.at) else { continue };
        if (RELATED_COSINE..DUPLICATE_COSINE).contains(&cos) {
            if open_pair(store, me.table, me.id, corpus.docs[j].id, cos, true)?.is_some() {
                n += 1;
            }
        }
    }
    Ok(n)
}

pub struct Backtest {
    pub writes: usize,
    pub flagged_duplicate: usize,
    pub flagged_related: usize,
    pub supersedes: Vec<(String, i64, i64, &'static str, f32)>,
    pub pair_found: Option<f32>,
}

/// Replay the rules over the whole history, oldest first: for each write, its
/// nearest earlier entry of the same kind, and what the rules would have done.
/// `watch` is a pair to report on (the #37/#38 contradiction).
pub fn backtest(store: &Store, watch: Option<(&str, i64, i64)>) -> Result<Backtest> {
    let corpus = Corpus::build(knowledge_sim::all_docs(store)?);
    let docs = &corpus.docs;
    let mut bt = Backtest { writes: docs.len(), flagged_duplicate: 0, flagged_related: 0, supersedes: Vec::new(), pair_found: None };
    let index_of = |kind: &str, id: i64| docs.iter().position(|d| d.table == kind && d.id == id);
    for i in 0..docs.len() {
        let me = &docs[i];
        let Some((j, cos)) = corpus.nearest_to(i, |j, d| j < i && d.table == me.table) else { continue };
        let same_name = me.table == "patterns" && docs[j].label == me.label;
        if same_name || cos >= DUPLICATE_COSINE {
            bt.flagged_duplicate += 1;
        } else if cos >= RELATED_COSINE {
            bt.flagged_related += 1;
        }
        if let Some((kind, a, b)) = watch {
            if me.table == kind && (me.id == a || me.id == b) {
                let other = if me.id == a { b } else { a };
                if docs[j].id == other {
                    bt.pair_found = Some(cos);
                }
            }
        }
    }
    // Every real supersede: would the rules have caught it when the newer entry was written?
    for (i, d) in docs.iter().enumerate() {
        let Some(new_id) = d.superseded_by.filter(|&n| n > 0) else { continue };
        let Some(ni) = index_of(d.table, new_id) else { continue };
        let Some((best, cos)) = corpus.nearest_to(ni, |j, x| j < ni && x.table == d.table) else { continue };
        let same_name = d.table == "patterns" && d.label == docs[ni].label;
        let caught = if same_name || (best == i && cos >= DUPLICATE_COSINE) {
            "merged"
        } else if best == i && cos >= RELATED_COSINE {
            "paired"
        } else {
            "missed"
        };
        bt.supersedes.push((d.table.to_string(), d.id, new_id, caught, corpus.cosine(i, ni)));
    }
    Ok(bt)
}

/// Open pairs and disputes, for `cortex knowledge pairs`.
pub fn counts(store: &Store) -> (i64, i64, i64) {
    let one = |sql: &str| -> i64 { store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    (
        one("SELECT COUNT(*) FROM loop_pairs WHERE status = 'open'"),
        one("SELECT COUNT(*) FROM loop_pairs WHERE status = 'resolved'"),
        one("SELECT COUNT(*) FROM loop_disputes WHERE settled_at IS NULL"),
    )
}

pub fn pair_exists(store: &Store, kind: &str, a: i64, b: i64) -> bool {
    store
        .conn()
        .query_row(
            "SELECT id FROM loop_pairs WHERE kind = ?1 AND ((new_id = ?2 AND old_id = ?3) OR (new_id = ?3 AND old_id = ?2))",
            params![kind, a, b],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> crate::test_support::TempStore {
        crate::test_support::TempStore::new("reconcile").unwrap()
    }

    fn ap(s: &Store, description: &str, at: &str) -> i64 {
        s.conn()
            .execute(
                "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at) VALUES (?1, 'w', 'c', '[]', ?2)",
                params![description, at],
            )
            .unwrap();
        s.conn().last_insert_rowid()
    }

    #[test]
    fn a_new_pair_is_announced_once_and_shown_where_it_is_used() {
        let s = store();
        let a = ap(&s, "gif frames must be composited onto a running canvas", "2026-05-14T03:47:00+00:00");
        let b = ap(&s, "gif frames should be extracted without overlay compositing", "2026-05-14T21:33:00+00:00");
        open_pair(&s, "anti_patterns", b, a, 0.25, false).unwrap();
        let notice = pending_notices(&s);
        assert!(notice.contains(&format!("ap:{b}")) && notice.contains("resolve_pair"), "{notice}");
        assert!(pending_notices(&s).is_empty(), "announced once");
        let notes = serving_notes(&s, "anti_patterns", &[a]);
        assert!(notes.contains("never reconciled"), "{notes}");
        assert!(serving_notes(&s, "anti_patterns", &[999]).is_empty());

        let out = resolve_pair(&s, "anti_patterns", b, a, "conflict", None, None).unwrap();
        assert!(out.contains("disputed"), "{out}");
        let notes = serving_notes(&s, "anti_patterns", &[a, b]);
        assert!(notes.matches("disputed").count() >= 2, "{notes}");
        // An opinion does not settle it; a measurement does.
        assert!(resolve_pair(&s, "anti_patterns", b, a, "conflict", Some(a), Some("inferred: I think so @ memory @ 2026-09-30")).is_err());
        let out = resolve_pair(&s, "anti_patterns", b, a, "conflict", Some(a), Some("measured: decoder yields delta frames @ gif crate 0.13 test @ 2026-09-30")).unwrap();
        assert!(out.contains(&format!("ap:{b} is retired")), "{out}");
        assert!(open_disputes(&s, "anti_patterns", a).is_empty());
        assert!(s.all_anti_patterns().unwrap().iter().all(|x| x.id != Some(b)));
    }

    #[test]
    fn duplicates_merge_without_a_fact_and_compatible_just_closes() {
        let s = store();
        let a = ap(&s, "old wording", "2026-01-01T00:00:00+00:00");
        let b = ap(&s, "new wording", "2026-02-01T00:00:00+00:00");
        let c = ap(&s, "unrelated", "2026-03-01T00:00:00+00:00");
        open_pair(&s, "anti_patterns", b, a, 0.5, true).unwrap();
        open_pair(&s, "anti_patterns", c, b, 0.3, true).unwrap();
        resolve_pair(&s, "anti_patterns", b, a, "duplicate", None, None).unwrap();
        assert!(s.all_anti_patterns().unwrap().iter().all(|x| x.id != Some(a)));
        resolve_pair(&s, "anti_patterns", c, b, "compatible", None, None).unwrap();
        assert_eq!(counts(&s), (0, 2, 0));
        assert!(resolve_pair(&s, "anti_patterns", c, b, "maybe", None, None).is_err());
    }

    #[test]
    fn a_dispute_is_raised_once_and_settled_only_by_a_fact() {
        let s = store();
        let a = ap(&s, "cite wall #7 as the reason", "2026-01-01T00:00:00+00:00");
        assert_eq!(dispute_entries_citing_wall(&s, 7, "moved").unwrap(), 1);
        assert_eq!(dispute_entries_citing_wall(&s, 7, "moved").unwrap(), 0, "once per reason");
        assert!(settle_dispute(&s, "anti_patterns", a, "holds", "authority: the docs say so @ x @ 2026").is_err());
        let out = settle_dispute(&s, "anti_patterns", a, "wrong", "measured: the limit moved @ headset @ 2026-09-30").unwrap();
        assert!(out.contains("retracted"), "{out}");
        assert!(settle_dispute(&s, "anti_patterns", a, "holds", "measured: x @ y @ 2026-09-30").is_err(), "nothing left to settle");
    }

    #[test]
    fn seeding_pairs_only_related_older_neighbours() {
        let s = store();
        for i in 0..20 {
            ap(&s, &format!("filler lesson {i} about subsystem_{i} handler_{i}"), &format!("2026-01-{:02}T00:00:00+00:00", i + 1));
        }
        let a = ap(&s, "probe blending shows hard seams at the handover between rooms", "2026-05-01T00:00:00+00:00");
        let b = ap(&s, "probe blending seams appear where two rooms hand over the reflection", "2026-06-01T00:00:00+00:00");
        let n = seed_pairs(&s).unwrap();
        assert!(n >= 1, "{n}");
        assert!(pair_exists(&s, "anti_patterns", b, a));
        assert!(pending_notices(&s).is_empty(), "a seeded backlog is not pushed at anyone");
    }
}
