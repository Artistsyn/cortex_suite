/// Phase 2: Deterministic verification gates for consolidation proposals.
///
/// Gates run BEFORE a proposal is staged in the `proposals` table.
/// A failed gate auto-rejects the proposal with an explanation logged to
/// `.cortex/rejected-proposals.jsonl` so near-duplicate ideas aren't retried.
///
/// Five deterministic gates (no external LLM required):
///   1. Duplicate detection  — content_hash must be unique
///   2. Rust snippet check   — any Rust in the proposed text must parse via `syn`
///   3. Credibility filter   — skill candidates need sufficient session evidence
///   4. Gap-reduction trial  — gap proposals enter a 7-day trial before permanent commit
///   5. Survival delta gate  — pattern-touching proposals need a non-negative survival signal
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::memory::Store;

// ── Gate outcome ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Pass: proposal is safe to stage.
    Pass,
    /// Reject: proposal fails a gate. Includes reason.
    Reject(String),
    /// Trial: proposal needs a waiting period before permanent commit.
    Trial { reason: String, trial_days: u32 },
}

impl GateOutcome {
    pub fn is_pass(&self) -> bool { matches!(self, GateOutcome::Pass) }

    pub fn label(&self) -> &str {
        match self {
            GateOutcome::Pass       => "pass",
            GateOutcome::Reject(_)  => "reject",
            GateOutcome::Trial{..}  => "trial",
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            GateOutcome::Pass              => "",
            GateOutcome::Reject(r)         => r.as_str(),
            GateOutcome::Trial { reason, .. } => reason.as_str(),
        }
    }
}

// ── GateSignals logged to proposals.gate_signals ─────────────────────────────

#[derive(Debug, Default)]
pub struct GateSignals {
    pub duplicate_check:  Option<bool>,
    pub snippet_valid:    Option<bool>,
    pub credibility_ok:   Option<bool>,
    pub gap_trial_days:   Option<u32>,
    pub survival_ok:      Option<bool>,
}

impl GateSignals {
    pub fn to_json(&self) -> String {
        serde_json::to_string(&json!({
            "duplicate_check":  self.duplicate_check,
            "snippet_valid":    self.snippet_valid,
            "credibility_ok":   self.credibility_ok,
            "gap_trial_days":   self.gap_trial_days,
            "survival_ok":      self.survival_ok,
        })).unwrap_or_else(|_| "{}".to_string())
    }
}

// ── Master gate runner ────────────────────────────────────────────────────────

/// Run all applicable gates for a proposal.
/// Returns the overall outcome and a GateSignals record.
pub fn run_gates(
    store: &Store,
    proposal_type: &str,
    content_hash:  &str,
    proposed_text: &str,
    evidence:      &Value,
) -> (GateOutcome, GateSignals) {
    let mut signals = GateSignals::default();

    // Gate 1: Duplicate detection.
    match gate_duplicate(store, content_hash) {
        Ok(false) => {
            signals.duplicate_check = Some(false);
            return (GateOutcome::Reject(
                format!("duplicate: proposal with content_hash {content_hash} already exists")
            ), signals);
        }
        Ok(true) => { signals.duplicate_check = Some(true); }
        Err(e) => { eprintln!("[verify] warn: duplicate gate error: {e}"); }
    }

    // Gate 2: Rust snippet syntax check.
    if contains_rust_snippet(proposed_text) {
        let valid = gate_rust_syntax(proposed_text);
        signals.snippet_valid = Some(valid);
        if !valid {
            return (GateOutcome::Reject(
                "rust_syntax: proposed text contains a Rust snippet that fails syn parse".to_string()
            ), signals);
        }
    }

    // Gate 3: Credibility filter for skill/pref proposals.
    // When the evidence carries session_keys, credibility is COMPUTED from the
    // DB (build_pass fraction of the cited sessions) — an evidence-supplied
    // number alone is never trusted. Without session_keys, fall back to the
    // evidence value (gap proposals have no session backing; the trial gate
    // covers those instead).
    if proposal_type == "skill" || proposal_type == "pref_note" {
        let credibility = match evidence.get("session_keys").and_then(|v| v.as_array()) {
            Some(keys) => {
                let keys: Vec<String> = keys.iter()
                    .filter_map(|k| k.as_str().map(String::from))
                    .collect();
                compute_credibility(store, &keys)
            }
            None => evidence.get("credibility")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0) as f32,
        };
        let min_cred = 0.2f32; // at least 2/10 cited sessions passing to be credible
        let ok = credibility >= min_cred;
        signals.credibility_ok = Some(ok);
        if !ok {
            return (GateOutcome::Reject(
                format!("credibility: {credibility:.2} < {min_cred:.2} — insufficient session evidence")
            ), signals);
        }
    }

    // Gate 4: Gap-reduction trial (gap proposals use a 7-day trial period).
    if proposal_type == "pref_note" {
        let seen_count = evidence.get("seen_count").and_then(|v| v.as_i64()).unwrap_or(0);
        if seen_count < 5 {
            // Low-count gap: needs a trial period to confirm the gap is real.
            signals.gap_trial_days = Some(7);
            return (GateOutcome::Trial {
                reason: format!("gap_trial: seen_count={seen_count} < 5 — requires 7-day trial to confirm gap persists"),
                trial_days: 7,
            }, signals);
        }
        // High-count gap (≥5 misses): skip trial, stage immediately.
        signals.gap_trial_days = Some(0);
    }

    // Gate 5: Survival delta for dying-pattern proposals.
    if proposal_type == "anti_pattern" || proposal_type == "dying_pattern" {
        if let Some(pattern_id) = evidence.get("pattern_id").and_then(|v| v.as_i64()) {
            match gate_survival_trend(store, pattern_id) {
                Ok(trending_down) => {
                    signals.survival_ok = Some(trending_down);
                    if !trending_down {
                        // Pattern is recovering — don't stage a removal proposal.
                        return (GateOutcome::Reject(
                            format!("survival_trend: pattern {pattern_id} is recovering — skip removal proposal")
                        ), signals);
                    }
                }
                Err(e) => { eprintln!("[verify] warn: survival gate error: {e}"); }
            }
        }
    }

    (GateOutcome::Pass, signals)
}

// ── Individual gates ──────────────────────────────────────────────────────────

/// Gate 1: Returns true if NO existing proposal with this hash exists (i.e., proposal is new).
pub fn gate_duplicate(store: &Store, content_hash: &str) -> Result<bool> {
    let count: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE content_hash = ?1",
        rusqlite::params![content_hash],
        |r| r.get(0),
    )?;
    Ok(count == 0) // true = not a duplicate = OK to proceed
}

/// Heuristic: does the text look like it contains a Rust code snippet?
fn contains_rust_snippet(text: &str) -> bool {
    // Patterns that strongly suggest Rust code.
    text.contains("fn ") ||
    text.contains("let ") ||
    text.contains("impl ") ||
    text.contains("pub ") ||
    text.contains("::") ||
    text.contains("->") ||
    (text.contains("{") && text.contains("}") && text.contains(";"))
}

/// Gate 2: Parse Rust-looking snippets with `syn` to detect syntax errors.
/// Returns true if parsing succeeds (or text is not actually Rust).
fn gate_rust_syntax(text: &str) -> bool {
    // Extract fenced code blocks if present, otherwise test the whole text.
    let snippets = extract_code_blocks(text);
    let targets: Vec<&str> = if snippets.is_empty() {
        vec![text]
    } else {
        snippets.iter().map(|s| s.as_str()).collect()
    };

    for snippet in targets {
        // Try to parse as a Rust file (most lenient mode).
        if syn::parse_str::<syn::File>(snippet).is_err() {
            // Fallback: try as a statement block.
            let wrapped = format!("fn _check() {{ {} }}", snippet);
            if syn::parse_str::<syn::File>(&wrapped).is_err() {
                return false;
            }
        }
    }
    true
}

fn extract_code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_block {
                blocks.push(current.clone());
                current.clear();
                in_block = false;
            } else {
                in_block = true;
            }
        } else if in_block {
            current.push_str(line);
            current.push('\n');
        }
    }
    blocks
}

/// Gate 3 helper: compute credibility from the proposals evidence or DB.
pub fn compute_credibility(store: &Store, session_keys: &[String]) -> f32 {
    if session_keys.is_empty() { return 0.0; }
    let pass_count = session_keys.iter().filter(|key| {
        // Count sessions with build_pass outcome via session_snapshots.
        let has_pass = store.conn().query_row(
            "SELECT COUNT(*) > 0 FROM session_snapshots
             WHERE session_key = ?1 AND outcome_type = 'build_pass'",
            rusqlite::params![key],
            |r| r.get::<_, bool>(0),
        ).unwrap_or(false);
        has_pass
    }).count();
    // credibility = min(occurrences, 10) / 10 — mirrors the patterns.credibility column.
    (pass_count.min(10) as f32) / 10.0
}

/// Gate 5: Returns true if the pattern has been consistently trending downward
/// (i.e., newly reverted since last consolidation run).
fn gate_survival_trend(store: &Store, pattern_id: i64) -> Result<bool> {
    // Look at the survival gate_signals of existing proposals for this pattern.
    // If a prior proposal was already rejected for this pattern recently, skip.
    let recent_rejection: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals
         WHERE evidence LIKE ? AND status = 'rejected'
         AND created_at > (unixepoch() - 86400 * 7)",
        rusqlite::params![format!("%\"pattern_id\":{pattern_id}%")],
        |r| r.get(0),
    ).unwrap_or(0);

    if recent_rejection > 0 {
        // Already tried and rejected within the last 7 days — don't retry.
        return Ok(false);
    }

    // Check: does the pattern currently have declining survival?
    let (uses, reverts): (i64, i64) = store.conn().query_row(
        "SELECT use_count, reverted_count FROM patterns WHERE id = ?1",
        rusqlite::params![pattern_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap_or((0, 0));

    // Trending down: reverted more than half the uses.
    Ok(uses >= 3 && reverts as f32 / uses.max(1) as f32 >= 0.5)
}

// ── Rejection log ─────────────────────────────────────────────────────────────

/// Append a rejected proposal to `.cortex/rejected-proposals.jsonl`.
/// Auto-rotates entries older than 90 days, but only rewrites the file
/// if it has grown beyond 100KB to avoid rewriting on every rejection.
pub fn log_rejection(
    rejected_log_path: &Path,
    proposal_type: &str,
    content_hash: &str,
    proposed_text: &str,
    reason: &str,
    signals: &GateSignals,
) {
    // Only rotate if the file is large enough to be worth rewriting.
    const ROTATION_SIZE_THRESHOLD: u64 = 100 * 1024; // 100KB
    if rejected_log_path.exists() {
        let file_size = std::fs::metadata(rejected_log_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if file_size > ROTATION_SIZE_THRESHOLD {
            rotate_rejection_log(rejected_log_path, 90);
        }
    }

    let entry = json!({
        "timestamp":     Utc::now().to_rfc3339(),
        "proposal_type": proposal_type,
        "content_hash":  content_hash,
        "reason":        reason,
        "proposed_text_preview": proposed_text.chars().take(120).collect::<String>(),
        "gate_signals":  signals.to_json(),
    });

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rejected_log_path)
    {
        let _ = writeln!(file, "{}", serde_json::to_string(&entry).unwrap_or_default());
    }
}

/// Check if a proposal was already rejected recently (content hash match).
pub fn is_recently_rejected(rejected_log_path: &Path, content_hash: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(rejected_log_path) else { return false; };
    let cutoff = Utc::now().timestamp() - 30 * 86400; // 30-day window

    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("content_hash").and_then(|h| h.as_str()) == Some(content_hash) {
                // Check age.
                if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                        if dt.timestamp() > cutoff {
                            return true; // rejected within 30 days
                        }
                    }
                }
            }
        }
    }
    false
}

/// Rotate the rejection log: rewrite only entries newer than `max_age_days`.
/// Keeps the log bounded so it doesn't grow forever.
fn rotate_rejection_log(path: &Path, max_age_days: i64) {
    let cutoff = Utc::now().timestamp() - max_age_days * 86400;
    let Ok(content) = std::fs::read_to_string(path) else { return; };

    let keep: Vec<&str> = content.lines()
        .filter(|line| {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                        return dt.timestamp() >= cutoff;
                    }
                }
            }
            false
        })
        .collect();

    if keep.len() < content.lines().count() {
        // Some entries were pruned — rewrite.
        if let Ok(mut file) = std::fs::File::create(path) {
            use std::io::Write;
            for line in keep {
                let _ = writeln!(file, "{}", line);
            }
        }
    }
}

// ── Trial period management ───────────────────────────────────────────────────

/// Mark a proposal as `trial` status — will be re-evaluated after `trial_days`.
pub fn stage_as_trial(store: &Store, content_hash: &str, trial_days: u32) -> Result<()> {
    let trial_expires = Utc::now().timestamp() + (trial_days as i64 * 86400);
    store.conn().execute(
        "UPDATE proposals SET status = 'trial', gate_signals = json_set(gate_signals, '$.trial_expires', ?1)
         WHERE content_hash = ?2",
        rusqlite::params![trial_expires, content_hash],
    )?;
    Ok(())
}

/// Promote proposals whose trial period has expired and whose gap has reduced.
/// Returns the count of proposals promoted from `trial` to `pending`.
pub fn evaluate_trial_proposals(store: &Store) -> Result<usize> {
    let now = Utc::now().timestamp();

    // Load trial proposals.
    let trial_ids: Vec<(i64, String, String)> = {
        let mut stmt = store.conn().prepare(
            "SELECT id, content_hash, evidence FROM proposals
             WHERE status = 'trial'
             AND CAST(json_extract(gate_signals, '$.trial_expires') AS INTEGER) <= ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![now], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut promoted = 0usize;
    for (id, _hash, evidence_str) in trial_ids {
        let evidence: Value = serde_json::from_str(&evidence_str).unwrap_or(json!({}));

        // Check if the gap has reduced (seen_count went down or stayed the same).
        let original_count = evidence.get("seen_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let query_text = evidence.get("source_query")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let current_count: i64 = if query_text.is_empty() {
            original_count // can't check without query text; promote anyway
        } else {
            store.conn().query_row(
                "SELECT seen_count FROM query_gap_log WHERE query_text = ?1",
                rusqlite::params![query_text],
                |r| r.get(0),
            ).unwrap_or(0)
        };

        // If gap is still present (count unchanged or grew), promote to `pending`.
        // If gap is gone (count dropped significantly), reject — note resolved.
        if current_count >= original_count {
            store.conn().execute(
                "UPDATE proposals SET status = 'pending' WHERE id = ?1",
                rusqlite::params![id],
            )?;
            promoted += 1;
        } else {
            store.conn().execute(
                "UPDATE proposals SET status = 'rejected',
                 gate_signals = json_set(gate_signals, '$.trial_resolved', 1)
                 WHERE id = ?1",
                rusqlite::params![id],
            )?;
        }
    }

    Ok(promoted)
}

// ── Process fidelity score (Phase 2 extension of bootstrap analysis) ─────────

/// Score a session's process fidelity: did the agent follow the ideal PROTOCOL sequence?
///
/// Ideal sequence: begin_protocol_session → get_delta → get_preferences →
///   get_anti_patterns → (get_context | list_patterns) → [work tools] → closeout_session
///
/// Returns a score 0.0–1.0 and a list of missing steps.
///
/// `penalties` optionally overrides the default per-step penalties.
/// Default: 0.2 for each required step, 0.1 for the partial step.
/// Important steps (like closeout_session) can be weighted higher.
/// Default fidelity penalty weights: closeout is weighted highest (0.25),
/// then bootstrap steps (0.15 each), then other steps (0.10 each).
const DEFAULT_FIDELITY_PENALTIES: &[(f32, &[&str])] = &[
    (0.25, &["closeout_session"]),
    (0.15, &["begin_protocol_session"]),
    (0.10, &["get_delta"]),
    (0.10, &["get_preferences"]),
    (0.10, &["get_anti_patterns"]),
    (0.10, &["get_context", "list_patterns"]),
];

/// Score a session's process fidelity: did the agent follow the ideal PROTOCOL sequence?
///
/// Ideal sequence: begin_protocol_session → get_delta → get_preferences →
///   get_anti_patterns → (get_context | list_patterns) → [work tools] → closeout_session
///
/// Returns a score 0.0–1.0 and a list of missing steps.
///
/// `penalties` optionally overrides the default per-step penalties.
/// Default: closeout=0.25, bootstrap=0.15, steps=0.10, partial=0.10.
pub fn score_process_fidelity(
    tool_sequence: &[String],
    penalties: Option<&[(f32, &[&str])]>,
) -> (f32, Vec<String>) {
    let seq_set: std::collections::HashSet<&str> = tool_sequence.iter()
        .map(|s| s.as_str())
        .collect();

    let active = penalties.unwrap_or(DEFAULT_FIDELITY_PENALTIES);

    let mut missing = Vec::new();
    let mut score = 1.0f32;

    for &(penalty, steps) in active {
        let present = steps.iter().any(|s| seq_set.contains(s));
        if !present {
            let key = if steps.len() == 1 { steps[0].to_string() }
                      else { format!("{}|{}", steps[0], steps[1]) };
            missing.push(key);
            score -= penalty;
        }
    }

    (score.max(0.0), missing)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_syntax_gate_accepts_valid_snippet() {
        let valid = "let x: u32 = 42;";
        assert!(gate_rust_syntax(valid));
    }

    #[test]
    fn rust_syntax_gate_rejects_invalid_snippet() {
        let invalid = "let x = {{{broken syntax!!";
        assert!(!gate_rust_syntax(invalid));
    }

    #[test]
    fn rust_syntax_gate_accepts_plain_text() {
        // Plain prose — no Rust signatures, should not be sent to syn.
        let prose = "AnimatedSprite has no frame method. Use set_image instead.";
        assert!(!contains_rust_snippet(prose));
    }

    #[test]
    fn process_fidelity_perfect_sequence() {
        let seq = vec![
            "begin_protocol_session", "get_delta", "get_preferences",
            "get_anti_patterns", "get_context", "semantic_search",
            "recall", "closeout_session",
        ].into_iter().map(str::to_string).collect::<Vec<_>>();
        let (score, missing) = score_process_fidelity(&seq, None);
        assert!(score >= 0.9, "perfect sequence should score >= 0.9, got {score}");
        assert!(missing.is_empty(), "no missing steps, got: {missing:?}");
    }

    #[test]
    fn process_fidelity_missing_closeout() {
        let seq = vec![
            "begin_protocol_session", "get_delta", "get_preferences", "get_anti_patterns",
        ].into_iter().map(str::to_string).collect::<Vec<_>>();
        let (score, missing) = score_process_fidelity(&seq, None);
        assert!(score < 1.0);
        assert!(missing.contains(&"closeout_session".to_string()));
    }

    #[test]
    fn duplicate_rejection_log_writes_and_reads() {
        // Use a temporary path in target/test_output/
        use std::path::PathBuf;
        let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or(".".into()))
            .join("target").join("test_output");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_rejected.jsonl");
        let _ = std::fs::write(&path, ""); // clear

        let signals = GateSignals { duplicate_check: Some(false), ..Default::default() };
        log_rejection(&path, "pref_note", "abc123", "let x = 1;", "duplicate", &signals);
        assert!(is_recently_rejected(&path, "abc123"));
        assert!(!is_recently_rejected(&path, "different_hash"));

        let _ = std::fs::remove_file(&path);
    }
}
