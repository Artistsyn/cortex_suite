//! The weekly maintenance run (docs/self-learning-loop-2026-09-30.md, L3).
//!
//! Work that needs a model -- judging look-alike pairs, writing the digest --
//! runs in a scheduled fresh session, where context starts at about 70k tokens
//! instead of averaging 531k in a working session, and never in the user's.
//!
//! The judge is calibrated before it is trusted. Its queue mixes pairs a person
//! has labelled with real open pairs, under opaque item numbers, so it cannot
//! tell which answers are being checked. Its verdicts on real pairs act only once
//! it agrees with the labels at >= 90% over >= 20 of them, and only while the
//! label file is unchanged since it was registered (loop_ledger evaluators).
//! Agreement is scored on what a verdict DOES: duplicate and refinement merge,
//! compatible keeps both, conflict disputes both.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde_json::Value;

use crate::memory::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS loop_queue (
    item        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    ref         TEXT NOT NULL,
    issued_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    verdict     TEXT,
    answered_at INTEGER,
    acted       TEXT NOT NULL DEFAULT ''
);
";

/// Where the calibration labels live, relative to the workspace.
pub const LABELS_FILE: &str = ".cortex/corpora/pair_labels.json";
/// The name the label file is registered under as an evaluator.
pub const EVALUATOR: &str = "pair-labels";
pub const CALIBRATION_MIN: i64 = 20;
pub const CALIBRATION_AGREEMENT: f64 = 0.9;
/// Calibration items re-asked each run once calibrated, so drift shows.
const REFRESHER: usize = 5;
const REAL_ITEMS_PER_RUN: usize = 15;
/// Text per entry in the queue.
const ENTRY_CHARS: usize = 450;
/// The first user message of every maintenance run contains this.
pub const MARKER: &str = "cortex-weekly-maintenance";
/// A run over either limit is over budget (plan §7.1).
pub const MAX_CALLS: usize = 30;
pub const MAX_CONTEXT: i64 = 250_000;

fn action(verdict: &str) -> Option<&'static str> {
    match verdict {
        "duplicate" | "refinement" => Some("merge"),
        "compatible" => Some("keep"),
        "conflict" => Some("dispute"),
        _ => None,
    }
}

struct Labelled {
    kind: String,
    older: i64,
    newer: i64,
    label: String,
}

fn load_labels(repo_root: &Path) -> Result<Vec<Labelled>> {
    let path = repo_root.join(LABELS_FILE);
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("no calibration labels at {}", path.display()))?)?;
    Ok(v["pairs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Labelled {
                kind: p["kind"].as_str()?.to_string(),
                older: p["older"].as_i64()?,
                newer: p["newer"].as_i64()?,
                label: p["label"].as_str()?.to_string(),
            })
        })
        .collect())
}

fn entry_text(store: &Store, kind: &str, id: i64) -> String {
    let sql = if kind == "patterns" {
        "SELECT name || ' -- ' || intent || char(10) || body FROM patterns WHERE id = ?1"
    } else {
        "SELECT description || char(10) || 'wrong: ' || wrong || char(10) || 'correct: ' || correct FROM anti_patterns WHERE id = ?1"
    };
    let t: String = store.conn().query_row(sql, params![id], |r| r.get(0)).unwrap_or_default();
    let mut s: String = t.chars().take(ENTRY_CHARS).collect();
    if t.chars().count() > ENTRY_CHARS {
        s.push('…');
    }
    s.replace('\n', " / ")
}

/// Calibration answered so far, and how many agreed with the labels.
pub fn calibration(store: &Store) -> (i64, i64) {
    store
        .conn()
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(acted = 'agree'), 0) FROM loop_queue
             WHERE kind = 'calibration' AND verdict IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0))
}

/// Trusted: enough calibration, enough agreement, labels unchanged.
pub fn calibrated(store: &Store) -> std::result::Result<(), String> {
    let (n, agree) = calibration(store);
    if n < CALIBRATION_MIN {
        return Err(format!("{n} of {CALIBRATION_MIN} calibration answers so far"));
    }
    let rate = agree as f64 / n as f64;
    if rate < CALIBRATION_AGREEMENT {
        return Err(format!("agreement {:.0}% ({agree}/{n}) is under {:.0}%", rate * 100.0, CALIBRATION_AGREEMENT * 100.0));
    }
    crate::loop_ledger::evaluator_stamp(store, EVALUATOR).map(|_| ()).map_err(|e| e.to_string())
}

/// Issue this run's items: calibration pairs mixed with real open pairs, under
/// opaque numbers, texts only.
pub fn queue(store: &Store, repo_root: &Path) -> Result<String> {
    let labels = load_labels(repo_root)?;
    let (answered, _) = calibration(store);
    let asked: std::collections::HashSet<String> = {
        let mut stmt = store.conn().prepare("SELECT ref FROM loop_queue WHERE kind = 'calibration' AND verdict IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    // Unanswered issued items are withdrawn: a run that stopped halfway
    // leaves nothing half-open.
    store.conn().execute("DELETE FROM loop_queue WHERE verdict IS NULL", [])?;
    let mut refs: Vec<(String, String)> = Vec::new();
    // Every label is asked once; after that a few are re-asked each run, so a
    // judge that drifts shows it.
    let unasked: Vec<usize> = (0..labels.len()).filter(|i| !asked.contains(&i.to_string())).collect();
    let calib: Vec<usize> = if !unasked.is_empty() {
        unasked
    } else {
        let all: Vec<usize> = (0..labels.len()).collect();
        let mut stmt = store.conn().prepare("SELECT value FROM json_each(?1) ORDER BY RANDOM() LIMIT ?2")?;
        let rows = stmt.query_map(params![serde_json::to_string(&all)?, REFRESHER as i64], |r| r.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).map(|v| v as usize).collect()
    };
    let _ = answered;
    for i in calib {
        refs.push(("calibration".into(), i.to_string()));
    }
    let pairs: Vec<i64> = {
        let mut stmt = store.conn().prepare(
            "SELECT id FROM loop_pairs WHERE status = 'open' AND verdict = ''
             AND CAST(id AS TEXT) NOT IN (SELECT ref FROM loop_queue WHERE kind = 'pair')
             ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![REAL_ITEMS_PER_RUN as i64], |r| r.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for p in pairs {
        refs.push(("pair".into(), p.to_string()));
    }
    if refs.is_empty() {
        return Ok("Nothing to judge this week.".into());
    }
    for (kind, r) in &refs {
        store.conn().execute("INSERT INTO loop_queue (kind, ref) VALUES (?1, ?2)", params![kind, r])?;
    }
    // Shuffled together: the judge must not be able to tell which are checked.
    let issued: Vec<(i64, String, String)> = {
        let mut stmt = store.conn().prepare("SELECT item, kind, ref FROM loop_queue WHERE verdict IS NULL ORDER BY RANDOM()")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let mut out = format!(
        "{} item(s). Each shows an OLDER and a NEWER knowledge entry that read alike. For each, answer from \
         the two texts alone:\n  duplicate   -- the same lesson\n  refinement  -- the newer one improves or \
         extends the older\n  conflict    -- they give opposite advice about the same thing\n  compatible  \
         -- related, but both stand\nThen call loop_judge once with every answer.\n\n",
        issued.len()
    );
    for (item, kind, r) in issued {
        let (table, older, newer) = if kind == "calibration" {
            let l = &labels[r.parse::<usize>()?];
            (l.kind.clone(), l.older, l.newer)
        } else {
            store.conn().query_row(
                "SELECT kind, old_id, new_id FROM loop_pairs WHERE id = ?1",
                params![r.parse::<i64>()?],
                |x| Ok((x.get::<_, String>(0)?, x.get(1)?, x.get(2)?)),
            )?
        };
        out.push_str(&format!(
            "Item {item} ({table})\n  OLDER: {}\n  NEWER: {}\n\n",
            entry_text(store, &table, older),
            entry_text(store, &table, newer)
        ));
    }
    Ok(out)
}

/// Record the judge's answers. Calibration items are scored; real pairs are
/// acted on only if the judge is calibrated, and otherwise kept as suggestions.
pub fn judge(store: &Store, repo_root: &Path, answers: &[(i64, String)]) -> Result<String> {
    let labels = load_labels(repo_root)?;
    let mut real: Vec<(i64, i64, String)> = Vec::new();
    let (mut scored, mut agreed) = (0, 0);
    for (item, verdict) in answers {
        let verdict = verdict.trim().to_lowercase();
        let Some(act) = action(&verdict) else { bail!("item {item}: unknown verdict `{verdict}`") };
        let row: Option<(String, String, Option<String>)> = store
            .conn()
            .query_row("SELECT kind, ref, verdict FROM loop_queue WHERE item = ?1", params![item], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok();
        let Some((kind, r, previous)) = row else { bail!("no queued item {item}") };
        if previous.is_some() {
            continue;
        }
        store.conn().execute(
            "UPDATE loop_queue SET verdict = ?2, answered_at = unixepoch() WHERE item = ?1",
            params![item, verdict],
        )?;
        if kind == "calibration" {
            let l = &labels[r.parse::<usize>()?];
            let agree = action(&l.label) == Some(act);
            store.conn().execute(
                "UPDATE loop_queue SET acted = ?2 WHERE item = ?1",
                params![item, if agree { "agree" } else { "disagree" }],
            )?;
            scored += 1;
            agreed += i64::from(agree);
        } else {
            real.push((*item, r.parse()?, verdict));
        }
    }
    let (n, agree) = calibration(store);
    let mut out = format!(
        "Calibration: {agreed}/{scored} agreed this run; {agree}/{n} overall ({:.0}%).\n",
        if n > 0 { 100.0 * agree as f64 / n as f64 } else { 0.0 }
    );
    match calibrated(store) {
        Ok(()) => {
            out.push_str("The judge is calibrated: its verdicts on real pairs act.\n");
            for (item, pair, verdict) in real {
                let (kind, new_id, old_id): (String, i64, i64) = store.conn().query_row(
                    "SELECT kind, new_id, old_id FROM loop_pairs WHERE id = ?1",
                    params![pair],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                let result = crate::reconcile::resolve_pair(store, &kind, new_id, old_id, &verdict, None, None)
                    .unwrap_or_else(|e| format!("not applied: {e}"));
                store.conn().execute("UPDATE loop_queue SET acted = ?2 WHERE item = ?1", params![item, format!("applied: {verdict}")])?;
                out.push_str(&format!("  pair {pair} ({kind} {new_id} vs {old_id}): {verdict} -> {result}\n"));
            }
        }
        Err(why) => {
            out.push_str(&format!("Not calibrated yet ({why}): verdicts on real pairs are kept as suggestions.\n"));
            for (item, pair, verdict) in real {
                store.conn().execute("UPDATE loop_queue SET acted = ?2 WHERE item = ?1", params![item, format!("suggested: {verdict}")])?;
                store.conn().execute(
                    "UPDATE loop_pairs SET verdict = ?2 WHERE id = ?1 AND status = 'open'",
                    params![pair, format!("suggested {verdict}")],
                )?;
            }
        }
    }
    Ok(out)
}

// ── what the runs cost ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct RunCost {
    pub file: String,
    pub calls: usize,
    pub max_context: i64,
    pub input_equivalent: f64,
}

impl RunCost {
    pub fn over_budget(&self) -> bool {
        self.calls > MAX_CALLS || self.max_context > MAX_CONTEXT
    }
}

/// Maintenance runs among the transcripts modified in the last `days`, found by
/// the marker in their first user message, with their cost in input-token
/// equivalents (cache read 0.1x, cache write 2x, output 5x).
pub fn maintenance_runs(dir: &Path, days: u64) -> Vec<RunCost> {
    let Ok(files) = crate::capture::transcripts(dir) else { return Vec::new() };
    let now = std::time::SystemTime::now();
    let mut runs = Vec::new();
    for f in files {
        let recent = std::fs::metadata(&f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() < days * 86_400);
        if !recent {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let mut lines = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
        let first_user = lines.by_ref().find(|v| v.get("type").and_then(Value::as_str) == Some("user"));
        if !first_user.is_some_and(|v| v.to_string().contains(MARKER)) {
            continue;
        }
        let mut seen = std::collections::HashSet::new();
        let (mut calls, mut max_ctx, mut eq) = (0usize, 0i64, 0f64);
        for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
            if v.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_string();
            if !seen.insert(id) {
                continue;
            }
            let u = |k: &str| v.pointer(&format!("/message/usage/{k}")).and_then(Value::as_i64).unwrap_or(0);
            let (input, write, read, output) = (u("input_tokens"), u("cache_creation_input_tokens"), u("cache_read_input_tokens"), u("output_tokens"));
            calls += 1;
            max_ctx = max_ctx.max(input + write + read);
            eq += input as f64 + write as f64 * 2.0 + read as f64 * 0.1 + output as f64 * 5.0;
        }
        runs.push(RunCost { file: f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), calls, max_context: max_ctx, input_equivalent: eq });
    }
    runs
}

// ── the digest ───────────────────────────────────────────────────────────────

/// The week in one page, written to `.cortex/loop-digest.md` and returned.
/// Ends with STOP when the last two runs were over budget.
pub fn digest(store: &Store, repo_root: &Path, transcripts: Option<PathBuf>) -> Result<String> {
    let one = |sql: &str| -> i64 { store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let week = "created_at >= unixepoch() - 7 * 86400";
    let mut o = format!("# Cortex loop digest, {}\n\n", chrono::Utc::now().format("%Y-%m-%d"));
    o.push_str(&format!(
        "## This week\n- Entries committed automatically: {} (backfill {}), duplicates merged: {}, retracted: {}, restored: {}\n",
        one(&format!("SELECT COUNT(*) FROM loop_changes WHERE class = 'entry' AND {week}")),
        one(&format!("SELECT COUNT(*) FROM loop_changes WHERE class = 'backfill' AND {week}")),
        one(&format!("SELECT COUNT(*) FROM loop_changes WHERE class = 'duplicate' AND {week}")),
        one(&format!("SELECT COUNT(*) FROM loop_changes WHERE class = 'retract' AND {week}")),
        one(&format!("SELECT COUNT(*) FROM loop_changes WHERE class = 'restore' AND {week}")),
    ));
    let (audited, bad) = crate::loop_ledger::audit_stats(store);
    let unaudited = crate::loop_ledger::unaudited(store);
    o.push_str(&format!(
        "\n## Your part\n- {unaudited} automatically committed entries are unaudited. `./.cortex/cortex.sh knowledge audit` shows 5 (about a minute).\n\
         - Audited so far: {audited}; wrong or useless in the last {}: {bad} (automatic commit switches off above {}).\n\
         - Automatic commit is {}.\n",
        crate::loop_ledger::AUDIT_WINDOW,
        crate::loop_ledger::AUDIT_MAX_BAD,
        if crate::loop_ledger::auto_commit_enabled(store) { "on" } else { "OFF" }
    ));
    let (open_pairs, resolved_pairs, disputes) = crate::reconcile::counts(store);
    let (n, agree) = calibration(store);
    o.push_str(&format!(
        "\n## Look-alikes and disputes\n- Pairs: {open_pairs} open, {resolved_pairs} resolved. Open disputes: {disputes}.\n\
         - Judge calibration: {agree}/{n} agree -> {}.\n",
        match calibrated(store) {
            Ok(()) => "its verdicts act".to_string(),
            Err(why) => format!("suggestions only ({why})"),
        }
    ));
    let capture: Option<(i64, i64, i64)> = store
        .conn()
        .query_row("SELECT fired, matched, last_fired FROM hook_heartbeat WHERE hook = 'capture_markers'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok();
    o.push_str("\n## Capture\n");
    match capture {
        Some((fired, matched, last)) => o.push_str(&format!(
            "- The capture hook ran {fired} times ({matched} with markers), last {:.1} h ago.\n",
            (chrono::Utc::now().timestamp() - last) as f64 / 3600.0
        )),
        None => o.push_str("- The capture hook has never run: markers reach the store only at closeout.\n"),
    }
    if let Some(err) = store.get_meta("loop.capture_error")?.filter(|e| !e.is_empty()) {
        o.push_str(&format!("- Last capture error: {err}\n"));
    }
    let mut stop = false;
    if let Some(dir) = transcripts {
        let prefs = repo_root.join(".cortex").join("prefs.toml");
        let prefs = prefs.exists().then_some(prefs.as_path());
        if let Ok(cov) = crate::capture::coverage(store, &dir, chrono::Utc::now().timestamp() - 7 * 86_400, prefs) {
            o.push_str(&format!("- Markers written in the last 7 days: {}, in the store: {}.\n", cov.written, cov.stored));
        }
        let runs = maintenance_runs(&dir, 35);
        o.push_str("\n## What this loop cost\n");
        if runs.is_empty() {
            o.push_str("- No maintenance run recorded yet.\n");
        }
        for r in &runs {
            o.push_str(&format!(
                "- run {}: {} calls, peak context {}k, about {:.2}M input-token equivalents{}\n",
                r.file,
                r.calls,
                r.max_context / 1000,
                r.input_equivalent / 1e6,
                if r.over_budget() { " (OVER BUDGET)" } else { "" }
            ));
        }
        stop = runs.len() >= 2 && runs.iter().rev().take(2).all(RunCost::over_budget);
    }
    if stop {
        o.push_str(&format!(
            "\nSTOP: the last two maintenance runs were over budget ({MAX_CALLS} calls or {}k context). Disable the scheduled task and say why.\n",
            MAX_CONTEXT / 1000
        ));
    }
    let _ = std::fs::write(repo_root.join(".cortex").join("loop-digest.md"), &o);
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str) -> (crate::test_support::TempStore, PathBuf) {
        let s = crate::test_support::TempStore::new(tag).unwrap();
        let root = std::env::temp_dir().join(format!("cortex-maint-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".cortex/corpora")).unwrap();
        let mut pairs = Vec::new();
        for i in 0..22 {
            let a = ap(&s, &format!("older lesson {i}"));
            let b = ap(&s, &format!("newer lesson {i}"));
            let label = if i % 2 == 0 { "duplicate" } else { "compatible" };
            pairs.push(serde_json::json!({"kind": "anti_patterns", "older": a, "newer": b, "label": label}));
        }
        std::fs::write(root.join(LABELS_FILE), serde_json::json!({ "pairs": pairs }).to_string()).unwrap();
        crate::loop_ledger::register_evaluator(&s, EVALUATOR, &[root.join(LABELS_FILE)]).unwrap();
        (s, root)
    }

    fn ap(s: &Store, d: &str) -> i64 {
        s.conn()
            .execute(
                "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at) VALUES (?1, 'w', 'c', '[]', ?2)",
                params![d, chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
        s.conn().last_insert_rowid()
    }

    fn items(s: &Store) -> Vec<(i64, String, String)> {
        let mut stmt = s.conn().prepare("SELECT item, kind, ref FROM loop_queue WHERE verdict IS NULL ORDER BY item").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().filter_map(|r| r.ok()).collect()
    }

    #[test]
    fn the_queue_never_shows_which_items_are_checked() {
        let (s, root) = setup("blind");
        let real_old = ap(&s, "real older");
        let real_new = ap(&s, "real newer");
        crate::reconcile::open_pair(&s, "anti_patterns", real_new, real_old, 0.4, true).unwrap();
        let text = queue(&s, &root).unwrap();
        assert!(!text.contains("calibration"), "the queue says which items are checked: {text}");
        assert!(!text.to_lowercase().contains("label"), "labels leak into the queue: {text}");
        assert_eq!(items(&s).len(), 23);
    }

    #[test]
    fn verdicts_on_real_pairs_act_only_after_calibration() {
        let (s, root) = setup("calibrate");
        let real_old = ap(&s, "real older");
        let real_new = ap(&s, "real newer");
        crate::reconcile::open_pair(&s, "anti_patterns", real_new, real_old, 0.4, true).unwrap();
        queue(&s, &root).unwrap();
        let labels = load_labels(&root).unwrap();
        // A judge that agrees on every calibration item, and calls the real pair a duplicate.
        let answers: Vec<(i64, String)> = items(&s)
            .into_iter()
            .map(|(item, kind, r)| {
                let v = if kind == "calibration" { labels[r.parse::<usize>().unwrap()].label.clone() } else { "duplicate".into() };
                (item, v)
            })
            .collect();
        let out = judge(&s, &root, &answers).unwrap();
        assert!(out.contains("verdicts on real pairs act"), "{out}");
        assert!(s.all_anti_patterns().unwrap().iter().all(|a| a.id != Some(real_old)), "the duplicate verdict was applied");

        // An edited label file: nothing acts until a person re-registers it.
        std::fs::write(root.join(LABELS_FILE), "{\"pairs\": []}").unwrap();
        assert!(calibrated(&s).unwrap_err().contains("changed since it was registered"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_judge_that_disagrees_only_suggests() {
        let (s, root) = setup("disagree");
        let real_old = ap(&s, "real older");
        let real_new = ap(&s, "real newer");
        crate::reconcile::open_pair(&s, "anti_patterns", real_new, real_old, 0.4, true).unwrap();
        queue(&s, &root).unwrap();
        let answers: Vec<(i64, String)> = items(&s).into_iter().map(|(item, _, _)| (item, "conflict".to_string())).collect();
        let out = judge(&s, &root, &answers).unwrap();
        assert!(out.contains("Not calibrated yet"), "{out}");
        assert!(s.all_anti_patterns().unwrap().iter().any(|a| a.id == Some(real_old)), "nothing was applied");
        let v: String = s.conn().query_row("SELECT verdict FROM loop_pairs", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "suggested conflict");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_digest_names_the_audit_and_writes_its_file() {
        let (s, root) = setup("digest");
        let d = digest(&s, &root, None).unwrap();
        assert!(d.contains("knowledge audit") && d.contains("Judge calibration"), "{d}");
        assert!(root.join(".cortex/loop-digest.md").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
