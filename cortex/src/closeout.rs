/// Phase 0D: Session closeout logic.
///
/// closeout_session is the single MCP tool that replaces the 7-step manual checklist.
/// With inline_approve=true (triggered by "KNOWLEDGE COMMITTED"), all markers are
/// immediately committed to the DB. With inline_approve=false (default), markers
/// are staged in knowledge_markers for later review.
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
    pub adrs_committed:          usize,
    pub prefs_notes_committed:   usize,
    pub skill_candidates_staged: usize,
    pub markers_staged:          usize,
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

    if inline_approve {
        // Tier 1: commit immediately.
        for marker in &markers {
            match commit_marker(store, session_key, marker, prefs_path) {
                Ok(committed) => {
                    // Log it either way: a marker that turned out to be a
                    // duplicate was still produced by this session, and the
                    // capture metric is about what the agent emitted.
                    // `promoted` is what distinguishes new from already-known.
                    let _ = record_marker(store, session_key, marker, committed);
                    if committed {
                        match marker {
                            KnowledgeMarker::Pattern { .. }        => result.patterns_committed += 1,
                            KnowledgeMarker::AntiPattern { .. }    => result.anti_patterns_committed += 1,
                            KnowledgeMarker::Correction { .. }     => result.corrections_committed += 1,
                            KnowledgeMarker::Adr { .. }            => result.adrs_committed += 1,
                            KnowledgeMarker::PrefsNote { .. }      => result.prefs_notes_committed += 1,
                            KnowledgeMarker::SkillCandidate { .. } => result.skill_candidates_staged += 1,
                        }
                    }
                }
                Err(e) => {
                    // Emitted but did not land — log it unpromoted so the
                    // session is not silently credited with zero output.
                    let _ = record_marker(store, session_key, marker, false);
                    eprintln!("[closeout] warn: failed to commit marker: {e}");
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
                         Run: graphify-rs build --path . --code-only --update --output .graphify-output",
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
                // Prune snapshots older than 30 days.
                prune_old_snapshots(&snapshots_dir, 30);
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
fn commit_marker(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    prefs_path: Option<&Path>,
) -> Result<bool> {
    match marker {
        KnowledgeMarker::Pattern { name, intent, body, trust, uses, tags } => {
            // Check for duplicate name.
            let exists: bool = store.conn().query_row(
                "SELECT COUNT(*) > 0 FROM patterns WHERE name = ?1",
                params![name], |r| r.get::<_, bool>(0),
            ).unwrap_or(false);
            if exists { return Ok(false); }

            let body_with_trust = format!("{body}\nTrust: {trust} {}", Utc::now().format("%Y-%m-%d"));
            let p = Pattern {
                id: None,
                name: name.clone(),
                intent: intent.clone(),
                body: body_with_trust,
                uses: uses.clone(),
                tags: tags.clone(),
                approved_at: Utc::now(),
                use_count: 0,
                reverted_count: 0,
                survival_rate: 1.0,
            };
            store.insert_pattern(&p)?;
            mark_promoted_nonfatal(store, session_key, "pattern", name);
            Ok(true)
        }

        KnowledgeMarker::AntiPattern { description, wrong, correct, tags } => {
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
                added_at: Utc::now(),
            };
            store.insert_anti_pattern(&ap)?;
            mark_promoted_nonfatal(store, session_key, "anti_pattern", description);
            Ok(true)
        }

        KnowledgeMarker::Correction { attempted, reason, fix, tags } => {
            store.insert_self_correction(attempted, reason, fix, tags)?;
            mark_promoted_nonfatal(store, session_key, "correction", attempted);
            Ok(true)
        }

        KnowledgeMarker::Adr { title, context, decision, tags } => {
            use crate::model::Adr;
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7)",
        params![session_key, marker.marker_type(), name, body, tags, trust,
                if promoted { 1 } else { 0 }],
    )?;
    Ok(())
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
            let root_norm = repo_root.to_string_lossy().replace('\\', "/");
            let tag = norm
                .strip_prefix(root_norm.trim_end_matches('/'))
                .unwrap_or(&norm)
                .trim_start_matches('/')
                .split('/')
                .find(|c| !c.is_empty())
                .unwrap_or("")
                .to_string();
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
    });

    // Read recent tool sequences from mcp_calls for this session.
    let mut tool_seq: Vec<String> = {
        if let Ok(mut stmt) = store.conn().prepare(
            "SELECT DISTINCT tool FROM mcp_calls
             WHERE called_at >= datetime('now', '-3 hours')
             ORDER BY id ASC LIMIT 30"
        ) {
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default()
        } else {
            vec![]
        }
    };

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

const MAX_SNAPSHOTS: usize = 50;

fn prune_old_snapshots(dir: &Path, max_age_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_days * 86400))
        .unwrap_or(std::time::UNIX_EPOCH);

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if mtime < cutoff {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    // Count-based pruning: keep only the N newest snapshots.
    let mut snapshots: Vec<_> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            snapshots.push(entry.path());
        }
    }
    snapshots.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    while snapshots.len() > MAX_SNAPSHOTS {
        if let Some(old) = snapshots.pop() {
            let _ = std::fs::remove_file(&old);
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
            trust: "verified".into(), uses: vec![], tags: vec![],
        }).unwrap();
        // Must not error — the old LIMIT form failed at prepare time.
        mark_promoted(&store, "s1", "pattern", "p1").unwrap();
        let promoted: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM knowledge_markers WHERE promoted = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(promoted, 1);
    }
}


/// Is `graph.json` older than the code it claims to describe?
///
/// Returns a human reason when stale, `None` when it is current. Compares
/// against the newest source file rather than a fixed age: a graph built an hour
/// ago is stale if the code changed since, and one built a month ago is fine if
/// nothing has.
fn graph_is_stale(repo_root: &Path, graph: &Path) -> Option<String> {
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
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
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
fn rebuild_graph(repo_root: &Path) -> Result<()> {
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
