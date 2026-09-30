//! The self-learning loop's ledger (docs/self-learning-loop-2026-09-30.md, L0-L1).
//!
//! The loop commits knowledge without asking a person first. That was earned:
//! 99.3-100% of markers that reached a closeout were committed anyway, and the
//! per-task approval lost 123 of 411 markers across compactions. What replaces
//! the approval is kept here:
//!
//! - every automated change is a row, with what it replaced and why, so any
//!   of them can be undone and none of them is silent;
//! - a person audits a random sample instead of every item, and the sample is
//!   the loop's measured precision;
//! - when the audit says the loop is wrong too often, automatic commit switches
//!   itself off, and knowledge waits for approval again;
//! - evaluators are registered with a hash, so a change can never be judged by
//!   something that was edited after it was registered.
//!
//! Nothing is deleted. A retracted entry keeps its row with `superseded_by = 0`,
//! which every serving path already filters out.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::memory::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS loop_changes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    class       TEXT NOT NULL,
    target      TEXT NOT NULL,
    before      TEXT NOT NULL DEFAULT '',
    after       TEXT NOT NULL DEFAULT '',
    evidence    TEXT NOT NULL DEFAULT '',
    evaluator   TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'live',
    session_id  TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS idx_loop_changes_class ON loop_changes(class, status);
CREATE INDEX IF NOT EXISTS idx_loop_changes_target ON loop_changes(target);
CREATE TABLE IF NOT EXISTS loop_audits (
    change_id   INTEGER PRIMARY KEY,
    verdict     TEXT NOT NULL,
    note        TEXT NOT NULL DEFAULT '',
    audited_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS loop_evaluators (
    name          TEXT PRIMARY KEY,
    paths         TEXT NOT NULL,
    sha256        TEXT NOT NULL,
    registered_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS capture_offsets (
    transcript  TEXT PRIMARY KEY,
    byte_offset INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
";

/// Knowledge commits without per-item approval unless this meta key is "0".
pub const AUTO_COMMIT_KEY: &str = "loop.auto_commit";
/// The audit window, and how many entries in it may be judged wrong or useless
/// before automatic commit switches itself off (plan L1, acceptance 3).
pub const AUDIT_WINDOW: i64 = 20;
pub const AUDIT_MAX_BAD: i64 = 3;
/// Entries shown per audit round.
pub const AUDIT_SAMPLE: i64 = 5;

/// Change classes an audit can judge: knowledge that entered the store
/// without a person reading it first.
const AUDITABLE: &str = "('entry', 'backfill')";

pub fn auto_commit_enabled(store: &Store) -> bool {
    !matches!(store.get_meta(AUTO_COMMIT_KEY).ok().flatten().as_deref(), Some("0"))
}

/// Switch automatic commit, recorded like any other change.
pub fn set_auto_commit(store: &Store, on: bool, why: &str) -> Result<i64> {
    let before = if auto_commit_enabled(store) { "on" } else { "off" };
    store.set_meta(AUTO_COMMIT_KEY, if on { "1" } else { "0" })?;
    record(
        store,
        &NewChange {
            class: "switch",
            target: AUTO_COMMIT_KEY,
            before,
            after: if on { "on" } else { "off" },
            evidence: why,
            ..Default::default()
        },
    )
}

#[derive(Default)]
pub struct NewChange<'a> {
    pub class: &'a str,
    pub target: &'a str,
    pub before: &'a str,
    pub after: &'a str,
    pub evidence: &'a str,
    pub evaluator: &'a str,
    pub session_id: &'a str,
    /// Empty means `live`.
    pub status: &'a str,
}

pub fn record(store: &Store, c: &NewChange) -> Result<i64> {
    store.conn().execute(
        "INSERT INTO loop_changes (class, target, before, after, evidence, evaluator, status, session_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(NULLIF(?7, ''), 'live'), ?8)",
        params![c.class, c.target, c.before, c.after, c.evidence, c.evaluator, c.status, c.session_id],
    )?;
    Ok(store.conn().last_insert_rowid())
}

pub fn set_status(store: &Store, id: i64, status: &str) -> Result<()> {
    store.conn().execute(
        "UPDATE loop_changes SET status = ?2, updated_at = unixepoch() WHERE id = ?1",
        params![id, status],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Change {
    pub id: i64,
    pub class: String,
    pub target: String,
    pub before: String,
    pub after: String,
    pub evidence: String,
    pub status: String,
    pub created_at: i64,
}

fn change_from_row(r: &rusqlite::Row) -> rusqlite::Result<Change> {
    Ok(Change {
        id: r.get(0)?,
        class: r.get(1)?,
        target: r.get(2)?,
        before: r.get(3)?,
        after: r.get(4)?,
        evidence: r.get(5)?,
        status: r.get(6)?,
        created_at: r.get(7)?,
    })
}

const CHANGE_COLS: &str = "id, class, target, before, after, evidence, status, created_at";

pub fn recent(store: &Store, limit: i64) -> Result<Vec<Change>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT {CHANGE_COLS} FROM loop_changes ORDER BY id DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], change_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get(store: &Store, id: i64) -> Result<Option<Change>> {
    Ok(store
        .conn()
        .query_row(
            &format!("SELECT {CHANGE_COLS} FROM loop_changes WHERE id = ?1"),
            params![id],
            change_from_row,
        )
        .optional()?)
}

/// Counts of changes by class and status, for the scoreboard.
pub fn counts(store: &Store) -> Vec<(String, String, i64)> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT class, status, COUNT(*) FROM loop_changes GROUP BY class, status ORDER BY class, status",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

// ── the audit ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditVerdict {
    Right,
    Wrong,
    Useless,
}

impl AuditVerdict {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "r" | "right" | "ok" | "good" => Some(Self::Right),
            "w" | "wrong" | "bad" => Some(Self::Wrong),
            "u" | "useless" | "noise" => Some(Self::Useless),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Wrong => "wrong",
            Self::Useless => "useless",
        }
    }

    fn is_bad(&self) -> bool {
        !matches!(self, Self::Right)
    }
}

/// A random sample of automatically committed entries nobody has audited yet.
pub fn audit_sample(store: &Store, n: i64) -> Result<Vec<Change>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT {CHANGE_COLS} FROM loop_changes c
         WHERE c.class IN {AUDITABLE} AND c.status = 'live'
           AND NOT EXISTS (SELECT 1 FROM loop_audits a WHERE a.change_id = c.id)
         ORDER BY RANDOM() LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![n], change_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Audited so far, and how many of the most recent `AUDIT_WINDOW` were bad.
pub fn audit_stats(store: &Store) -> (i64, i64) {
    let total: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM loop_audits", [], |r| r.get(0))
        .unwrap_or(0);
    let bad: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM (SELECT verdict FROM loop_audits
                                   ORDER BY audited_at DESC, change_id DESC LIMIT ?1)
             WHERE verdict != 'right'",
            params![AUDIT_WINDOW],
            |r| r.get(0),
        )
        .unwrap_or(0);
    (total, bad)
}

/// Live automatically committed entries nobody has audited.
pub fn unaudited(store: &Store) -> i64 {
    store
        .conn()
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM loop_changes c
                 WHERE c.class IN {AUDITABLE} AND c.status = 'live'
                   AND NOT EXISTS (SELECT 1 FROM loop_audits a WHERE a.change_id = c.id)"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

/// Record a verdict on one automatically committed entry. A wrong or useless
/// entry is retracted on the spot; if the window now holds more bad verdicts
/// than the limit, automatic commit turns itself off and says so.
pub fn record_audit(store: &Store, change_id: i64, verdict: AuditVerdict, note: &str) -> Result<String> {
    let Some(change) = get(store, change_id)? else {
        bail!("no loop change #{change_id}");
    };
    if !matches!(change.class.as_str(), "entry" | "backfill") {
        bail!("loop change #{change_id} is a `{}`, not an entry that can be audited", change.class);
    }
    store.conn().execute(
        "INSERT OR REPLACE INTO loop_audits (change_id, verdict, note, audited_at)
         VALUES (?1, ?2, ?3, unixepoch())",
        params![change_id, verdict.as_str(), note],
    )?;
    let mut out = format!("#{change_id} {}: {}", change.target, verdict.as_str());
    if verdict.is_bad() && change.status == "live" {
        if let Some((table, id)) = parse_entry_ref(&change.target) {
            match retract(store, table, id, &format!("audit: {}", verdict.as_str()), "") {
                Ok(_) => out.push_str(" -> retracted"),
                Err(e) => out.push_str(&format!(" (not retracted: {e})")),
            }
        }
        set_status(store, change_id, "invalidated")?;
    }
    if let Some(kill) = check_kill(store)? {
        out.push('\n');
        out.push_str(&kill);
    }
    Ok(out)
}

/// The kill criterion: more than `AUDIT_MAX_BAD` bad verdicts in the last
/// `AUDIT_WINDOW` audits switches automatic commit off. Fires once; switching
/// it back on is a person's decision.
pub fn check_kill(store: &Store) -> Result<Option<String>> {
    let (_, bad) = audit_stats(store);
    if bad > AUDIT_MAX_BAD && auto_commit_enabled(store) {
        let why = format!(
            "{bad} of the last {AUDIT_WINDOW} audited entries were judged wrong or useless (limit {AUDIT_MAX_BAD})"
        );
        set_auto_commit(store, false, &why)?;
        return Ok(Some(format!(
            "Automatic commit is now OFF: {why}. Markers are still captured, and wait \
             for approval (closeout with inline_approve). `cortex knowledge auto-commit on` \
             turns it back on."
        )));
    }
    Ok(None)
}

// ── retract and restore ──────────────────────────────────────────────────────

/// `ap:12`, `anti_patterns:12`, `pattern:3`, `patterns:3`.
pub fn parse_entry_ref(s: &str) -> Option<(&'static str, i64)> {
    let (kind, id) = s.trim().split_once(':')?;
    let id: i64 = id.trim().trim_start_matches('#').parse().ok()?;
    let table = match kind.trim().to_lowercase().as_str() {
        "ap" | "anti_pattern" | "anti_patterns" | "anti-pattern" => "anti_patterns",
        "pattern" | "patterns" | "pat" => "patterns",
        _ => return None,
    };
    Some((table, id))
}

fn check_table(table: &str) -> Result<()> {
    if !matches!(table, "patterns" | "anti_patterns") {
        bail!("unknown knowledge table `{table}`");
    }
    Ok(())
}

/// Take a live entry out of every serving path, keeping its row.
pub fn retract(store: &Store, table: &str, id: i64, reason: &str, session_id: &str) -> Result<i64> {
    check_table(table)?;
    let n = store.conn().execute(
        &format!("UPDATE {table} SET superseded_by = 0 WHERE id = ?1 AND superseded_by IS NULL"),
        params![id],
    )?;
    if n == 0 {
        bail!("no live {table} row with id {id}");
    }
    let target = format!("{table}:{id}");
    // The change that brought it in is no longer live.
    store.conn().execute(
        &format!(
            "UPDATE loop_changes SET status = 'invalidated', updated_at = unixepoch()
             WHERE target = ?1 AND class IN {AUDITABLE} AND status = 'live'"
        ),
        params![target],
    )?;
    record(
        store,
        &NewChange {
            class: "retract",
            target: &target,
            before: "live",
            after: "retracted",
            evidence: reason,
            session_id,
            ..Default::default()
        },
    )
}

/// Retract every live entry a class of change brought in (`backfill`, `entry`):
/// the group undo. Returns what was retracted and what cannot be (prefs notes,
/// corrections, ADRs and walls are not served by the retract filter).
pub fn retract_class(store: &Store, class: &str, reason: &str) -> Result<(Vec<String>, Vec<String>)> {
    let targets: Vec<(i64, String)> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT id, target FROM loop_changes WHERE class = ?1 AND status = 'live' ORDER BY id")?;
        let rows = stmt.query_map(params![class], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let (mut done, mut skipped) = (Vec::new(), Vec::new());
    for (_, target) in targets {
        match parse_entry_ref(&target) {
            Some((table, id)) => match retract(store, table, id, reason, "") {
                Ok(_) => done.push(target),
                Err(e) => skipped.push(format!("{target}: {e}")),
            },
            None => skipped.push(format!("{target}: not retractable by the store; remove it by hand")),
        }
    }
    Ok((done, skipped))
}

/// Put a retracted or superseded entry back in service.
pub fn restore(store: &Store, table: &str, id: i64) -> Result<String> {
    check_table(table)?;
    let prev: Option<Option<i64>> = store
        .conn()
        .query_row(&format!("SELECT superseded_by FROM {table} WHERE id = ?1"), params![id], |r| r.get(0))
        .optional()?;
    let Some(prev) = prev else { bail!("no {table} row with id {id}") };
    let Some(prev) = prev else { return Ok(format!("{table}:{id} is already live")) };
    store.conn().execute(&format!("UPDATE {table} SET superseded_by = NULL WHERE id = ?1"), params![id])?;
    let target = format!("{table}:{id}");
    store.conn().execute(
        "UPDATE loop_changes SET status = 'rolled_back', updated_at = unixepoch()
         WHERE id = (SELECT id FROM loop_changes WHERE target = ?1 AND class IN ('retract', 'duplicate')
                     AND status = 'live' ORDER BY id DESC LIMIT 1)",
        params![target],
    )?;
    let before = if prev == 0 { "retracted".to_string() } else { format!("superseded by {prev}") };
    record(
        store,
        &NewChange { class: "restore", target: &target, before: &before, after: "live", ..Default::default() },
    )?;
    Ok(format!("{target} restored (was {before})"))
}

// ── evaluators ───────────────────────────────────────────────────────────────

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> =
            std::fs::read_dir(path)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for e in entries {
            collect_files(&e, out)?;
        }
    } else if path.is_file() {
        out.push(path.to_path_buf());
    } else {
        bail!("evaluator path does not exist: {}", path.display());
    }
    Ok(())
}

/// One hash over every file under `paths`, names included, in a fixed order.
pub fn hash_paths(paths: &[PathBuf]) -> Result<String> {
    let mut files = Vec::new();
    for p in paths {
        collect_files(p, &mut files)?;
    }
    let mut h = Sha256::new();
    for f in files {
        h.update(f.to_string_lossy().as_bytes());
        h.update([0u8]);
        h.update(std::fs::read(&f)?);
        h.update([0u8]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Register (or re-register) what judges a class of change. Re-registering is
/// a person's act: nothing in the loop calls it.
pub fn register_evaluator(store: &Store, name: &str, paths: &[PathBuf]) -> Result<String> {
    let hash = hash_paths(paths)?;
    let list: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    store.conn().execute(
        "INSERT INTO loop_evaluators (name, paths, sha256, registered_at) VALUES (?1, ?2, ?3, unixepoch())
         ON CONFLICT(name) DO UPDATE SET paths = excluded.paths, sha256 = excluded.sha256,
                                         registered_at = excluded.registered_at",
        params![name, serde_json::to_string(&list)?, hash],
    )?;
    Ok(hash)
}

/// `name@hash` if the evaluator is registered and unchanged since; an error
/// naming the difference otherwise. A change is judged only through this.
pub fn evaluator_stamp(store: &Store, name: &str) -> Result<String> {
    let row: Option<(String, String)> = store
        .conn()
        .query_row("SELECT paths, sha256 FROM loop_evaluators WHERE name = ?1", params![name], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((paths, registered)) = row else {
        bail!("evaluator `{name}` is not registered; a person registers it with `cortex knowledge evaluator register`");
    };
    let paths: Vec<PathBuf> = serde_json::from_str::<Vec<String>>(&paths)?.into_iter().map(PathBuf::from).collect();
    let now = hash_paths(&paths)?;
    if now != registered {
        bail!(
            "evaluator `{name}` changed since it was registered ({} -> {}); \
             nothing is judged by it until a person re-registers it",
            &registered[..12],
            &now[..12]
        );
    }
    Ok(format!("{name}@{}", &now[..12]))
}

// ── capture offsets ──────────────────────────────────────────────────────────

pub fn capture_offset(store: &Store, transcript: &Path) -> u64 {
    store
        .conn()
        .query_row(
            "SELECT byte_offset FROM capture_offsets WHERE transcript = ?1",
            params![transcript.to_string_lossy()],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u64)
        .unwrap_or(0)
}

pub fn set_capture_offset(store: &Store, transcript: &Path, offset: u64) -> Result<()> {
    store.conn().execute(
        "INSERT INTO capture_offsets (transcript, byte_offset, updated_at) VALUES (?1, ?2, unixepoch())
         ON CONFLICT(transcript) DO UPDATE SET byte_offset = excluded.byte_offset, updated_at = unixepoch()",
        params![transcript.to_string_lossy(), offset as i64],
    )?;
    Ok(())
}

/// Transcripts capture has already seen, with the offset reached.
pub fn known_transcripts(store: &Store) -> Vec<(PathBuf, u64)> {
    let Ok(mut stmt) = store.conn().prepare("SELECT transcript, byte_offset FROM capture_offsets") else {
        return Vec::new();
    };
    stmt.query_map([], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, i64>(1)?.max(0) as u64)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> crate::test_support::TempStore {
        crate::test_support::TempStore::new("loop").unwrap()
    }

    fn live_ap(store: &Store, description: &str) -> i64 {
        store
            .conn()
            .execute(
                "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at) VALUES (?1, 'w', 'c', '[]', ?2)",
                params![description, chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
        store.conn().last_insert_rowid()
    }

    fn superseded_by(store: &Store, id: i64) -> Option<i64> {
        store.conn().query_row("SELECT superseded_by FROM anti_patterns WHERE id = ?1", params![id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn auto_commit_defaults_on_and_the_switch_is_a_recorded_change() {
        let s = store();
        assert!(auto_commit_enabled(&s));
        set_auto_commit(&s, false, "test").unwrap();
        assert!(!auto_commit_enabled(&s));
        let last = recent(&s, 1).unwrap().remove(0);
        assert_eq!((last.class.as_str(), last.before.as_str(), last.after.as_str()), ("switch", "on", "off"));
    }

    #[test]
    fn retract_hides_an_entry_keeps_the_row_and_restore_brings_it_back() {
        let s = store();
        let id = live_ap(&s, "a trap");
        retract(&s, "anti_patterns", id, "test", "").unwrap();
        assert_eq!(superseded_by(&s, id), Some(0));
        assert!(s.all_anti_patterns().unwrap().iter().all(|a| a.id != Some(id)), "a retracted entry is still served");
        assert!(retract(&s, "anti_patterns", id, "again", "").is_err(), "retracting twice must fail loudly");
        let msg = restore(&s, "anti_patterns", id).unwrap();
        assert!(msg.contains("was retracted"), "{msg}");
        assert_eq!(superseded_by(&s, id), None);
        assert!(s.all_anti_patterns().unwrap().iter().any(|a| a.id == Some(id)));
    }

    #[test]
    fn a_bad_audit_retracts_the_entry_and_too_many_switch_auto_commit_off() {
        let s = store();
        let mut changes = Vec::new();
        for i in 0..6 {
            let id = live_ap(&s, &format!("trap {i}"));
            changes.push((
                id,
                record(&s, &NewChange { class: "entry", target: &format!("anti_patterns:{id}"), ..Default::default() }).unwrap(),
            ));
        }
        assert_eq!(unaudited(&s), 6);
        assert_eq!(audit_sample(&s, AUDIT_SAMPLE).unwrap().len(), 5);

        record_audit(&s, changes[0].1, AuditVerdict::Right, "").unwrap();
        let out = record_audit(&s, changes[1].1, AuditVerdict::Wrong, "").unwrap();
        assert!(out.contains("retracted"), "{out}");
        assert_eq!(superseded_by(&s, changes[1].0), Some(0));
        assert_eq!(get(&s, changes[1].1).unwrap().unwrap().status, "invalidated");

        for (_, c) in &changes[2..4] {
            record_audit(&s, *c, AuditVerdict::Useless, "").unwrap();
        }
        assert!(auto_commit_enabled(&s), "3 bad of 4 is at the limit, not over it");
        let out = record_audit(&s, changes[4].1, AuditVerdict::Wrong, "").unwrap();
        assert!(out.contains("Automatic commit is now OFF"), "{out}");
        assert!(!auto_commit_enabled(&s));
        assert_eq!(audit_stats(&s), (5, 4));
    }

    #[test]
    fn a_class_of_change_can_be_undone_as_a_group() {
        let s = store();
        let a = live_ap(&s, "backfilled one");
        let b = live_ap(&s, "backfilled two");
        let keep = live_ap(&s, "captured live");
        for (id, class) in [(a, "backfill"), (b, "backfill"), (keep, "entry")] {
            record(&s, &NewChange { class, target: &format!("anti_patterns:{id}"), ..Default::default() }).unwrap();
        }
        record(&s, &NewChange { class: "backfill", target: "prefs.toml", ..Default::default() }).unwrap();
        let (done, skipped) = retract_class(&s, "backfill", "test").unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(skipped.len(), 1, "a prefs note is reported, not silently kept: {skipped:?}");
        assert_eq!(superseded_by(&s, a), Some(0));
        assert_eq!(superseded_by(&s, keep), None, "other classes are untouched");
    }

    #[test]
    fn only_entries_can_be_audited() {
        let s = store();
        let c = record(&s, &NewChange { class: "switch", target: AUTO_COMMIT_KEY, ..Default::default() }).unwrap();
        assert!(record_audit(&s, c, AuditVerdict::Right, "").is_err());
        assert!(record_audit(&s, 9999, AuditVerdict::Right, "").is_err());
    }

    #[test]
    fn entry_refs_parse_both_spellings() {
        assert_eq!(parse_entry_ref("ap:12"), Some(("anti_patterns", 12)));
        assert_eq!(parse_entry_ref("anti_patterns:#12"), Some(("anti_patterns", 12)));
        assert_eq!(parse_entry_ref("pattern:3"), Some(("patterns", 3)));
        assert_eq!(parse_entry_ref("walls:3"), None);
        assert_eq!(parse_entry_ref("ap:x"), None);
    }

    #[test]
    fn an_evaluator_edited_after_registration_is_refused() {
        let dir = std::env::temp_dir().join(format!("cortex-eval-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("labels.json");
        std::fs::write(&file, "[1,2,3]").unwrap();
        let s = store();
        assert!(evaluator_stamp(&s, "cues").is_err(), "unregistered must refuse");
        register_evaluator(&s, "cues", &[dir.clone()]).unwrap();
        let stamp = evaluator_stamp(&s, "cues").unwrap();
        assert!(stamp.starts_with("cues@"), "{stamp}");
        std::fs::write(&file, "[1,2,3,4]").unwrap();
        let err = evaluator_stamp(&s, "cues").unwrap_err().to_string();
        assert!(err.contains("changed since it was registered"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_offsets_round_trip() {
        let s = store();
        let p = Path::new("/tmp/x.jsonl");
        assert_eq!(capture_offset(&s, p), 0);
        set_capture_offset(&s, p, 42).unwrap();
        set_capture_offset(&s, p, 84).unwrap();
        assert_eq!(capture_offset(&s, p), 84);
        assert_eq!(known_transcripts(&s), vec![(p.to_path_buf(), 84)]);
    }
}
