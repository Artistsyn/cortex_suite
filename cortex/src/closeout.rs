/// Phase 0D: Session closeout logic.
///
/// closeout_session is the single MCP tool that replaces the 7-step manual checklist.
///
/// Markers commit through the same gates whichever way they arrive -- captured
/// from the transcript when written (capture.rs), passed as `markers_text`, or
/// scraped from a host's chat store. Since 2026-09-30 they commit without a
/// per-item approval while `loop.auto_commit` is on (the default): the approval
/// passed 99.3-100% of what reached it, and the protocol around it lost 30% of
/// what was approved. Every such commit is a `loop_changes` row, audited by
/// sample (`cortex knowledge audit`); too many bad verdicts switch the gate back
/// on, and then inline_approve=true ("KNOWLEDGE COMMITTED") is required again.
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::params;
use serde_json::json;

use crate::markers::{self, KnowledgeMarker};
use crate::memory::Store;
use crate::model::{AntiPattern, Pattern};
use crate::session_store;

// ── Closeout result ───────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct CloseoutResult {
    pub patterns_committed:     usize,
    pub anti_patterns_committed: usize,
    pub corrections_committed:   usize,
    pub walls_committed:         usize,
    pub adrs_committed:          usize,
    pub prefs_notes_committed:   usize,
    pub skill_candidates_staged: usize,
    pub markers_staged:          usize,
    /// Committed without a person approving this closeout (loop.auto_commit).
    pub auto_committed:          bool,
    /// Markers already in the store (an earlier capture or closeout).
    pub markers_known:           usize,
    pub outcome_logged:          bool,
    /// Patterns whose use/reverted telemetry was updated from this session's
    /// targeted retrievals × outcome (survival feedback loop).
    pub patterns_scored:         usize,
    pub graph_snapshot_written:  bool,
    pub session_snapshot_written: bool,
    pub mirror_written:          bool,
    /// Things the caller should be told that are not counts — a graph rebuilt
    /// before snapshotting, or a snapshot skipped because it could not be.
    /// Surfaced so a skipped step is visible rather than silently absent.
    pub notes:                   Vec<String>,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Run the full session closeout.
///
/// - `inline_approve`: if true, all extracted markers are immediately committed
///   to their target DB tables. Set only when the user has typed "KNOWLEDGE COMMITTED".
///   If false, markers are staged in knowledge_markers with promoted=0.
/// - `markers_text`: when provided (non-empty), CORTEX-* markers are parsed directly
///   from this text instead of scraping a host-specific chat store. This is the
///   platform-independent capture path: any agent (Claude Code, Copilot, Continue)
///   passes the text containing its own markers. Falls back to the VS Code session
///   store, then the mcp_calls DB, only when this is None/empty.
pub fn run_closeout(
    store: &Store,
    session_key: &str,
    outcome_type: &str,
    error_text: Option<&str>,
    diff_symbols: Option<&str>,
    inline_approve: bool,
    repo_root: &Path,
    prefs_path: Option<&Path>,
    markers_text: Option<&str>,
) -> Result<CloseoutResult> {
    let mut result = CloseoutResult::default();

    // ── Step 1: Flush knowledge markers ──────────────────────────────────────
    // Priority: (1) markers passed in directly by the agent (platform-independent),
    // (2) VS Code Copilot session store, (3) recent mcp_calls in the DB.
    let markers = match markers_text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => markers::parse_markers(text),
        None => extract_session_markers().unwrap_or_else(|e| {
            eprintln!("[closeout] warn: session store unavailable ({e}) — no markers from store");
            // Fallback: try to extract markers from recent mcp_calls in DB
            extract_markers_from_mcp_calls(store).unwrap_or_default()
        }),
    };
    let extracted_markers = markers.clone();

    let auto = crate::loop_ledger::auto_commit_enabled(store);
    if inline_approve || auto {
        result.auto_committed = !inline_approve;
        // A person's approval also releases what capture staged while the
        // gate was on; an automatic closeout never does.
        let mut to_commit: Vec<(Option<i64>, KnowledgeMarker)> = Vec::new();
        if inline_approve {
            for (row, marker) in staged_captures(store) {
                to_commit.push((Some(row), marker));
            }
        }
        to_commit.extend(markers.iter().cloned().map(|m| (None, m)));
        let class = if inline_approve { "approved" } else { "entry" };
        for (staged_row, marker) in &to_commit {
            let outcome = commit_one(store, session_key, marker, "", prefs_path, class, "closeout", staged_row.is_none(), Some(Utc::now()));
            if let Some(row) = staged_row {
                if matches!(outcome, CommitOutcome::Committed { .. } | CommitOutcome::Updated { .. } | CommitOutcome::Known) {
                    let _ = store.conn().execute("UPDATE knowledge_markers SET promoted = 1 WHERE id = ?1", params![row]);
                }
            }
            match outcome {
                CommitOutcome::Committed { merged, .. } => {
                    count_committed(&mut result, marker);
                    result.notes.extend(merged);
                }
                CommitOutcome::Updated { replaced, target } => {
                    count_committed(&mut result, marker);
                    result.notes.push(format!("{target} replaces {replaced} (same name, new text)"));
                }
                CommitOutcome::Known => result.markers_known += 1,
                CommitOutcome::Refused(e) => {
                    // Emitted but did not land; say why in the report:
                    // stderr reaches no one.
                    eprintln!("[closeout] warn: failed to commit marker: {e}");
                    result.notes.push(format!(
                        "{} marker \"{}\" was not committed: {e}",
                        marker.marker_type(),
                        marker.display_name()
                    ));
                }
            }
        }
        // Mark knowledge as flushed and inline-approved in protocol_sessions.
        let _ = store.conn().execute(
            "UPDATE protocol_sessions
             SET knowledge_markers_flushed = 1, inline_approved = 1
             WHERE session_key = ?1",
            params![session_key],
        );
    } else {
        // Tier 2: stage markers for later review.
        for marker in &markers {
            let _ = stage_marker(store, session_key, marker);
            result.markers_staged += 1;
        }
        let _ = store.conn().execute(
            "UPDATE protocol_sessions
             SET knowledge_markers_flushed = 1
             WHERE session_key = ?1",
            params![session_key],
        );
    }

    // ── Step 2: Log outcome ───────────────────────────────────────────────────
    if let Ok(outcome_id) = store.log_outcome(session_key, outcome_type, error_text, diff_symbols) {
        // Auto-apply weighted evidence.
        let _ = store.conn().execute(
            "INSERT OR IGNORE INTO outcome_applied_log (outcome_id, session_id, applied_at)
             VALUES (?1, ?2, unixepoch())",
            params![outcome_id, session_key],
        );
        result.outcome_logged = true;
    }

    // ── Step 2b: Retrieval × outcome → pattern survival telemetry ────────────
    // Every pattern this session retrieved via a TARGETED lookup (recall topic
    // match / get_context relevance — not bulk list_patterns browsing) gets a
    // usage tick; failed build/test sessions also tick reverted. This is what
    // makes survival_rate a real signal instead of a default-100% placeholder.
    result.patterns_scored = apply_retrieval_outcomes(store, session_key, outcome_type)
        .unwrap_or_else(|e| {
            eprintln!("[closeout] warn: retrieval-outcome telemetry failed: {e}");
            0
        });

    // ── Step 3: Run git-review (pattern relevance scan) ───────────────────────
    if let Ok(deltas) = crate::git::head_deltas_with_options(repo_root, &crate::git::DeltaOptions {
        include: None,
        exclude: Some("assets".to_string()),
        max_files: 8,
        max_patch_lines: 20,
    }) {
        // Scan deltas for pattern/anti-pattern relevance keywords (simple approach).
        let delta_text: String = deltas.iter()
            .map(|d| format!("{} {}", d.path, d.summary))
            .collect::<Vec<_>>()
            .join(" ");
        // Annotate the session snapshot with touched domains.
        let _ = delta_text; // used in snapshot below
    }

    // ── Step 4: Write Graphify graph snapshot ─────────────────────────────────
    //
    // Only if the graph still describes the code. Snapshotting a stale
    // graph.json is worse than snapshotting nothing: every later drift
    // comparison is then measured against a file that predates the changes, and
    // the pipeline reports drift everywhere. That is exactly what happened — a
    // graph 15 days older than the source produced a digest claiming 1303
    // communities had drifted, which was noise the meta-analyser then correctly
    // flagged as "zero proposals approved out of 1303".
    //
    // So: rebuild it when a rebuild is possible, and when it is not, skip the
    // snapshot and say why rather than emitting a signal that cannot be trusted.
    let graph_src = repo_root.join(".graphify-output").join("graph.json");
    if graph_src.exists() {
        if let Some(reason) = graph_is_stale(repo_root, &graph_src) {
            match rebuild_graph(repo_root) {
                Ok(()) => result.notes.push(format!(
                    "graph rebuilt before snapshot ({reason})"
                )),
                Err(e) => {
                    // No rebuild, no snapshot. A drift measurement against this
                    // file would be fiction.
                    result.notes.push(format!(
                        // The suggested command MUST carry --output. Without it
                        // graphify writes to ~/.graphify-rs/<project>-<hash>/ and
                        // .graphify-output/graph.json stays stale, so following
                        // this advice literally would leave the user in exactly
                        // the state the message is asking them to fix.
                        "graph snapshot SKIPPED — {reason}, and rebuild failed: {}. \
                         Run: graphify-rs build --path . --code-only --format json --no-llm --update --output .graphify-output",
                        crate::closeout::one_line(&e.to_string())
                    ));
                }
            }
        }
    }
    if graph_src.exists() && graph_is_stale(repo_root, &graph_src).is_none() {
        let snapshots_dir = repo_root.join(".graphify-output").join("snapshots");
        if std::fs::create_dir_all(&snapshots_dir).is_ok() {
            let ts = Utc::now().format("%Y%m%d_%H%M%S");
            let dest = snapshots_dir.join(format!("graph_{ts}.json"));
            if std::fs::copy(&graph_src, &dest).is_ok() {
                result.graph_snapshot_written = true;
                // `[consolidation] graph_snapshot_days` was parsed and documented
                // but never read here; the age limit was a literal 30.
                let max_age_days = prefs_path
                    .and_then(|p| crate::prefs::load(p).ok())
                    .map(|p| u64::from(p.consolidation.graph_snapshot_days))
                    .unwrap_or(30);
                prune_old_snapshots(&snapshots_dir, max_age_days);
                let _ = store.conn().execute(
                    "UPDATE protocol_sessions SET graph_snapshot_written = 1 WHERE session_key = ?1",
                    params![session_key],
                );
            }
        }
    }

    // ── Step 5: Write session snapshot ────────────────────────────────────────
    let snapshot_path = write_session_snapshot(
        store, session_key, outcome_type, &extracted_markers, repo_root
    ).unwrap_or_default();
    if !snapshot_path.is_empty() {
        result.session_snapshot_written = true;
    }

    // ── Step 6: Write agent-memory mirror ─────────────────────────────────────
    // Enforce max mirror files before writing.
    let mirror_dir = repo_root.join(".agent-memory").join("mirrors").join("repo");
    if std::fs::create_dir_all(&mirror_dir).is_ok() {
        // Prune oldest mirrors if over limit (max 200).
        const MAX_MIRROR_FILES: usize = 200;
        let mut mirrors: Vec<_> = std::fs::read_dir(&mirror_dir)
            .into_iter()
            .flat_map(|rd| rd.flatten())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        mirrors.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        while mirrors.len() > MAX_MIRROR_FILES {
            if let Some(old) = mirrors.pop() {
                let _ = std::fs::remove_file(&old);
            }
        }

        let date = Utc::now().format("%Y-%m-%d");
        let mirror_path = mirror_dir.join(format!("session-closeout-{date}.md"));
        if let Ok(content) = build_mirror_content(
            session_key, outcome_type, &extracted_markers, inline_approve,
        ) {
            if std::fs::write(&mirror_path, content).is_ok() {
                result.mirror_written = true;
            }
        }
    }

    // ── Step 7: Mark protocol session as closed ───────────────────────────────
    let now = Utc::now().timestamp();
    let _ = store.conn().execute(
        "UPDATE protocol_sessions
         SET closeout_run = 1, outcome_type = ?1, closed_at = ?2
         WHERE session_key = ?3",
        params![outcome_type, now, session_key],
    );

    Ok(result)
}

// ── Retrieval × outcome telemetry ────────────────────────────────────────────

/// Feed session outcome back into pattern use/reverted counts.
///
/// Patterns retrieved via targeted lookups this session (`recall` topic match,
/// `get_context` relevance hit) are treated as "engaged":
///   - build_pass            → use_count + 1
///   - build_fail/test_fail  → use_count + 1 AND reverted_count + 1
///   - research_only / review_findings → no telemetry (nothing was exercised)
///
/// Capped at 12 patterns per session to keep one noisy session from swinging
/// the whole store. Returns the number of patterns updated.
fn apply_retrieval_outcomes(store: &Store, session_key: &str, outcome_type: &str) -> Result<usize> {
    // The build already answered this question, and more honestly.
    //
    // Test outcomes are observed continuously from the compaction hook, so by
    // the time a session closes its patterns are usually scored from what the
    // compiler actually said rather than from the outcome_type the agent
    // reports. Applying both would count one session twice — and closeout is
    // the weaker of the two, since it is a self-assessment made after the fact.
    if crate::test_signal::already_scored(store, session_key) {
        return Ok(0);
    }

    let (use_delta, reverted_delta): (i64, i64) = match outcome_type {
        "build_pass"              => (1, 0),
        "build_fail" | "test_fail" => (1, 1),
        _                          => return Ok(0),
    };

    let pattern_ids: Vec<i64> = {
        let mut stmt = store.conn().prepare(
            "SELECT DISTINCT entry_id FROM session_retrieval_log
             WHERE session_id = ?1 AND entry_table = 'patterns'
               AND tool_name IN ('recall', 'get_context', 'list_patterns_hint')
             LIMIT 12",
        )?;
        let rows = stmt.query_map(params![session_key], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut updated = 0usize;
    for id in &pattern_ids {
        let touched = store.conn().execute(
            "UPDATE patterns
             SET use_count = use_count + ?1, reverted_count = reverted_count + ?2
             WHERE id = ?3",
            params![use_delta, reverted_delta, id],
        )?;
        if touched > 0 {
            let _ = store.recompute_pattern_survival(*id);
            updated += 1;
        }
    }
    Ok(updated)
}

// ── Marker extraction from session store ────────────────────────────────────

/// Scan the VS Code session store for CORTEX-* markers in recent turns.
fn extract_session_markers() -> Result<Vec<KnowledgeMarker>> {
    let store_path = session_store::find_session_store()
        .context("VS Code session store not found")?;

    let conn = session_store::open_readonly(&store_path)?;
    let responses = session_store::recent_assistant_responses(&conn, 50)?;

    let all_text = responses.join("\n\n---\n\n");
    Ok(markers::parse_markers(&all_text))
}

// ── Commit a marker to its target DB table ────────────────────────────────────

/// Returns true if the marker was committed, false if it was skipped (e.g. duplicate).
pub(crate) fn commit_marker(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    prefs_path: Option<&Path>,
) -> Result<bool> {
    commit_marker_at(store, session_key, marker, prefs_path, Utc::now())
}

/// `commit_marker`, dating patterns and anti-patterns `at` -- when the marker
/// was written, which for a captured or backfilled marker is not now.
pub(crate) fn commit_marker_at(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    prefs_path: Option<&Path>,
    at: chrono::DateTime<Utc>,
) -> Result<bool> {
    match marker {
        KnowledgeMarker::Pattern { name, .. } => {
            // Check for duplicate name.
            let exists: bool = store.conn().query_row(
                "SELECT COUNT(*) > 0 FROM patterns WHERE name = ?1",
                params![name], |r| r.get::<_, bool>(0),
            ).unwrap_or(false);
            if exists { return Ok(false); }

            let mut p = pattern_from_marker(marker).expect("a Pattern marker");
            p.approved_at = at;
            store.insert_pattern(&p)?;
            mark_promoted_nonfatal(store, session_key, "pattern", name);
            Ok(true)
        }

        KnowledgeMarker::AntiPattern { description, wrong, correct, tags } => {
            // A trap without a remedy teaches nothing. The parser fills in
            // "see body above" when a marker has no `correct:` line of its own
            // (often a mismatched closing tag); refuse it with the reason rather
            // than store a placeholder the store-health check then flags.
            if correct.trim().is_empty() || correct.trim() == "see body above" {
                anyhow::bail!(
                    "no usable remedy: the marker has no `correct:` line of its own \
                     (check that it closes with [/CORTEX-AP])"
                );
            }
            // Check for duplicate description.
            let exists: bool = store.conn().query_row(
                "SELECT COUNT(*) > 0 FROM anti_patterns WHERE description = ?1",
                params![description], |r| r.get::<_, bool>(0),
            ).unwrap_or(false);
            if exists { return Ok(false); }

            let ap = AntiPattern {
                id: None,
                description: description.clone(),
                wrong: wrong.clone(),
                correct: correct.clone(),
                tags: tags.clone(),
                added_at: at,
                hash: None,
                superseded_by: None,
            };
            store.insert_anti_pattern(&ap)?;
            mark_promoted_nonfatal(store, session_key, "anti_pattern", description);
            Ok(true)
        }

        KnowledgeMarker::Wall(new) => {
            // Same rules as record_wall; a marker that fails them is refused
            // with the reason rather than stored half-formed.
            if crate::walls::find_by_claim(store, &new.claim)?.is_some() {
                return Ok(false);
            }
            crate::walls::record(store, new.clone())?;
            mark_promoted_nonfatal(store, session_key, "wall", &new.claim.chars().take(60).collect::<String>());
            Ok(true)
        }

        KnowledgeMarker::Correction { attempted, reason, fix, tags } => {
            // Keyed on what was attempted: the table's own key also includes the
            // reason, so a restatement with other wording became a second row
            // (found replaying transcripts, 2026-09-30).
            let exists: bool = store.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM self_corrections WHERE lower(trim(attempted)) = lower(trim(?1)))",
                params![attempted], |r| r.get(0),
            ).unwrap_or(false);
            if exists { return Ok(false); }
            store.insert_self_correction(attempted, reason, fix, tags)?;
            mark_promoted_nonfatal(store, session_key, "correction", attempted);
            Ok(true)
        }

        KnowledgeMarker::Adr { title, context, decision, tags } => {
            use crate::model::Adr;
            // One ADR per title; a replayed marker used to take a new number.
            let exists: bool = store.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM adrs WHERE lower(trim(title)) = lower(trim(?1)))",
                params![title], |r| r.get(0),
            ).unwrap_or(false);
            if exists { return Ok(false); }
            let number = store.next_adr_number()?;
            let adr = Adr {
                id: None,
                adr_number: number,
                title: title.clone(),
                status: "accepted".to_string(),
                context: context.clone(),
                decision: decision.clone(),
                reasoning: String::new(),
                alternatives: String::new(),
                consequences: String::new(),
                concept_tags: tags.clone(),
                superseded_by: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            store.insert_adr(&adr)?;
            mark_promoted_nonfatal(store, session_key, "adr", title);
            Ok(true)
        }

        KnowledgeMarker::PrefsNote { body, tags } => {
            // Append to prefs.toml notes array if a path is provided.
            if let Some(path) = prefs_path {
                if let Ok(mut prefs) = crate::prefs::load(path) {
                    // The same note arriving twice (captured, then passed at
                    // closeout) must not be appended twice.
                    let head: String = body.chars().take(60).collect();
                    if prefs.project.notes.iter().any(|n| n.starts_with(&head)) {
                        return Ok(false);
                    }
                    // Add trust annotation.
                    let dated = format!("{} Trust: annotated {}", body, Utc::now().format("%Y-%m-%d"));
                    prefs.project.notes.push(dated);
                    let _ = crate::prefs::save(&prefs, path);
                    mark_promoted_nonfatal(store, session_key, "prefs_note", &body.chars().take(60).collect::<String>());
                    return Ok(true);
                }
            }
            // Fall back to adding as an annotation.
            let ann = crate::model::Annotation {
                id: None,
                topic: format!("prefs-note: {}", body.chars().take(60).collect::<String>()),
                body: body.clone(),
                tags: tags.clone(),
                added_at: Utc::now(),
                hash: None,
            };
            store.insert_annotation(&ann)?;
            Ok(true)
        }

        KnowledgeMarker::SkillCandidate { name, trigger, summary } => {
            // Upsert into skill_candidates (always staged, never directly committed).
            store.conn().execute(
                "INSERT INTO skill_candidates
                     (name, trigger_hint, tool_sequence, session_keys, occurrence_count,
                      first_seen_at, last_seen_at)
                 VALUES (?1, ?2, '[]', json_array(?3), 1, unixepoch(), unixepoch())
                 ON CONFLICT(name) DO UPDATE SET
                     trigger_hint     = CASE WHEN excluded.trigger_hint != '' THEN excluded.trigger_hint
                                        ELSE skill_candidates.trigger_hint END,
                     session_keys     = json_insert(skill_candidates.session_keys,
                                            '$[#]', excluded.session_keys->>'$[0]'),
                     occurrence_count = skill_candidates.occurrence_count + 1,
                     last_seen_at     = unixepoch()",
                params![name, trigger, session_key],
            )?;
            let _ = summary;  // stored via trigger_hint; full summary in tool sequence
            Ok(true)
        }
    }
}

/// Record that a knowledge marker was promoted to its target table.
///
/// NOTE: standard SQLite does not support `UPDATE ... LIMIT` (needs the
/// SQLITE_ENABLE_UPDATE_DELETE_LIMIT compile flag, which bundled rusqlite
/// lacks). The old LIMIT form failed to prepare on EVERY commit, and the
/// error propagated after the real insert had succeeded — so closeout
/// reported "0 committed" while the data was actually in the DB. Use a
/// rowid subquery instead (LIMIT inside a subselect is standard).
fn mark_promoted(store: &Store, session_key: &str, marker_type: &str, name: &str) -> Result<()> {
    store.conn().execute(
        "UPDATE knowledge_markers SET promoted = 1
         WHERE id = (
             SELECT id FROM knowledge_markers
             WHERE session_key = ?1 AND marker_type = ?2 AND (name = ?3 OR body LIKE ?4)
             AND promoted = 0
             ORDER BY id LIMIT 1
         )",
        params![session_key, marker_type, name, format!("%{}%", &name.chars().take(30).collect::<String>())],
    )?;
    Ok(())
}

/// mark_promoted is bookkeeping — a failure there must never mask a commit
/// that already happened. Log and continue.
fn mark_promoted_nonfatal(store: &Store, session_key: &str, marker_type: &str, name: &str) {
    if let Err(e) = mark_promoted(store, session_key, marker_type, name) {
        eprintln!("[closeout] warn: mark_promoted failed for {marker_type} '{name}': {e}");
    }
}

// ── Stage a marker for later review ──────────────────────────────────────────

fn stage_marker(store: &Store, session_key: &str, marker: &KnowledgeMarker) -> Result<()> {
    record_marker(store, session_key, marker, false)
}

/// Log a marker to `knowledge_markers` — the record of what this session
/// actually produced.
///
/// Every marker is logged, on BOTH closeout paths. `promoted` says whether it
/// also landed in its destination table (patterns / anti_patterns / adrs /
/// prefs) rather than waiting for review.
///
/// This exists because the inline-approve path used to commit markers straight
/// to their destination tables and never write here, so the scoreboard's
/// marker-capture metric — which reads this table — counted zero for every
/// session that was successfully closed with KNOWLEDGE COMMITTED. The metric
/// was measuring un-committed knowledge, which is backwards.
fn record_marker(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    promoted: bool,
) -> Result<()> {
    record_marker_raw(store, session_key, marker, promoted, "")
}

/// `record_marker`, keeping the marker's source text in `raw_tag`, so a marker
/// captured while the approval gate is on can be committed when it is approved.
pub(crate) fn record_marker_raw(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    promoted: bool,
    raw: &str,
) -> Result<()> {
    let body = marker_body(marker);
    let name = marker.display_name();
    let tags = marker_tags_json(marker);
    let trust = match marker {
        KnowledgeMarker::Pattern { trust, .. } => trust.clone(),
        _ => "annotated".to_string(),
    };

    store.conn().execute(
        "INSERT INTO knowledge_markers
             (session_key, marker_type, name, body, tags, trust_level, raw_tag, promoted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?7)",
        params![session_key, marker.marker_type(), name, body, tags, trust,
                if promoted { 1 } else { 0 }, raw],
    )?;
    Ok(())
}

// ── One marker, whichever way it arrived ─────────────────────────────────────

/// What happened to one marker.
#[derive(Debug)]
pub(crate) enum CommitOutcome {
    /// Newly stored at `target`; `merged` names any older duplicate it replaced.
    Committed { target: String, merged: Vec<String> },
    /// A pattern with this name existed with different text: the new version
    /// replaced it.
    Updated { target: String, replaced: String },
    /// Already in the store.
    Known,
    /// Refused by a gate, with the reason.
    Refused(String),
}

fn count_committed(result: &mut CloseoutResult, marker: &KnowledgeMarker) {
    match marker {
        KnowledgeMarker::Pattern { .. }        => result.patterns_committed += 1,
        KnowledgeMarker::AntiPattern { .. }    => result.anti_patterns_committed += 1,
        KnowledgeMarker::Correction { .. }     => result.corrections_committed += 1,
        KnowledgeMarker::Adr { .. }            => result.adrs_committed += 1,
        KnowledgeMarker::PrefsNote { .. }      => result.prefs_notes_committed += 1,
        KnowledgeMarker::SkillCandidate { .. } => result.skill_candidates_staged += 1,
        KnowledgeMarker::Wall(_)               => result.walls_committed += 1,
    }
}

/// Commit one marker through the gates and record what happened: the
/// knowledge_markers log (when `log`), a `loop_changes` row of `class` for
/// anything new, and the replacement rules below.
///
/// A marker that restates a stored entry replaces it -- same pattern name with
/// new text, or cosine >= 0.9 with a live entry of the same kind -- ONLY when
/// the marker was written after that entry. Catching up on a transcript meets
/// drafts that a later, corrected version had already replaced; found
/// 2026-09-30 when a first capture on a copy of the live store put a session's
/// 03:43 draft of a pattern over the version committed at 04:40. `written_at`
/// is when the marker was written (the transcript line's time); an entry
/// committed from it is dated then, so later comparisons stay truthful. An
/// undatable marker never replaces anything.
#[allow(clippy::too_many_arguments)]
pub(crate) fn commit_one(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    raw: &str,
    prefs_path: Option<&Path>,
    class: &str,
    evidence: &str,
    log: bool,
    written_at: Option<chrono::DateTime<Utc>>,
) -> CommitOutcome {
    let known = || {
        // Still logged, once per session: the session produced it, and the log
        // is how a re-derived lesson is told apart from one never written.
        if log && !logged_in_session(store, session_key, marker) {
            let _ = record_marker_raw(store, session_key, marker, false, raw);
        }
        CommitOutcome::Known
    };
    if already_committed(store, marker) {
        return known();
    }
    let newer_than = |stored: Option<chrono::DateTime<Utc>>| -> bool {
        matches!((written_at, stored), (Some(w), Some(s)) if w > s)
    };
    let at = written_at.unwrap_or_else(Utc::now);

    // Same pattern name, new text: a new version.
    if let KnowledgeMarker::Pattern { name, intent, body, .. } = marker {
        if let Some((old_id, old_intent, old_body, old_at)) = live_pattern_named(store, name) {
            let stored = old_body.rsplit_once("\nTrust: ").map(|(b, _)| b).unwrap_or(&old_body);
            // A version under half the length of the live one is a recap of
            // it (a closing summary), not a revision: replaying transcripts
            // met 1,234-character entries restated in 774.
            let recap = body.trim().len() * 2 < stored.trim().len();
            if (stored.trim() == body.trim() && old_intent.trim() == intent.trim()) || recap || !newer_than(old_at) {
                return known();
            }
            return match insert_replacing(store, session_key, marker, "patterns", old_id, at, class, evidence, "same name, new text") {
                Ok(target) => {
                    if log {
                        let _ = record_marker_raw(store, session_key, marker, true, raw);
                    }
                    CommitOutcome::Updated { target, replaced: format!("patterns:{old_id}") }
                }
                Err(e) => CommitOutcome::Refused(e.to_string()),
            };
        }
    }

    // An older wording of a live anti-pattern: same opening, stored later.
    // Catching up on old transcripts met entries reworded when they were
    // committed (four in one replay); the transcript's draft is not news.
    if let KnowledgeMarker::AntiPattern { description, .. } = marker {
        let opening: String = description.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().chars().take(40).collect();
        if opening.chars().count() == 40 {
            let same_opening: Option<i64> = store
                .conn()
                .query_row(
                    "SELECT id FROM anti_patterns WHERE superseded_by IS NULL AND lower(substr(trim(description), 1, 40)) = ?1
                     ORDER BY id DESC LIMIT 1",
                    params![opening],
                    |r| r.get(0),
                )
                .ok();
            if let Some(id) = same_opening {
                if !newer_than(stored_time(store, "anti_patterns", id)) {
                    return known();
                }
            }
        }
    }

    // A restatement of a live entry under different words.
    let restates = entry_text(marker).and_then(|(table, text)| {
        crate::knowledge_sim::nearest(store, table, &text, None)
            .ok()
            .flatten()
            .filter(|n| n.cosine >= crate::knowledge_sim::DUPLICATE_COSINE)
            .map(|n| (table, n))
    });
    if let Some((table, near)) = &restates {
        if !newer_than(stored_time(store, table, near.id)) {
            return known();
        }
    }

    match commit_marker_at(store, session_key, marker, prefs_path, at) {
        Ok(true) => {
            if log {
                let _ = record_marker_raw(store, session_key, marker, true, raw);
            }
            let target = entry_target(store, marker);
            let _ = crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
                class,
                target: &target,
                after: &marker.display_name(),
                evidence,
                session_id: session_key,
                ..Default::default()
            });
            let mut merged = Vec::new();
            if restates.is_none() {
                if let (Some((table, text)), Some(new_id)) = (
                    entry_text(marker),
                    target.split_once(':').and_then(|(_, id)| id.parse::<i64>().ok()),
                ) {
                    let _ = crate::reconcile::pair_new_entry(store, table, new_id, &text);
                }
            }
            if let Some((table, near)) = restates {
                if let Some(new_id) = target.split_once(':').and_then(|(_, id)| id.parse::<i64>().ok()) {
                    if store.supersede(table, near.id, new_id).is_ok() {
                        let old = format!("{table}:{}", near.id);
                        let _ = crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
                            class: "duplicate",
                            target: &old,
                            before: "live",
                            after: &format!("superseded by {new_id}"),
                            evidence: &format!("cosine {:.2} with {target}", near.cosine),
                            session_id: session_key,
                            ..Default::default()
                        });
                        merged.push(format!("{target} replaces {old} (cosine {:.2}: a restatement)", near.cosine));
                    }
                }
            }
            CommitOutcome::Committed { target, merged }
        }
        Ok(false) => known(),
        Err(e) => {
            if log {
                let _ = record_marker_raw(store, session_key, marker, false, raw);
            }
            CommitOutcome::Refused(e.to_string())
        }
    }
}

/// Committed before, by any path, in any session.
pub(crate) fn already_committed(store: &Store, marker: &KnowledgeMarker) -> bool {
    store
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_markers
                           WHERE marker_type = ?1 AND body = ?2 AND promoted = 1)",
            params![marker.marker_type(), marker_body(marker)],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
}

fn logged_in_session(store: &Store, session_key: &str, marker: &KnowledgeMarker) -> bool {
    store
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_markers
                           WHERE session_key = ?1 AND marker_type = ?2 AND body = ?3)",
            params![session_key, marker.marker_type(), marker_body(marker)],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
}

fn parse_time(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// When a stored entry was written (added_at / approved_at).
fn stored_time(store: &Store, table: &str, id: i64) -> Option<chrono::DateTime<Utc>> {
    let col = match table {
        "patterns" => "approved_at",
        "anti_patterns" => "added_at",
        _ => return None,
    };
    let raw: String = store
        .conn()
        .query_row(&format!("SELECT {col} FROM {table} WHERE id = ?1"), params![id], |r| r.get(0))
        .ok()?;
    parse_time(&raw)
}

fn live_pattern_named(store: &Store, name: &str) -> Option<(i64, String, String, Option<chrono::DateTime<Utc>>)> {
    store
        .conn()
        .query_row(
            "SELECT id, intent, body, approved_at FROM patterns WHERE name = ?1 AND superseded_by IS NULL
             ORDER BY id DESC LIMIT 1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)),
        )
        .ok()
        .map(|(id, intent, body, at)| (id, intent, body, parse_time(&at)))
}

/// Where a committed marker landed, as `table:id` (or a file for prefs notes).
pub(crate) fn entry_target(store: &Store, marker: &KnowledgeMarker) -> String {
    let id = |sql: &str, key: &str| -> Option<i64> {
        store.conn().query_row(sql, params![key], |r| r.get(0)).ok()
    };
    let found = match marker {
        KnowledgeMarker::Pattern { name, .. } => id(
            "SELECT id FROM patterns WHERE name = ?1 AND superseded_by IS NULL ORDER BY id DESC LIMIT 1",
            name,
        )
        .map(|i| format!("patterns:{i}")),
        KnowledgeMarker::AntiPattern { description, .. } => {
            id("SELECT id FROM anti_patterns WHERE description = ?1 ORDER BY id DESC LIMIT 1", description)
                .map(|i| format!("anti_patterns:{i}"))
        }
        KnowledgeMarker::Correction { attempted, .. } => {
            id("SELECT id FROM self_corrections WHERE attempted = ?1 ORDER BY id DESC LIMIT 1", attempted)
                .map(|i| format!("self_corrections:{i}"))
        }
        KnowledgeMarker::Adr { title, .. } => {
            id("SELECT id FROM adrs WHERE title = ?1 ORDER BY id DESC LIMIT 1", title).map(|i| format!("adrs:{i}"))
        }
        KnowledgeMarker::Wall(w) => crate::walls::find_by_claim(store, &w.claim)
            .ok()
            .flatten()
            .map(|id| format!("walls:{id}")),
        KnowledgeMarker::PrefsNote { .. } => Some("prefs.toml".to_string()),
        KnowledgeMarker::SkillCandidate { name, .. } => Some(format!("skill_candidates:{name}")),
    };
    found.unwrap_or_else(|| format!("{}:?", marker.marker_type()))
}

/// The text similarity is measured on, matching knowledge_sim::live_docs.
fn entry_text(marker: &KnowledgeMarker) -> Option<(&'static str, String)> {
    match marker {
        KnowledgeMarker::AntiPattern { description, wrong, correct, tags } => {
            Some(("anti_patterns", format!("{description} {wrong} {correct} {}", tags.join(" "))))
        }
        KnowledgeMarker::Pattern { name, intent, body, tags, .. } => {
            Some(("patterns", format!("{name} {intent} {body} {}", tags.join(" "))))
        }
        _ => None,
    }
}

/// Store a pattern marker as a new version and retire `old_id` in its favour.
#[allow(clippy::too_many_arguments)]
fn insert_replacing(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    table: &str,
    old_id: i64,
    at: chrono::DateTime<Utc>,
    class: &str,
    evidence: &str,
    why: &str,
) -> Result<String> {
    let mut p = pattern_from_marker(marker).context("not a pattern marker")?;
    p.approved_at = at;
    let new_id = store.insert_pattern(&p)?;
    store.supersede(table, old_id, new_id)?;
    if let KnowledgeMarker::Pattern { name, .. } = marker {
        mark_promoted_nonfatal(store, session_key, "pattern", name);
    }
    let target = format!("{table}:{new_id}");
    let replaced = format!("{table}:{old_id}");
    let _ = crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
        class,
        target: &target,
        after: &marker.display_name(),
        evidence,
        session_id: session_key,
        ..Default::default()
    });
    let _ = crate::loop_ledger::record(store, &crate::loop_ledger::NewChange {
        class: "duplicate",
        target: &replaced,
        before: "live",
        after: &format!("superseded by {new_id}"),
        evidence: why,
        session_id: session_key,
        ..Default::default()
    });
    Ok(target)
}

pub(crate) fn pattern_from_marker(marker: &KnowledgeMarker) -> Option<Pattern> {
    let KnowledgeMarker::Pattern { name, intent, body, trust, kind, uses, tags } = marker else { return None };
    Some(Pattern {
        id: None,
        name: name.clone(),
        intent: intent.clone(),
        body: format!("{body}\nTrust: {trust} {}", Utc::now().format("%Y-%m-%d")),
        uses: uses.clone(),
        tags: tags.clone(),
        approved_at: Utc::now(),
        use_count: 0,
        reverted_count: 0,
        survival_rate: 1.0,
        credibility: 0.0,
        trust_level: crate::model::TrustLevel::default(),
        kind: crate::model::MemoryKind::from_str(kind),
        tier: crate::model::EpistemicTier::default(),
        hash: None,
        included_in_context_count: 0,
        confirmed_count: 0,
        corrected_count: 0,
        superseded_by: None,
    })
}

/// Markers captured while the approval gate was on, waiting for a person.
fn staged_captures(store: &Store) -> Vec<(i64, KnowledgeMarker)> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT id, raw_tag FROM knowledge_markers
         WHERE promoted = 0 AND raw_tag != '' AND extracted_at >= unixepoch() - 86400
         ORDER BY id",
    ) else {
        return Vec::new();
    };
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(id, raw)| markers::parse_markers(&raw).into_iter().next().map(|m| (id, m)))
        .collect()
}

fn marker_body(marker: &KnowledgeMarker) -> String {
    match marker {
        KnowledgeMarker::Pattern { body, .. }        => body.clone(),
        KnowledgeMarker::AntiPattern { description, wrong, correct, .. } =>
            format!("{description}\nwrong: {wrong}\ncorrect: {correct}"),
        KnowledgeMarker::Correction { attempted, reason, fix, .. } =>
            format!("attempted: {attempted}\nreason: {reason}\nfix: {fix}"),
        KnowledgeMarker::Adr { context, decision, .. } =>
            format!("Context: {context}\nDecision: {decision}"),
        KnowledgeMarker::PrefsNote { body, .. }      => body.clone(),
        KnowledgeMarker::SkillCandidate { summary, .. } => summary.clone(),
        KnowledgeMarker::Wall(w)                     => crate::walls::marker_body(w),
    }
}

fn marker_tags_json(marker: &KnowledgeMarker) -> String {
    let tags: Vec<String> = match marker {
        KnowledgeMarker::Pattern { tags, .. }     => tags.clone(),
        KnowledgeMarker::AntiPattern { tags, .. } => tags.clone(),
        KnowledgeMarker::Correction { tags, .. }  => tags.clone(),
        KnowledgeMarker::Adr { tags, .. }         => tags.clone(),
        KnowledgeMarker::PrefsNote { tags, .. }   => tags.clone(),
        KnowledgeMarker::SkillCandidate { .. }    => vec![],
        KnowledgeMarker::Wall(w)                  => w.topic.clone(),
    };
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

// ── Host trace ingestion (Claude Code hook bridge) ────────────────────────────

/// Parse `.cortex/session-trace.jsonl` (appended by the Claude Code PostToolUse
/// hook), returning (tool names in first-seen order, domain tags from touched
/// paths). After ingestion the trace is archived to
/// `mined-tasks/trace_<session>.jsonl` so the next session starts clean.
fn ingest_session_trace(
    trace_path: &Path,
    mined_dir: &Path,
    session_key: &str,
    repo_root: &Path,
) -> (Vec<String>, Vec<String>) {
    let Ok(content) = std::fs::read_to_string(trace_path) else {
        return (vec![], vec![]);
    };

    let mut tools: Vec<String> = Vec::new();
    let mut tags:  Vec<String> = Vec::new();

    // Resolve the root once. The server is usually started with `--repo .`,
    // while hook paths are absolute (and on macOS canonical: /private/var, not
    // /var), so a literal prefix strip never matched and every tag became the
    // first component of an absolute path -- `Users`, `C:`, `private`.
    let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue; };

        if let Some(tool) = v.get("tool_name").and_then(|t| t.as_str()) {
            if !tool.is_empty() && !tools.iter().any(|t| t == tool) {
                tools.push(tool.to_string());
            }
        }

        // Derive a domain tag from the touched path: the first path component
        // under the repo root (crate / top-level dir name).
        let input = v.get("tool_input");
        let path_str = input
            .and_then(|i| i.get("file_path").or_else(|| i.get("path")).or_else(|| i.get("notebook_path")))
            .and_then(|p| p.as_str());
        if let Some(p) = path_str {
            let norm = p.replace('\\', "/");
            // Strip the ACTUAL repo root, which cortex is told at startup,
            // rather than guessing at directory names.
            //
            // This used to look for a literal "/RProjects/" segment and then
            // skip a component literally named "FlowMake" -- one developer's
            // folder layout and workspace name baked into the tagger. Anywhere
            // else the whole path became the tag, so the miner clustered on
            // noise and skill detection quietly degraded for every user but one.
            let touched = Path::new(&norm);
            let touched = if touched.is_relative() { root.join(touched) } else { touched.to_path_buf() };
            // Canonicalise when the file still exists; a deleted file keeps its
            // spelling, which is checked against both forms of the root.
            let touched = std::fs::canonicalize(&touched).unwrap_or(touched);
            // A path outside the repository -- a scratch file, a temp dir -- says
            // nothing about which part of the project the session worked on.
            let tag = touched
                .strip_prefix(&root)
                .or_else(|_| touched.strip_prefix(repo_root))
                .ok()
                .and_then(|rel| rel.components().next())
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .unwrap_or_default();
            if !tag.is_empty() && !tag.contains('.') && !tags.iter().any(|t| t == &tag) && tags.len() < 10 {
                tags.push(tag);
            }
        }
    }
    tools.truncate(30);

    // Archive the raw trace next to the session snapshot, then remove the live file.
    let archived = mined_dir.join(format!("trace_{}.jsonl", session_key.replace('/', "_")));
    if std::fs::rename(trace_path, &archived).is_err() {
        // Rename across a lock or missing dir — fall back to truncation.
        let _ = std::fs::write(trace_path, "");
    }

    (tools, tags)
}

// ── Session snapshot ──────────────────────────────────────────────────────────

/// The cortex tools a session called, first use first.
///
/// Keyed by session. Calls logged before they were stamped with one have no
/// key, so those fall back to a recent window -- compared in the stored
/// format. The old filter used `datetime('now', '-3 hours')`, which renders as
/// `2026-09-17 03:…` against stored `2026-09-17T06:…`; 'T' sorts after ' ', so
/// it matched the whole day, and every session that day shared one trajectory.
fn session_tools(store: &Store, session_key: &str) -> Vec<String> {
    fn first_column(row: &rusqlite::Row) -> rusqlite::Result<String> {
        row.get(0)
    }
    let collect = |rows: rusqlite::Result<rusqlite::MappedRows<'_, fn(&rusqlite::Row) -> rusqlite::Result<String>>>| {
        rows.map(|r| r.filter_map(|x| x.ok()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    let own = match store.conn().prepare(
        "SELECT tool FROM mcp_calls WHERE logical_session_key = ?1
         GROUP BY tool ORDER BY MIN(id) LIMIT 30",
    ) {
        Ok(mut stmt) => collect(stmt.query_map(params![session_key], first_column as fn(&rusqlite::Row) -> _)),
        Err(_) => vec![],
    };
    if !own.is_empty() {
        return own;
    }
    match store.conn().prepare(
        "SELECT tool FROM mcp_calls
         WHERE logical_session_key IS NULL
           AND called_at >= strftime('%Y-%m-%dT%H:%M:%S', 'now', '-3 hours')
         GROUP BY tool ORDER BY MIN(id) LIMIT 30",
    ) {
        Ok(mut stmt) => collect(stmt.query_map([], first_column as fn(&rusqlite::Row) -> _)),
        Err(_) => vec![],
    }
}

fn write_session_snapshot(
    store: &Store,
    session_key: &str,
    outcome_type: &str,
    markers: &[KnowledgeMarker],
    repo_root: &Path,
) -> Result<String> {
    let dir = repo_root.join(".cortex").join("mined-tasks");
    std::fs::create_dir_all(&dir).context("create mined-tasks dir")?;

    let marker_counts = json!({
        "pattern":        markers.iter().filter(|m| matches!(m, KnowledgeMarker::Pattern { .. })).count(),
        "anti_pattern":   markers.iter().filter(|m| matches!(m, KnowledgeMarker::AntiPattern { .. })).count(),
        "correction":     markers.iter().filter(|m| matches!(m, KnowledgeMarker::Correction { .. })).count(),
        "adr":            markers.iter().filter(|m| matches!(m, KnowledgeMarker::Adr { .. })).count(),
        "prefs_note":     markers.iter().filter(|m| matches!(m, KnowledgeMarker::PrefsNote { .. })).count(),
        "skill_candidate":markers.iter().filter(|m| matches!(m, KnowledgeMarker::SkillCandidate { .. })).count(),
        "wall":           markers.iter().filter(|m| matches!(m, KnowledgeMarker::Wall(_))).count(),
    });

    // Read recent tool sequences from mcp_calls for this session.
    let mut tool_seq: Vec<String> = session_tools(store, session_key);

    // Merge in host-side trace events (Claude Code PostToolUse hook writes
    // .cortex/session-trace.jsonl). This is what makes trajectories on Claude
    // Code as rich as the VS Code session store makes them for Copilot:
    // real work tools (Edit/Bash/Read...) + touched crates as domain tags.
    let trace_path = repo_root.join(".cortex").join("session-trace.jsonl");
    let (trace_tools, domain_tags) = ingest_session_trace(&trace_path, &dir, session_key, repo_root);
    for t in trace_tools {
        if !tool_seq.contains(&t) {
            tool_seq.push(t);
        }
    }
    tool_seq.truncate(60);

    let snapshot = json!({
        "session_key":   session_key,
        "outcome_type":  outcome_type,
        "marker_counts": marker_counts,
        "tool_sequence": tool_seq,
        "domain_tags":   domain_tags,
        "created_at":    Utc::now().to_rfc3339(),
    });

    let filename = format!("session_{}.json", session_key.replace('/', "_"));
    let path = dir.join(&filename);
    std::fs::write(&path, serde_json::to_string_pretty(&snapshot)?)
        .context("write session snapshot")?;

    let path_str = path.to_string_lossy().to_string();

    // Record in session_snapshots table.
    let _ = store.conn().execute(
        "INSERT OR REPLACE INTO session_snapshots
             (session_key, outcome_type, tool_sequence, marker_counts, snapshot_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
        params![
            session_key,
            outcome_type,
            serde_json::to_string(&tool_seq).unwrap_or_else(|_| "[]".to_string()),
            marker_counts.to_string(),
            path_str.clone(),
        ],
    );

    Ok(path_str)
}

// ── Mirror file content ───────────────────────────────────────────────────────

fn build_mirror_content(
    session_key: &str,
    outcome_type: &str,
    markers: &[KnowledgeMarker],
    inline_approved: bool,
) -> Result<String> {
    let mut out = format!("# Session Closeout — {}\n\n", Utc::now().format("%Y-%m-%d"));
    out.push_str(&format!("**Session:** {session_key}  \n"));
    out.push_str(&format!("**Outcome:** {outcome_type}  \n"));
    out.push_str(&format!("**Knowledge committed:** {}  \n\n",
        if inline_approved { "✓ KNOWLEDGE COMMITTED" } else { "staged only" }));

    if !markers.is_empty() {
        out.push_str("## Knowledge captured\n\n");
        for m in markers {
            out.push_str(&format!("- [{}] {}\n", m.marker_type(), m.display_name()));
        }
    }

    Ok(out)
}

/// Fallback marker extraction: scan recent mcp_calls arguments for CORTEX-* tags.
/// Used when the VS Code session store is inaccessible.
fn extract_markers_from_mcp_calls(store: &Store) -> Result<Vec<KnowledgeMarker>> {
    let mut stmt = store.conn().prepare(
        "SELECT args FROM mcp_calls
         WHERE called_at > datetime('now', '-1 day')
         ORDER BY id DESC LIMIT 30"
    )?;
    let args_list: Vec<String> = stmt.query_map([], |r| {
        r.get::<_, String>(0)
    })?.collect::<rusqlite::Result<Vec<_>>>()?;

    let all_text = args_list.join("\n\n---\n\n");
    Ok(markers::parse_markers(&all_text))
}

// ── Graph snapshot pruning ────────────────────────────────────────────────────

/// Drift analysis and `cortex graph-diff` only ever read the NEWEST snapshot
/// (`graph_diff::find_previous_snapshot`); nothing else reads this directory.
/// At ~34MB each, a cap of 50 held 1.1GB for one file's worth of use. A few are
/// kept so a manual diff against a slightly older baseline stays possible.
const MAX_SNAPSHOTS: usize = 5;

fn prune_old_snapshots(dir: &Path, max_age_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_days * 86400))
        .unwrap_or(std::time::UNIX_EPOCH);

    let mut snapshots: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => return,
    };
    // Names carry the timestamp, so newest first.
    snapshots.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    for (i, path) in snapshots.iter().enumerate() {
        // The newest is the drift baseline and survives whatever its age.
        // Age comes from mtime, and std::fs::copy carries graph.json's mtime over
        // to the snapshot -- so without this, a fresh snapshot of a graph built
        // long ago was deleted the moment it was written.
        let too_old = i > 0
            && std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
        if i >= MAX_SNAPSHOTS || too_old {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> crate::test_support::TempStore {
        crate::test_support::TempStore::new(name).unwrap()
    }

    #[test]
    fn a_wall_marker_commits_through_the_ledger_rules_and_a_bad_one_says_why() {
        let store = test_store("walls_marker");
        let _g = crate::test_support::TempDir::new("closeout_walls").unwrap();
        let markers_text = r#"
[CORTEX-WALL: claim="Hardware ray queries are unavailable on Quest 3" provenance="platform" topic="quest,raytracing" untested="driver exposure" cheapest_test="log VK_KHR_ray_query at startup (~15 min)"]
paper: Mesa Turnip exposes accelerated ray queries on a740+ @ phoronix @ 2025
[/CORTEX-WALL]
[CORTEX-WALL: claim="Splat relighting is not a Quest 3 technique" provenance="because I said so"]
inferred: desktop papers are slow
[/CORTEX-WALL]
"#;
        let result = run_closeout(
            &store, "s-walls", "build_pass", None, None, true, _g.path(), None, Some(markers_text),
        )
        .unwrap();
        assert_eq!(result.walls_committed, 1);
        let wall = crate::walls::all(&store).unwrap().pop().unwrap();
        assert_eq!((wall.provenance.as_str(), wall.status.as_str()), ("platform", "open"));
        assert_eq!(wall.evidence[0].date, "2025");
        assert!(
            result.notes.iter().any(|n| n.contains("wall marker") && n.contains("whose limit")),
            "the refused marker is reported with its reason: {:?}",
            result.notes
        );
    }

    /// Regression: closeout must COUNT what it commits. The old mark_promoted
    /// used `UPDATE ... LIMIT` (unsupported in bundled SQLite), which errored
    /// after each successful insert — data landed but every counter read 0,
    /// so "KNOWLEDGE COMMITTED" reported nothing was saved.
    #[test]
    fn closeout_markers_text_commits_and_counts() {
        let store = test_store("counts");
        let _g = crate::test_support::TempDir::new("closeout_repo").unwrap();
        let repo_root = _g.path().to_path_buf();
        let _ = std::fs::create_dir_all(&repo_root);

        let markers_text = r#"
[CORTEX-PATTERN: name="test-pattern-count" intent="verify counting" trust="verified"]body here[/CORTEX-PATTERN]
[CORTEX-AP: description="test anti-pattern count" tags="test"]wrong: x
correct: y[/CORTEX-AP]
[CORTEX-CORRECTION: attempted="counted wrong" reason="LIMIT clause" fix="subquery"][/CORTEX-CORRECTION]
"#;

        let result = run_closeout(
            &store, "session-test", "build_pass", None, None,
            true, &repo_root, None, Some(markers_text),
        ).unwrap();

        assert_eq!(result.patterns_committed, 1, "pattern commit must be counted");
        assert_eq!(result.anti_patterns_committed, 1, "anti-pattern commit must be counted");
        assert_eq!(result.corrections_committed, 1, "correction commit must be counted");

        // The data must actually be in the DB, matching the counts.
        let n: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM patterns WHERE name = 'test-pattern-count'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);

        // Second closeout with the same markers: duplicate pattern/AP are
        // skipped (counted 0), never double-inserted.
        let again = run_closeout(
            &store, "session-test-2", "build_pass", None, None,
            true, &repo_root, None, Some(markers_text),
        ).unwrap();
        assert_eq!(again.patterns_committed, 0, "duplicate pattern must not recount");
        assert_eq!(again.anti_patterns_committed, 0, "duplicate AP must not recount");
        let n2: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM patterns WHERE name = 'test-pattern-count'", [], |r| r.get(0)).unwrap();
        assert_eq!(n2, 1, "no duplicate rows");
    }

    /// A session closed with KNOWLEDGE COMMITTED must show up in the capture
    /// metric.
    ///
    /// The inline-approve path used to commit markers straight to patterns /
    /// anti_patterns / adrs and never touch `knowledge_markers`, which is the
    /// table the scoreboard counts. Every successful closeout therefore scored
    /// zero markers, so the metric reported the opposite of what it claimed:
    /// the only sessions it credited were the ones left un-approved.
    #[test]
    fn committing_knowledge_is_not_recorded_as_capturing_none() {
        let store = test_store("inline_capture");
        let repo_root = std::env::temp_dir().join("cx_inline_capture");
        let _ = std::fs::create_dir_all(&repo_root);
        let markers_text = concat!(
            "[CORTEX-PATTERN: name=\"inline-p\" intent=\"i\" trust=\"verified\" uses=\"\"]b[/CORTEX-PATTERN]\n",
            "[CORTEX-AP: description=\"inline-ap\" tags=\"t\"]wrong: x\ncorrect: y[/CORTEX-AP]",
        );

        let result = run_closeout(
            &store, "s-inline", "build_pass", None, None,
            true, &repo_root, None, Some(markers_text),
        ).unwrap();
        assert_eq!(result.patterns_committed, 1);
        assert_eq!(result.anti_patterns_committed, 1);

        // This is the assertion that would have failed before the fix.
        let logged: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers WHERE session_key = 's-inline'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(logged, 2,
            "markers committed inline must still be logged for the capture metric");

        // And they must be marked as having landed, not merely staged.
        let promoted: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers \
             WHERE session_key = 's-inline' AND promoted = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(promoted, 2, "committed markers must be flagged promoted");

        let _ = std::fs::remove_dir_all(&repo_root);
    }

    /// A re-run whose markers are all duplicates still produced work, but
    /// nothing new landed — the log must be able to tell those apart.
    #[test]
    fn a_duplicate_marker_is_logged_but_not_flagged_as_landed() {
        let store = test_store("dup_capture");
        let repo_root = std::env::temp_dir().join("cx_dup_capture");
        let _ = std::fs::create_dir_all(&repo_root);
        let markers_text =
            "[CORTEX-PATTERN: name=\"dup-p\" intent=\"i\" trust=\"verified\" uses=\"\"]b[/CORTEX-PATTERN]";

        for key in ["s-dup-1", "s-dup-2"] {
            run_closeout(&store, key, "build_pass", None, None,
                         true, &repo_root, None, Some(markers_text)).unwrap();
        }

        let second_logged: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers WHERE session_key = 's-dup-2'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(second_logged, 1, "the second session still emitted a marker");

        let second_promoted: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers \
             WHERE session_key = 's-dup-2' AND promoted = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(second_promoted, 0, "a duplicate did not land, so it is not promoted");

        let _ = std::fs::remove_dir_all(&repo_root);
    }

    /// mark_promoted must be valid SQL on stock SQLite (no UPDATE ... LIMIT).
    #[test]
    fn mark_promoted_sql_is_valid() {
        let store = test_store("promote");
        stage_marker(&store, "s1", &KnowledgeMarker::Pattern {
            name: "p1".into(), intent: "i".into(), body: "b".into(),
            trust: "verified".into(), kind: String::new(), uses: vec![], tags: vec![],
        }).unwrap();
        // Must not error — the old LIMIT form failed at prepare time.
        mark_promoted(&store, "s1", "pattern", "p1").unwrap();
        let promoted: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers WHERE promoted = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(promoted, 1);
    }

    /// The server runs with `--repo .` while hook paths are absolute and, on
    /// macOS, canonical, so the literal prefix strip never matched and tags came
    /// out as `Users`, `C:` or `private`.
    #[test]
    fn domain_tags_come_from_the_repo_relative_path_however_the_root_is_spelled() {
        let repo = crate::test_support::TempDir::new("tagger").unwrap();
        std::fs::create_dir_all(repo.join("engine").join("src")).unwrap();
        std::fs::create_dir_all(repo.join(".cortex").join("mined")).unwrap();
        let real = std::fs::canonicalize(repo.path()).unwrap();
        let trace = repo.join(".cortex").join("session-trace.jsonl");
        let lines = [
            json!({"tool_name": "Edit", "tool_input": {"file_path": real.join("engine/src/lib.rs").to_string_lossy()}}),
            json!({"tool_name": "Write", "tool_input": {"file_path": "/tmp/cortex-elsewhere/notes.md"}}),
        ];
        std::fs::write(&trace, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();

        // A non-canonical spelling of the root, as `--repo .` or a symlinked temp dir gives.
        let spelled = repo.path().join("engine").join("..");
        let (tools, tags) = ingest_session_trace(&trace, &repo.join(".cortex").join("mined"), "s", &spelled);
        assert_eq!(tools, vec!["Edit", "Write"]);
        assert_eq!(tags, vec!["engine"], "a path outside the repo must not become a tag either");
    }

    /// A snapshot took every tool any session called that day, mixing
    /// concurrent sessions into one trajectory.
    #[test]
    fn a_session_snapshot_records_only_that_sessions_tools() {
        let store = test_store("snapshot_tools");
        let repo = crate::test_support::TempDir::new("snapshot_repo").unwrap();
        // Through the logging chokepoint, which must stamp the session.
        for (tool, key) in [("get_item", "mine"), ("query_graph", "theirs"), ("recall", "mine"), ("get_item", "mine")] {
            store.log_mcp_call(tool, "{}", Some(key)).unwrap();
        }
        let path = write_session_snapshot(&store, "mine", "build_pass", &[], repo.path()).unwrap();
        let snap: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(snap["tool_sequence"], json!(["get_item", "recall"]));
    }

    /// Calls logged before they carried a session fall back to a recent window,
    /// compared in the stored timestamp format rather than matching all day.
    #[test]
    fn unkeyed_calls_fall_back_to_the_last_three_hours_not_the_whole_day() {
        let store = test_store("snapshot_fallback");
        let repo = crate::test_support::TempDir::new("snapshot_fallback_repo").unwrap();
        let recent = (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        let hours_ago = (Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
        for (tool, at) in [("query_graph", hours_ago.as_str()), ("recall", recent.as_str())] {
            store.conn().execute(
                "INSERT INTO mcp_calls (tool, args, called_at) VALUES (?1, '{}', ?2)",
                params![tool, at],
            ).unwrap();
        }
        let path = write_session_snapshot(&store, "unstamped", "build_pass", &[], repo.path()).unwrap();
        let snap: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(snap["tool_sequence"], json!(["recall"]));
    }

    #[test]
    fn snapshot_pruning_keeps_only_the_newest_few() {
        let dir = crate::test_support::TempDir::new("snap_prune").unwrap();
        for day in 1..=9 {
            std::fs::write(dir.join(&format!("graph_2026090{day}_000000.json")), "{}").unwrap();
        }
        prune_old_snapshots(dir.path(), 30);

        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left.len(), MAX_SNAPSHOTS);
        assert_eq!(left.last().unwrap(), "graph_20260909_000000.json", "the newest must survive");
    }

    /// A snapshot inherits graph.json's mtime through the copy, so a graph built
    /// long ago yields a brand-new snapshot that already looks old.
    #[test]
    fn the_newest_snapshot_survives_age_pruning_whatever_its_mtime() {
        let dir = crate::test_support::TempDir::new("snap_age").unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86400);
        for day in 1..=3 {
            let p = dir.join(&format!("graph_2026090{day}_000000.json"));
            std::fs::write(&p, "{}").unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(long_ago).unwrap();
        }
        prune_old_snapshots(dir.path(), 30);

        let left: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec!["graph_20260903_000000.json".to_string()],
            "old snapshots go, but the drift baseline must not");
    }
}


/// Is `graph.json` older than the code it claims to describe?
///
/// Returns a human reason when stale, `None` when it is current. Compares
/// against the newest source file rather than a fixed age: a graph built an hour
/// ago is stale if the code changed since, and one built a month ago is fine if
/// nothing has.
pub(crate) fn graph_is_stale(repo_root: &Path, graph: &Path) -> Option<String> {
    let graph_time = std::fs::metadata(graph).and_then(|m| m.modified()).ok()?;
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    let mut stack = vec![repo_root.to_path_buf()];
    let mut looked = 0usize;
    while let Some(dir) = stack.pop() {
        // Bounded: this runs on every closeout and must not walk a whole disk.
        if looked > 20_000 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "target" | "node_modules" | "venv" | "site-packages" | "__pycache__"
                        | "dist" | "build" | "vendor"
                )
            {
                continue;
            }
            if path.is_dir() {
                // A directory's own mtime moves when an entry is added, removed
                // or renamed - the changes a newest-FILE scan cannot see, since
                // `mv` keeps a file's mtime and a deleted file has none.
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    if newest.as_ref().map(|(nt, _)| t > *nt).unwrap_or(true) {
                        newest = Some((t, path.display().to_string()));
                    }
                }
                stack.push(path);
                continue;
            }
            let is_source = path
                .extension()
                .map(|x| matches!(x.to_string_lossy().as_ref(), "rs" | "slint" | "toml" | "py" | "ts" | "tsx"))
                .unwrap_or(false);
            if !is_source {
                continue;
            }
            looked += 1;
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().map(|(nt, _)| t > *nt).unwrap_or(true) {
                    newest = Some((t, path.display().to_string()));
                }
            }
        }
    }
    let (newest_time, newest_path) = newest?;
    if newest_time > graph_time {
        let age = newest_time
            .duration_since(graph_time)
            .map(|d| d.as_secs() / 3600)
            .unwrap_or(0);
        Some(format!(
            "graph.json is {age}h behind the newest source ({})",
            Path::new(&newest_path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or(newest_path)
        ))
    } else {
        None
    }
}

/// Rebuild the graph in place. Fails if graphify-rs is not on PATH, which is a
/// normal state on a machine that does not have it — hence a skipped snapshot
/// rather than a failed closeout.
pub(crate) fn rebuild_graph(repo_root: &Path) -> Result<()> {
    // --output is REQUIRED. Without it graphify-rs writes to its own per-project
    // cache under ~/.graphify-rs/<project>-<hash>/, not to the repo, so the
    // rebuild "succeeds" and .graphify-output/graph.json stays exactly as stale
    // as it was — the staleness check would then fire on every single closeout
    // and never clear. Verified: a rebuild without it left a 15-day-old file in
    // place and reported exit 0.
    let out = std::process::Command::new("graphify-rs")
        .args([
            "build",
            "--path", ".",
            "--code-only",
            // Only graph.json is read, by cortex and by the graphify MCP server.
            // Without --format every rebuild also wrote html, graphml, cypher,
            // svg, wiki and obsidian output: ~340MB and ~50,000 files, unused.
            "--format", "json",
            // Local only, matching the rebuild command in the operating manual.
            "--no-llm",
            "--update",
            "--output", ".graphify-output",
        ])
        .current_dir(repo_root)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "graphify-rs exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")
        );
    }
    Ok(())
}

/// First line only — provider and tool errors carry whole stack traces.
pub(crate) fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(160).collect()
}
