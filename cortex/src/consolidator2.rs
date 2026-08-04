/// Phase 1: Consolidation pipeline orchestrator.
///
/// Phase 2 additions: deterministic verification gates applied to every proposal
/// before it enters the `proposals` table. Gates live in `verify.rs`.
///
/// Stage notes:
///   1. health-check   → .cortex/health-report.json
///   2. cluster-sessions → .cortex/clusters.json
///   3. detect-skills  → .cortex/proposals/skill_*.md
///   4. propose-gaps   → gated (trial for seen_count < 5)
///   5. propose-survival → gated (dedup + survival trend)
///   6. process-fidelity → score each session; flag low-fidelity patterns
///
/// Each stage is idempotent — safe to re-run.
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::memory::Store;
use crate::miner;
use crate::prefs::Preferences;
use crate::skills;
use crate::verify;

// ── Pipeline result ───────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct PipelineResult {
    pub snapshots_read:         usize,
    pub clusters_found:         usize,
    pub skill_candidates_new:   usize,
    pub skill_drafts_written:   usize,
    pub gap_proposals:          usize,
    pub gap_trials:             usize,
    pub gap_rejected:           usize,
    pub survival_proposals:     usize,
    pub fidelity_sessions:      usize,
    pub trial_promotions:       usize,
    pub drift_report_written:   bool,
    pub high_drift_communities: usize,
    pub drift_based_proposals:  usize,
    pub meta_proposals_staged:  usize,
    pub last_run_updated:       bool,
}

impl PipelineResult {
    pub fn summary(&self) -> String {
        format!(
            "Pipeline complete: {} snapshots → {} clusters → {} skill candidates ({} drafts) | {} gap proposals ({} trials, {} rejected) | {} survival proposals | {} fidelity sessions scored | {} trial promotions | drift: {} communities ({} proposals) | meta: {} proposals staged",
            self.snapshots_read,
            self.clusters_found,
            self.skill_candidates_new,
            self.skill_drafts_written,
            self.gap_proposals,
            self.gap_trials,
            self.gap_rejected,
            self.survival_proposals,
            self.fidelity_sessions,
            self.trial_promotions,
            self.high_drift_communities,
            self.drift_based_proposals,
            self.meta_proposals_staged,
        )
    }
}

// ── Main pipeline entry ───────────────────────────────────────────────────────

pub fn run(
    store: &Store,
    repo_root: &Path,
    prefs: &Preferences,
) -> Result<PipelineResult> {
    let mut result = PipelineResult::default();

    let mined_tasks_dir = repo_root.join(".cortex").join("mined-tasks");
    let proposals_dir   = repo_root.join(".cortex").join("proposals");
    let clusters_path   = repo_root.join(".cortex").join("clusters.json");
    let health_path     = repo_root.join(".cortex").join("health-report.json");
    let rejected_log    = repo_root.join(".cortex").join("rejected-proposals.jsonl");

    std::fs::create_dir_all(&proposals_dir)?;

    // ── Phase 2: Promote any expired trial proposals before new pipeline run ─
    result.trial_promotions = verify::evaluate_trial_proposals(store)?;

    // ── Stage 1: Health report ────────────────────────────────────────────────
    let health = build_health_report(store)?;
    std::fs::write(&health_path, serde_json::to_string_pretty(&health)?)?;

    // ── Stage 2: Cluster sessions ─────────────────────────────────────────────
    let snapshots = miner::load_snapshots(&mined_tasks_dir)?;
    result.snapshots_read = snapshots.len();

    let threshold   = 0.55f32;  // lower than Plan to avoid over-splitting sparse data
    let clusters    = miner::cluster_snapshots(&snapshots, threshold);
    result.clusters_found = clusters.len();

    std::fs::write(&clusters_path, miner::clusters_to_json(&clusters))?;

    // ── Stage 3: Detect skill candidates ─────────────────────────────────────
    let min_occ = prefs.consolidation.skill_candidate_min_occurrences as usize;
    let candidates = skills::detect_skill_candidates(store, &clusters, min_occ as u32)?;
    result.skill_candidates_new = candidates.len();

    for candidate in &candidates {
        match skills::draft_skill_file(
            &candidate.name,
            &candidate.tool_sequence,
            candidate.occurrence_count,
            candidate.confidence,
            &proposals_dir,
            &prefs.skills.skills_dir,
        ) {
            Ok(path) => {
                let _ = skills::set_skill_draft_path(store, &candidate.name, &path);
                result.skill_drafts_written += 1;
            }
            Err(e) => {
                eprintln!("[consolidator] warn: could not draft skill {}: {e}", candidate.name);
            }
        }
    }

    // ── Stage 4: Gap-driven proposals (Phase 2: gated) ───────────────────────
    let gap_proposals = skills::detect_gap_proposals(store, 3)?;
    result.gap_proposals = gap_proposals.len();

    for (i, gap) in gap_proposals.iter().enumerate() {
        let content_hash = format!("{:x}", simple_hash(
            format!("gap:{}:{}", gap.tool_name, gap.query_text).as_bytes()
        ));

        // Phase 2 gate: check rejection log first.
        if verify::is_recently_rejected(&rejected_log, &content_hash) {
            result.gap_rejected += 1;
            continue;
        }

        let evidence = json!({
            "source": "query_gap_log",
            "seen_count": gap.seen_count,
            "source_query": gap.query_text,
        });

        let (outcome, signals) = verify::run_gates(
            store, "pref_note", &content_hash, &gap.proposed_note, &evidence
        );

        match outcome {
            verify::GateOutcome::Pass => {
                // Stage immediately.
                let _ = store.conn().execute(
                    "INSERT OR IGNORE INTO proposals
                         (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
                     VALUES ('pref_note', ?1, '.cortex/prefs.toml', ?2, ?3, 'pending', ?4)",
                    rusqlite::params![
                        content_hash, gap.proposed_note,
                        evidence.to_string(), signals.to_json(),
                    ],
                );
                // Write JSON file for review.
                let path = proposals_dir.join(format!("pref_gap_{i:02}.json"));
                let _ = std::fs::write(path, serde_json::to_string_pretty(&json!({
                    "proposal_type": "pref_note",
                    "tool_name": gap.tool_name,
                    "query_text": gap.query_text,
                    "seen_count": gap.seen_count,
                    "proposed_note": gap.proposed_note,
                }))?);
            }
            verify::GateOutcome::Trial { trial_days, .. } => {
                // Stage as trial; will be promoted after trial period expires.
                let _ = store.conn().execute(
                    "INSERT OR IGNORE INTO proposals
                         (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
                     VALUES ('pref_note', ?1, '.cortex/prefs.toml', ?2, ?3, 'trial', ?4)",
                    rusqlite::params![
                        content_hash, gap.proposed_note,
                        evidence.to_string(), signals.to_json(),
                    ],
                );
                let _ = verify::stage_as_trial(store, &content_hash, trial_days);
                result.gap_trials += 1;
            }
            verify::GateOutcome::Reject(reason) => {
                verify::log_rejection(
                    &rejected_log, "pref_note", &content_hash,
                    &gap.proposed_note, &reason, &signals,
                );
                result.gap_rejected += 1;
            }
        }
    }

    // ── Stage 5: Survival-based proposals (Phase 2: gated) ───────────────────
    result.survival_proposals = propose_survival_gated(store, &proposals_dir, &rejected_log)?;

    // ── Stage 6: Process fidelity scoring (Phase 2) ──────────────────────────
    result.fidelity_sessions = score_session_fidelity(store)?;

    // ── Stage 7: Graphify drift analysis (Phase 3) ───────────────────────────
    {
        let snapshots_dir = repo_root.join(".graphify-output").join("snapshots");
        let current_graph = repo_root.join(".graphify-output").join("graph.json");
        match crate::graph_diff::run_graph_diff(&snapshots_dir, &current_graph) {
            Ok(Some(report)) => {
                result.drift_report_written = true;
                result.high_drift_communities = report.high_drift_communities.len();

                // Write drift report to .cortex/drift-report.json
                let drift_path = repo_root.join(".cortex").join("drift-report.json");
                let _ = std::fs::write(&drift_path, serde_json::to_string_pretty(
                    &crate::graph_diff::drift_report_to_json(&report),
                )?);

                // Generate ONE drift digest proposal per pipeline run instead of one
                // proposal per community. A whole-graph rebuild after a stale period
                // makes virtually every community "drift" — the per-community version
                // once flooded the review funnel with 1300+ ungated pending proposals.
                let weights = crate::graph_diff::compute_community_weights(&report);
                let mut hot: Vec<&crate::graph_diff::CommunityWeight> = weights.iter()
                    .filter(|w| w.priority_boost >= 2.0)
                    .collect();
                hot.sort_by(|a, b| b.drift_score.partial_cmp(&a.drift_score)
                    .unwrap_or(std::cmp::Ordering::Equal));

                if !hot.is_empty() {
                    // Cap: never allow drift digests to pile up unreviewed.
                    let pending_drift: i64 = store.conn().query_row(
                        "SELECT COUNT(*) FROM proposals
                         WHERE proposal_type = 'drift_flag' AND status IN ('pending','trial')",
                        [], |r| r.get(0),
                    ).unwrap_or(0);

                    if pending_drift >= 3 {
                        eprintln!("[consolidator] drift: {} digests already pending — skipping new digest (review or reject them first)", pending_drift);
                    } else {
                        let top: Vec<_> = hot.iter().take(10).collect();
                        // Hash over the top community ids+scores: identical drift across
                        // runs dedups via the duplicate gate; changed drift stages fresh.
                        let hash_src: String = top.iter()
                            .map(|w| format!("{}:{:.2}", w.community_id, w.drift_score))
                            .collect::<Vec<_>>().join(",");
                        let content_hash = format!("{:x}", simple_hash(
                            format!("drift-digest:{hash_src}").as_bytes()
                        ));

                        let lines: Vec<String> = top.iter()
                            .map(|w| format!("community {} (drift {:.2})", w.community_id, w.drift_score))
                            .collect();
                        let proposed_text = format!(
                            "Graph drift digest: {} communities show significant drift. Top: {}. \
                             Review these areas for stale patterns/ADRs and rebuild-related churn.",
                            hot.len(), lines.join("; "),
                        );
                        let evidence = json!({
                            "source": "graph_drift",
                            "high_drift_count": hot.len(),
                            "top_communities": top.iter().map(|w| json!({
                                "community_id": w.community_id,
                                "drift_score": w.drift_score,
                            })).collect::<Vec<_>>(),
                        });

                        // Route through the same deterministic gates as every other proposal.
                        let (outcome, signals) = verify::run_gates(
                            store, "drift_flag", &content_hash, &proposed_text, &evidence,
                        );
                        match outcome {
                            verify::GateOutcome::Pass => {
                                let _ = store.conn().execute(
                                    "INSERT OR IGNORE INTO proposals
                                     (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
                                     VALUES ('drift_flag', ?1, '.graphify-output/graph.json', ?2, ?3, 'pending', ?4)",
                                    rusqlite::params![
                                        content_hash, proposed_text,
                                        evidence.to_string(), signals.to_json(),
                                    ],
                                );
                                result.drift_based_proposals += 1;
                            }
                            verify::GateOutcome::Reject(reason) => {
                                verify::log_rejection(
                                    &rejected_log, "drift_flag", &content_hash,
                                    &proposed_text, &reason, &signals,
                                );
                            }
                            verify::GateOutcome::Trial { .. } => { /* drift digests don't trial */ }
                        }
                    }
                }
            }
            Ok(None) => { /* no previous snapshot — skip */ }
            Err(e) => {
                eprintln!("[consolidator] warn: graph drift analysis failed: {e}");
            }
        }
    }

    // ── Stage 8: Meta-analysis (Phase 5b) ────────────────────────────────────
    {
        let rejected_log = repo_root.join(".cortex").join("rejected-proposals.jsonl");
        match crate::meta::build_meta_report(store, &rejected_log) {
            Ok(report) => {
                if report.total_proposals > 0 {
                    match crate::meta::stage_meta_proposals(store, &report) {
                        Ok(staged) => {
                            result.meta_proposals_staged = staged;
                        }
                        Err(e) => {
                            eprintln!("[consolidator] warn: stage_meta_proposals failed: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[consolidator] warn: meta report failed: {e}");
            }
        }
    }

    // ── Update last-run timestamp ─────────────────────────────────────────────
    let _ = store.conn().execute(
        "INSERT INTO annotations (topic, body, tags, added_at)
         VALUES ('consolidation-last-run', ?1, '[]', ?2)
         ON CONFLICT DO NOTHING",
        rusqlite::params![
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    );
    // Use REPLACE semantics via a dedicated annotation update.
    let _ = store.conn().execute(
        "UPDATE annotations SET body = ?1, added_at = ?2
         WHERE topic = 'consolidation-last-run'",
        rusqlite::params![Utc::now().to_rfc3339(), Utc::now().to_rfc3339()],
    );
    result.last_run_updated = true;

    Ok(result)
}

// ── Health report ─────────────────────────────────────────────────────────────

fn build_health_report(store: &Store) -> Result<Value> {
    let patterns: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM patterns", [], |r| r.get(0),
    ).unwrap_or(0);

    let low_survival: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM patterns WHERE survival_rate < 0.4", [], |r| r.get(0),
    ).unwrap_or(0);

    let anti_patterns: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM anti_patterns", [], |r| r.get(0),
    ).unwrap_or(0);

    let pending_obs: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM pending_observations", [], |r| r.get(0),
    ).unwrap_or(0);

    let pending_proposals: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'pending'", [], |r| r.get(0),
    ).unwrap_or(0);

    let orphaned_sessions: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM protocol_sessions WHERE closeout_run = 0
         AND started_at < (unixepoch() - 7200)",
        [], |r| r.get(0),
    ).unwrap_or(0);

    let hot_gaps: Vec<Value> = {
        let cutoff = Utc::now().timestamp() - 7 * 86400;
        let mut stmt = store.conn().prepare(
            "SELECT query_text, seen_count FROM query_gap_log
             WHERE last_seen_at >= ?1 ORDER BY seen_count DESC LIMIT 5"
        ).unwrap();
        stmt.query_map(rusqlite::params![cutoff], |r| {
            Ok(json!({"query": r.get::<_,String>(0)?, "count": r.get::<_,i64>(1)?}))
        }).unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };

    Ok(json!({
        "generated_at":     Utc::now().to_rfc3339(),
        "patterns":         patterns,
        "low_survival":     low_survival,
        "anti_patterns":    anti_patterns,
        "pending_obs":      pending_obs,
        "pending_proposals": pending_proposals,
        "orphaned_sessions": orphaned_sessions,
        "hot_gaps":         hot_gaps,
    }))
}

// ── Survival proposals (Phase 2: gated) ──────────────────────────────────────

fn propose_survival_gated(
    store: &Store,
    proposals_dir: &Path,
    rejected_log: &Path,
) -> Result<usize> {
    let mut stmt = store.conn().prepare(
        "SELECT id, name, intent, body, use_count, reverted_count, survival_rate
         FROM patterns
         WHERE use_count >= 3 AND survival_rate < 0.4
         ORDER BY survival_rate ASC
         LIMIT 10"
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, f32>(6)?,
        ))
    })?;

    let mut count = 0usize;
    for row in rows {
        let (id, name, intent, body, uses, reverts, rate) = row?;
        let content_hash = format!("{:x}", simple_hash(format!("dying:{id}:{name}").as_bytes()));

        if verify::is_recently_rejected(rejected_log, &content_hash) { continue; }

        let evidence = json!({"pattern_id": id, "use_count": uses, "reverted_count": reverts});
        let proposed_text = format!(
            "Pattern '{name}' has {rate:.0}% survival after {uses} uses — review for removal or anti-pattern conversion"
        );

        let (outcome, signals) = verify::run_gates(
            store, "anti_pattern", &content_hash, &proposed_text, &evidence
        );

        match outcome {
            verify::GateOutcome::Pass => {
                let safe_name = name.replace(['/', '\\', ' '], "-");
                let path = proposals_dir.join(format!("ap_dying_{safe_name}.json"));
                let _ = std::fs::write(path, serde_json::to_string_pretty(&json!({
                    "proposal_type": "dying_pattern",
                    "pattern_id": id, "name": name, "intent": intent,
                    "body_preview": body.chars().take(200).collect::<String>(),
                    "use_count": uses, "reverted_count": reverts, "survival_rate": rate,
                }))?);
                let _ = store.conn().execute(
                    "INSERT OR IGNORE INTO proposals
                         (proposal_type, content_hash, target_file, proposed_text, evidence, status, gate_signals)
                     VALUES ('anti_pattern', ?1, 'patterns', ?2, ?3, 'pending', ?4)",
                    rusqlite::params![content_hash, proposed_text, evidence.to_string(), signals.to_json()],
                );
                count += 1;
            }
            verify::GateOutcome::Reject(reason) => {
                verify::log_rejection(rejected_log, "anti_pattern", &content_hash, &proposed_text, &reason, &signals);
            }
            verify::GateOutcome::Trial { .. } => {}
        }
    }

    Ok(count)
}

/// CLI wrapper called by `propose-survival` and tests.
pub fn propose_survival_pub(store: &Store, proposals_dir: &Path) -> Result<usize> {
    let rejected_log = proposals_dir.parent()
        .unwrap_or(proposals_dir)
        .join("rejected-proposals.jsonl");
    propose_survival_gated(store, proposals_dir, &rejected_log)
}

// ── Stage 6: Process fidelity scoring ────────────────────────────────────────

fn score_session_fidelity(store: &Store) -> Result<usize> {
    let mut stmt = store.conn().prepare(
        "SELECT session_key, tool_sequence FROM session_snapshots
         WHERE json_extract(marker_counts, '$.fidelity_score') IS NULL
         ORDER BY created_at DESC LIMIT 50"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let sessions: Vec<(String, String)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let scored = sessions.len();

    for (session_key, seq_json) in sessions {
        let seq: Vec<String> = serde_json::from_str(&seq_json).unwrap_or_default();
        let (score, missing) = verify::score_process_fidelity(&seq, None);
        let _ = store.conn().execute(
            "UPDATE session_snapshots
             SET marker_counts = json_set(marker_counts,
                 '$.fidelity_score', ?1, '$.fidelity_missing', ?2)
             WHERE session_key = ?3",
            rusqlite::params![
                score,
                serde_json::to_string(&missing).unwrap_or_default(),
                session_key,
            ],
        );
    }
    Ok(scored)
}

/// Returns true if the last consolidation run was more than `staleness_hours` ago.
/// Returns true if the last consolidation run was more than `staleness_hours` ago,
/// OR if many sessions occurred since the last run (session-frequency scaling).
///
/// Session-frequency scaling: if >= 5 sessions completed since the last run,
/// treat as stale regardless of clock time. This ensures high-activity periods
/// get more frequent consolidation.
pub fn is_stale(store: &Store, staleness_hours: u32) -> bool {
    let cutoff = Utc::now().timestamp() - (staleness_hours as i64 * 3600);

    let last_run: Option<String> = store.conn().query_row(
        "SELECT body FROM annotations WHERE topic = 'consolidation-last-run' LIMIT 1",
        [], |r| r.get(0),
    ).ok().flatten();

    let last_ts = match last_run {
        None => return true, // never run
        Some(ref ts) => chrono::DateTime::parse_from_rfc3339(ts)
            .map(|dt| dt.timestamp())
            .unwrap_or(0),
    };

    // Clock-based staleness check.
    let clock_stale = last_ts < cutoff;
    if clock_stale {
        return true;
    }

    // Session-frequency scaling: count sessions closed since last run.
    let recent_sessions: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM protocol_sessions
         WHERE closeout_run = 1 AND closed_at >= ?1",
        rusqlite::params![last_ts],
        |r| r.get(0),
    ).unwrap_or(0);

    // If >= 5 sessions completed since last run, treat as stale.
    recent_sessions >= 5
}

// ── Simple hash for content dedup ─────────────────────────────────────────────

fn simple_hash(data: &[u8]) -> u64 {
    // FNV-1a 64-bit hash — no external deps needed.
    let mut h: u64 = 14695981039346656037u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211u64);
    }
    h
}

#[derive(Debug, Clone)]
pub struct ProposalRow {
    pub id:            i64,
    pub proposal_type: String,
    pub proposed_text: String,
    pub evidence:      String,
    pub created_at:    i64,
}

/// Load all pending proposals from the DB.
pub fn load_pending_proposals(store: &Store) -> Result<Vec<ProposalRow>> {
    let mut stmt = store.conn().prepare(
        "SELECT id, proposal_type, proposed_text, evidence, created_at
         FROM proposals WHERE status = 'pending'
         ORDER BY created_at ASC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProposalRow {
            id:            row.get(0)?,
            proposal_type: row.get(1)?,
            proposed_text: row.get(2)?,
            evidence:      row.get(3)?,
            created_at:    row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Set a proposal status (pending / approved / rejected).
pub fn set_proposal_status(store: &Store, id: i64, status: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    let (reviewed_col, committed_col) = match status {
        "approved" => (Some(now), Some(now)),
        "rejected" => (Some(now), None),
        _          => (None, None),
    };

    match (reviewed_col, committed_col) {
        (Some(r), Some(c)) => {
            store.conn().execute(
                "UPDATE proposals SET status=?1, reviewed_at=?2, committed_at=?3 WHERE id=?4",
                rusqlite::params![status, r, c, id],
            )?;
        }
        (Some(r), None) => {
            store.conn().execute(
                "UPDATE proposals SET status=?1, reviewed_at=?2 WHERE id=?3",
                rusqlite::params![status, r, id],
            )?;
        }
        _ => {
            store.conn().execute(
                "UPDATE proposals SET status=?1 WHERE id=?2",
                rusqlite::params![status, id],
            )?;
        }
    }
    Ok(())
}

/// Format the pending proposals as a human-readable review list.
pub fn format_pending_proposals(proposals: &[ProposalRow]) -> String {
    if proposals.is_empty() {
        return "No pending cross-session proposals. Run consolidation first.\n".to_string();
    }
    let mut out = format!("{} pending proposal(s):\n\n", proposals.len());
    for p in proposals {
        let age_hours = (Utc::now().timestamp() - p.created_at) / 3600;
        out.push_str(&format!(
            "  [{}] type:{} ({} hours ago)\n  {}\n\n",
            p.id, p.proposal_type, age_hours,
            p.proposed_text.chars().take(120).collect::<String>(),
        ));
    }
    out
}
