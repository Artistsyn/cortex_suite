/// Phase 5b: Meta-analysis for the consolidation pipeline.
///
/// Four analyzers — each returns `MetaSignal`s which are then staged as proposals:
///   1. `analyze_rejection_rates`   — gate firing patterns (Phase 5b)
///   2. `analyze_fidelity_trends`   — protocol step adherence over time (Phase 5c)
///   3. `analyze_gap_evolution`     — persistent unresolved query gaps (Phase 5c)
///   4. `analyze_threshold_impact`  — per-type approval rates vs thresholds (Phase 5c)
///
/// All findings are staged as `meta_*` proposals — never auto-applied,
/// never source-modifying. Apply via `cortex meta apply <id>`.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};

use crate::memory::Store;
use crate::verify;

// ── Meta report ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize)]
pub struct MetaReport {
    pub generated_at:           String,
    pub total_proposals:        usize,
    pub approved:               usize,
    pub rejected:               usize,
    pub pending:                usize,
    pub trial:                  usize,
    pub approval_rate:          f32,
    /// Fraction of proposals rejected by a gate (gate: prefix) vs human rejection.
    pub gate_rejection_rate:    f32,
    pub top_rejected_gates:     Vec<(String, usize)>,
    pub avg_fidelity_score:     f32,
    pub low_fidelity_sessions:  usize,
    pub most_missed_step:       Option<String>,
    pub persistent_gaps:        usize,
    pub threshold_alerts:       Vec<String>,
}

// ── Signal: atomic insight from one analyzer ─────────────────────────────────

#[derive(Debug)]
struct MetaSignal {
    hash:          String,
    proposal_type: &'static str,
    target:        &'static str,
    text:          String,
    evidence:      Value,
}

fn simple_hash(data: &[u8]) -> String {
    let h = data.iter()
        .fold(14695981039346656037u64, |h, &b| h.wrapping_mul(1099511628211u64) ^ b as u64);
    format!("{h:016x}")
}

// ── Phase 5b: Rejection rate analysis ────────────────────────────────────────

/// Analyze gate rejection patterns from the rejected-proposals.jsonl log.
/// Returns (gate_rejection_rate, gate_counts by gate name).
fn analyze_rejection_rates(
    store: &Store,
    rejected_log: &Path,
) -> Result<(f32, Vec<(String, usize)>)> {
    let total_proposals: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals", [], |r| r.get(0),
    ).unwrap_or(0);

    let mut gate_counts: HashMap<String, usize> = HashMap::new();

    if let Ok(content) = std::fs::read_to_string(rejected_log) {
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(reason) = v.get("reason").and_then(|r| r.as_str()) {
                    let gate = if reason.starts_with("duplicate:") { "duplicate"
                    } else if reason.starts_with("rust_syntax:") { "rust_syntax"
                    } else if reason.starts_with("credibility:") { "credibility"
                    } else if reason.starts_with("gap_trial:") { "gap_trial"
                    } else if reason.starts_with("survival_trend:") { "survival_trend"
                    } else { "other" };
                    *gate_counts.entry(gate.to_string()).or_default() += 1;
                }
            }
        }
    }

    let log_total: usize = gate_counts.values().sum();
    let gate_rejection_rate = if total_proposals > 0 {
        log_total as f32 / total_proposals as f32
    } else {
        0.0
    };

    let mut sorted: Vec<(String, usize)> = gate_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    Ok((gate_rejection_rate.min(1.0), sorted))
}

// ── Phase 5c: Fidelity trend analysis ────────────────────────────────────────

/// Analyze session fidelity scores over recent sessions.
/// Returns (avg_score, low_fidelity_count, most_missed_step).
fn analyze_fidelity_trends(store: &Store) -> Result<(f32, usize, Option<String>)> {
    // Read last 20 sessions that have fidelity scores.
    let mut stmt = store.conn().prepare(
        "SELECT marker_counts FROM session_snapshots
         WHERE json_extract(marker_counts, '$.fidelity_score') IS NOT NULL
         ORDER BY created_at DESC LIMIT 20"
    )?;

    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mc_list: Vec<String> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    if mc_list.is_empty() {
        return Ok((1.0, 0, None));
    }

    let mut scores: Vec<f32> = Vec::new();
    let mut miss_counts: HashMap<String, usize> = HashMap::new();

    for mc_json in &mc_list {
        if let Ok(mc) = serde_json::from_str::<Value>(mc_json) {
            if let Some(score) = mc.get("fidelity_score").and_then(|s| s.as_f64()) {
                scores.push(score as f32);
            }
            if let Some(missing) = mc.get("fidelity_missing").and_then(|m| m.as_array()) {
                for step in missing {
                    if let Some(s) = step.as_str() {
                        *miss_counts.entry(s.to_string()).or_default() += 1;
                    }
                }
            }
        }
    }

    let avg = if scores.is_empty() { 1.0 } else {
        scores.iter().sum::<f32>() / scores.len() as f32
    };
    let low_count = scores.iter().filter(|&&s| s < 0.6).count();
    let most_missed = miss_counts.into_iter().max_by_key(|(_, c)| *c).map(|(k, _)| k);

    Ok((avg, low_count, most_missed))
}

// ── Phase 5c: Gap evolution analysis ─────────────────────────────────────────

/// Find persistent unresolved gaps: query_gap_log entries with high seen_count
/// that have no corresponding approved or pending proposal.
fn analyze_gap_evolution(store: &Store) -> Result<usize> {
    let cutoff = Utc::now().timestamp() - 30 * 86400; // 30-day window

    let mut stmt = store.conn().prepare(
        "SELECT query_text, seen_count FROM query_gap_log
         WHERE seen_count >= 3 AND last_seen_at >= ?1
         ORDER BY seen_count DESC LIMIT 20"
    )?;

    let rows = stmt.query_map(rusqlite::params![cutoff], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let gaps: Vec<(String, i64)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut unresolved = 0usize;
    for (query, _count) in &gaps {
        let has_proposal: bool = store.conn().query_row(
            "SELECT COUNT(*) > 0 FROM proposals
             WHERE proposed_text LIKE ?1 AND status IN ('approved','pending')",
            rusqlite::params![format!("%{}%", &query.chars().take(40).collect::<String>())],
            |r| r.get(0),
        ).unwrap_or(false);
        if !has_proposal {
            unresolved += 1;
        }
    }

    Ok(unresolved)
}

// ── Phase 5c: Threshold impact analysis ──────────────────────────────────────

/// How far back the approval-rate analysis looks. Gate thresholds are tuned for
/// how the pipeline behaves *now*, so the signal must come from recent runs.
const THRESHOLD_WINDOW_DAYS: i64 = 30;

/// Compute per-type approval rates and return alerts for types with consistently
/// low approval (< 20% with >= 5 samples) within the recent window.
///
/// The window matters. Scoring all history lets one bad run poison the metric
/// permanently: a pre-fix burst produced 1,304 auto-rejected drift proposals in
/// a single minute, after which this analyzer reported "drift_flag: 0/1304
/// approved (0%)" on every subsequent run — advice derived entirely from a
/// defect that had already been fixed, which it then proposed acting on.
fn analyze_threshold_impact(store: &Store) -> Result<Vec<String>> {
    let cutoff = chrono::Utc::now().timestamp() - THRESHOLD_WINDOW_DAYS * 86_400;
    let mut stmt = store.conn().prepare(
        "SELECT proposal_type,
                COUNT(*) as total,
                SUM(CASE WHEN status='approved' THEN 1 ELSE 0 END) as approved_count
         FROM proposals
         WHERE created_at >= ?1
         GROUP BY proposal_type
         HAVING total >= 5"
    )?;

    let rows = stmt.query_map([cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;

    let mut alerts = Vec::new();
    for row in rows {
        let (ptype, total, approved) = row?;
        let rate = approved as f32 / total as f32;
        if rate < 0.2 {
            alerts.push(format!(
                "Type '{}': {}/{} approved ({:.0}%) in the last {} days — consider adjusting gate thresholds",
                ptype, approved, total, rate * 100.0, THRESHOLD_WINDOW_DAYS
            ));
        }
    }

    Ok(alerts)
}

// ── Main API ──────────────────────────────────────────────────────────────────

/// Build a full meta-analysis report combining all four analyzers.
pub fn build_meta_report(store: &Store, rejected_log: &Path) -> Result<MetaReport> {
    let total: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals", [], |r| r.get(0),
    ).unwrap_or(0);

    let approved: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'approved'", [], |r| r.get(0),
    ).unwrap_or(0);

    let rejected: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'rejected'", [], |r| r.get(0),
    ).unwrap_or(0);

    let pending: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'pending'", [], |r| r.get(0),
    ).unwrap_or(0);

    let trial: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'trial'", [], |r| r.get(0),
    ).unwrap_or(0);

    let approval_rate = if total > 0 { approved as f32 / total as f32 } else { 0.0 };

    let (gate_rejection_rate, top_rejected_gates) = analyze_rejection_rates(store, rejected_log)?;
    let (avg_fidelity, low_fidelity, most_missed) = analyze_fidelity_trends(store)?;
    let persistent_gaps = analyze_gap_evolution(store)?;
    let threshold_alerts = analyze_threshold_impact(store)?;

    Ok(MetaReport {
        generated_at: Utc::now().to_rfc3339(),
        total_proposals: total as usize,
        approved: approved as usize,
        rejected: rejected as usize,
        pending: pending as usize,
        trial: trial as usize,
        approval_rate,
        gate_rejection_rate,
        top_rejected_gates,
        avg_fidelity_score: avg_fidelity,
        low_fidelity_sessions: low_fidelity,
        most_missed_step: most_missed,
        persistent_gaps,
        threshold_alerts,
    })
}

/// Stage meta-proposals based on analysis findings.
/// Returns count of new proposals staged.
pub fn stage_meta_proposals(store: &Store, report: &MetaReport) -> Result<usize> {
    let mut signals: Vec<MetaSignal> = Vec::new();

    // Signal 1: High gate rejection rate.
    if report.gate_rejection_rate > 0.6 && report.total_proposals >= 5 {
        let text = format!(
            "Meta: Gate rejection rate is {:.0}% across {} proposals. \
             Review verification thresholds (credibility filter, gap trial).",
            report.gate_rejection_rate * 100.0, report.total_proposals
        );
        signals.push(MetaSignal {
            hash: simple_hash(b"meta:gate_rejection_rate_high"),
            proposal_type: "meta_threshold",
            target: ".cortex/prefs.toml",
            evidence: json!({
                "source": "meta_analysis",
                "gate_rejection_rate": report.gate_rejection_rate,
                "approval_rate": report.approval_rate,
            }),
            text,
        });
    }

    // Signal 2: Zero approval rate after enough data.
    if report.approval_rate == 0.0 && report.total_proposals >= 5 {
        signals.push(MetaSignal {
            hash: simple_hash(b"meta:zero_approval"),
            proposal_type: "meta_threshold",
            target: ".cortex/prefs.toml",
            text: format!(
                "Meta: Zero proposals approved out of {}. \
                 Pipeline may be generating low-quality proposals — review gate thresholds.",
                report.total_proposals
            ),
            evidence: json!({"source": "meta_analysis", "total": report.total_proposals}),
        });
    }

    // Signal 3: Low average fidelity score.
    if report.avg_fidelity_score < 0.6 && report.low_fidelity_sessions >= 3 {
        let step_hint = report.most_missed_step.as_deref().unwrap_or("unknown");
        signals.push(MetaSignal {
            hash: simple_hash(format!("meta:low_fidelity:{step_hint}").as_bytes()),
            proposal_type: "meta_instruction",
            target: ".github/copilot-instructions.md",
            text: format!(
                "Meta: Average fidelity score {:.0}% across {} sessions. \
                 Most missed step: '{}'. \
                 Consider strengthening the instruction for this protocol step.",
                report.avg_fidelity_score * 100.0, report.low_fidelity_sessions, step_hint
            ),
            evidence: json!({
                "source": "meta_analysis",
                "avg_fidelity": report.avg_fidelity_score,
                "most_missed_step": step_hint,
            }),
        });
    }

    // Signal 4: Persistent unresolved gaps.
    if report.persistent_gaps >= 3 {
        signals.push(MetaSignal {
            hash: simple_hash(b"meta:persistent_gaps"),
            proposal_type: "meta_gap_priority",
            target: ".cortex/prefs.toml",
            text: format!(
                "Meta: {} persistent query gaps (seen ≥3 times in 30 days) have no approved proposal. \
                 Run 'cortex propose-gaps' or lower gap_trial threshold.",
                report.persistent_gaps
            ),
            evidence: json!({"source": "meta_analysis", "gap_count": report.persistent_gaps}),
        });
    }

    // Signal 5: Per-type threshold alerts.
    for alert in &report.threshold_alerts {
        signals.push(MetaSignal {
            hash: simple_hash(format!("meta:threshold_alert:{alert}").as_bytes()),
            proposal_type: "meta_threshold",
            target: ".cortex/prefs.toml",
            text: format!("Meta: {alert}"),
            evidence: json!({"source": "meta_analysis"}),
        });
    }

    // Stage all signals as proposals (skip duplicates via INSERT OR IGNORE).
    let mut staged = 0usize;
    for sig in signals {
        let result = store.conn().execute(
            "INSERT OR IGNORE INTO proposals
             (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', '{\"gate\":\"meta_analysis\"}')",
            rusqlite::params![
                sig.proposal_type,
                sig.hash,
                sig.target,
                sig.text,
                sig.evidence.to_string(),
            ],
        );
        if result.map(|n| n > 0).unwrap_or(false) {
            staged += 1;
        }
    }

    Ok(staged)
}

/// Apply an approved meta-proposal to its target file.
/// For `meta_threshold` / `meta_gap_priority` targeting prefs.toml:
///   appends the proposed_text as a note in [project].notes.
/// For `meta_instruction` targeting copilot-instructions.md:
///   appends the proposed_text as a comment at the end.
///
/// Returns `(applied, dry_run_diff)`.
pub fn apply_meta_proposal(
    store: &Store,
    proposal_id: i64,
    repo_root: &Path,
    dry_run: bool,
) -> Result<(bool, String)> {
    // Load the proposal.
    let (proposal_type, target, proposed_text, status): (String, String, String, String) =
        store.conn().query_row(
            "SELECT proposal_type, target_file, proposed_text, status
             FROM proposals WHERE id = ?1",
            rusqlite::params![proposal_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

    if status != "approved" && status != "pending" {
        return Ok((false, format!("Proposal {} has status '{}' — only approved/pending proposals can be applied.", proposal_id, status)));
    }

    let target_path = repo_root.join(&target);
    let diff;

    match proposal_type.as_str() {
        "meta_threshold" | "meta_gap_priority" if target.ends_with("prefs.toml") => {
            if !target_path.exists() {
                return Ok((false, format!("Target file '{}' does not exist.", target_path.display())));
            }
            let existing = std::fs::read_to_string(&target_path)?;
            let note_line = format!("  \"META({}): {}\",", Utc::now().format("%Y-%m-%d"),
                proposed_text.chars().take(120).collect::<String>().replace('"', "'"));

            // Append into [project].notes if present, else append at end.
            let new_content = if existing.contains("[project]") && existing.contains("notes = [") {
                existing.replacen("notes = [", &format!("notes = [\n{note_line}"), 1)
            } else {
                format!("{existing}\n# Meta proposal applied {}\n# {}\n",
                    Utc::now().format("%Y-%m-%d"), proposed_text)
            };

            diff = format!("+ {note_line}");
            if !dry_run {
                std::fs::write(&target_path, &new_content)?;
                store.conn().execute(
                    "UPDATE proposals SET status = 'committed', committed_at = unixepoch() WHERE id = ?1",
                    rusqlite::params![proposal_id],
                )?;
            }
        }
        "meta_instruction" if target.contains("copilot-instructions") || target.contains("CLAUDE.md") => {
            // Instruction improvements apply to BOTH agent manuals — the files are
            // maintained as platform twins, and a note only one agent can see is
            // drift, not learning. Written as visible markdown (a bullet under a
            // dedicated section), not an HTML comment nobody reads.
            let twins = [".github/copilot-instructions.md", "CLAUDE.md"];
            let section = "## Meta-Learned Notes";
            let bullet = format!(
                "- {} — {}",
                Utc::now().format("%Y-%m-%d"),
                proposed_text.chars().take(300).collect::<String>().replace('\n', " "),
            );

            let mut applied_to: Vec<String> = Vec::new();
            for rel in twins {
                let path = repo_root.join(rel.trim());
                if !path.exists() { continue; }
                if !dry_run {
                    let existing = std::fs::read_to_string(&path)?;
                    let new_content = if let Some(pos) = existing.find(section) {
                        // Insert the bullet right after the section heading line.
                        let insert_at = existing[pos..].find('\n')
                            .map(|n| pos + n + 1)
                            .unwrap_or(existing.len());
                        format!("{}{}\n{}", &existing[..insert_at], bullet, &existing[insert_at..])
                    } else {
                        format!("{existing}\n{section}\n\nSelf-tuning notes staged by the cortex meta-analysis loop and human-approved via `cortex meta apply`.\n\n{bullet}\n")
                    };
                    std::fs::write(&path, new_content)?;
                }
                applied_to.push(rel.trim().to_string());
            }

            if applied_to.is_empty() {
                return Ok((false, "Neither instruction file exists — nothing applied.".to_string()));
            }
            diff = format!("+ {bullet}\n  (applied to: {})", applied_to.join(", "));
            if !dry_run {
                store.conn().execute(
                    "UPDATE proposals SET status = 'committed', committed_at = unixepoch() WHERE id = ?1",
                    rusqlite::params![proposal_id],
                )?;
            }
        }
        _ => {
            return Ok((false, format!("Proposal type '{}' targeting '{}' is not auto-applicable.", proposal_type, target)));
        }
    }

    Ok((true, diff))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Store;

    /// A store of its own, removed when the test ends -- see test_support.
    fn test_store(name: &str) -> crate::test_support::TempStore {
        crate::test_support::TempStore::new(name).unwrap()
    }

    #[test]
    fn meta_report_empty_db() {
        let store = test_store("empty");
        let log = std::env::temp_dir().join("cortex-meta-test-empty.jsonl");
        let report = build_meta_report(&store, &log).unwrap();
        assert_eq!(report.total_proposals, 0);
        assert_eq!(report.approval_rate, 0.0);
        assert_eq!(report.persistent_gaps, 0);
        assert!(report.threshold_alerts.is_empty());
    }

    #[test]
    fn meta_report_counts_proposals() {
        let store = test_store("counts");
        let log = std::env::temp_dir().join("cortex-meta-test-counts.jsonl");

        store.conn().execute_batch(
            "INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note', 'a', 'x', 'test', '{}', 'approved', '{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note', 'b', 'x', 'test', '{}', 'rejected', '{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note', 'c', 'x', 'test', '{}', 'pending', '{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note', 'd', 'x', 'test', '{}', 'trial', '{}');"
        ).unwrap();

        let report = build_meta_report(&store, &log).unwrap();
        assert_eq!(report.total_proposals, 4);
        assert_eq!(report.approved, 1);
        assert_eq!(report.rejected, 1);
        assert_eq!(report.pending, 1);
        assert_eq!(report.trial, 1);
        assert!((0.0..=1.0).contains(&report.approval_rate));
    }

    #[test]
    fn gate_rejection_rate_uses_log_only() {
        // The gate rejection rate should come from the log, not the DB rejected count.
        let store = test_store("gaterate");
        let log = std::env::temp_dir().join("cortex-meta-gate-rate.jsonl");
        // Write 2 gate rejections to log.
        let mut f = std::fs::File::create(&log).unwrap();
        use std::io::Write;
        writeln!(f, r#"{{"timestamp":"{}","reason":"credibility: low","content_hash":"x"}}"#,
            Utc::now().to_rfc3339()).unwrap();
        writeln!(f, r#"{{"timestamp":"{}","reason":"duplicate: hash exists","content_hash":"y"}}"#,
            Utc::now().to_rfc3339()).unwrap();

        // Add 4 proposals to DB.
        store.conn().execute_batch(
            "INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note','p1','x','t','{}','approved','{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note','p2','x','t','{}','pending','{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note','p3','x','t','{}','pending','{}');
             INSERT INTO proposals (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
             VALUES ('pref_note','p4','x','t','{}','pending','{}');"
        ).unwrap();

        let report = build_meta_report(&store, &log).unwrap();
        // 2 log rejections / 4 total proposals = 0.5
        assert!((report.gate_rejection_rate - 0.5).abs() < 0.01,
            "expected ~0.5, got {}", report.gate_rejection_rate);
        assert_eq!(report.top_rejected_gates.len(), 2);
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn stage_meta_proposals_deduplicates() {
        let store = test_store("dedup");
        let log = std::env::temp_dir().join("cortex-meta-dedup.jsonl");
        // Build a report that would trigger a zero-approval signal.
        let report = MetaReport {
            total_proposals: 5,
            approval_rate: 0.0,
            ..Default::default()
        };
        let first = stage_meta_proposals(&store, &report).unwrap();
        let second = stage_meta_proposals(&store, &report).unwrap();
        assert!(first > 0);
        assert_eq!(second, 0, "second call should stage 0 (deduplication via INSERT OR IGNORE)");
        let _ = std::fs::remove_file(&log);
    }
}

#[cfg(test)]
mod threshold_window_tests {
    use super::*;
    use crate::memory::Store;

    /// A store of its own, removed when the test ends -- see test_support.
    fn store(name: &str) -> crate::test_support::TempStore {
        crate::test_support::TempStore::new(name).unwrap()
    }

    fn add(s: &Store, ptype: &str, status: &str, age_days: i64, n: usize) {
        let ts = Utc::now().timestamp() - age_days * 86_400;
        for i in 0..n {
            s.conn()
                .execute(
                    "INSERT INTO proposals (proposal_type, content_hash, target_file, section,
                     proposed_text, evidence, status, gate_signals, created_at)
                     VALUES (?1, ?2, 't', 's', 'x', '{}', ?3, '{}', ?4)",
                    rusqlite::params![ptype, format!("{ptype}{status}{age_days}{i}"), status, ts],
                )
                .unwrap();
        }
    }

    /// A burst of auto-rejections from a defect that has since been fixed must
    /// stop influencing threshold advice once it ages out — otherwise the
    /// analyzer keeps recommending changes based on a bug that no longer exists.
    #[test]
    fn stale_rejections_age_out_of_the_approval_rate() {
        let s = store("stale");
        add(&s, "drift_flag", "rejected", 90, 50); // the old pre-fix burst
        add(&s, "drift_flag", "approved", 1, 8); // healthy recent behaviour

        let alerts = analyze_threshold_impact(&s).unwrap();
        assert!(
            alerts.is_empty(),
            "stale rejections still dragging the rate down: {alerts:?}"
        );
    }

    /// Genuinely poor recent performance must still raise an alert.
    #[test]
    fn recent_low_approval_still_alerts() {
        let s = store("recent");
        add(&s, "drift_flag", "rejected", 2, 20);

        let alerts = analyze_threshold_impact(&s).unwrap();
        assert_eq!(alerts.len(), 1, "expected one alert, got {alerts:?}");
        assert!(alerts[0].contains("drift_flag"), "{}", alerts[0]);
    }

    /// Below the sample floor, say nothing rather than guess from noise.
    #[test]
    fn too_few_recent_samples_produces_no_alert() {
        let s = store("few");
        add(&s, "drift_flag", "rejected", 2, 3);
        assert!(analyze_threshold_impact(&s).unwrap().is_empty());
    }
}
