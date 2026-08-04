/// Protocol state machine — Phase 0B.
///
/// Tracks which protocol phases a session has completed. Persists to the
/// `protocol_sessions` DB table so state survives MCP server restarts.
///
/// Session key derivation:
///   session_key = sha256(unix_minute of earliest call in the current
///                        2-hour inactivity window).
///
/// Two hours of MCP inactivity = new session. This groups calls from one
/// "work period" across all chat tabs into a single logical session.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

// ── Session key derivation ────────────────────────────────────────────────────

/// Derive the current logical session key from the most recent mcp_calls row
/// and the repo root (to avoid cross-project collisions).
/// Returns a new session key if no calls exist or the last call is > 2 hours ago.
pub fn current_session_key(conn: &Connection, repo_root: Option<&str>) -> Result<String> {
    // Find the timestamp of the most recent mcp_call in the current window.
    let two_hours_ago = Utc::now().timestamp() - 7200;

    let earliest_in_window: Option<i64> = conn.query_row(
        "SELECT MIN(unixepoch(called_at)) FROM mcp_calls
         WHERE unixepoch(called_at) >= ?1",
        params![two_hours_ago],
        |r| r.get(0),
    ).optional()?.flatten();

    let anchor = match earliest_in_window {
        Some(ts) => {
            // Round down to the minute for stability across calls in the same minute.
            ts - (ts % 60)
        }
        None => {
            // No calls in the last 2 hours — start a fresh session anchored to now.
            let now = Utc::now().timestamp();
            now - (now % 60)
        }
    };

    Ok(make_session_key(anchor, repo_root))
}

/// Generate a stable, human-readable session key from a unix-minute timestamp
/// and an optional repo root discriminator (prevents cross-project collision).
fn make_session_key(unix_minute: i64, repo_root: Option<&str>) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    write!(s, "session_{:016x}", unix_minute).unwrap();
    if let Some(root) = repo_root {
        // FNV-1a 64-bit hash of the repo root path — full 64-bit space, not truncated.
        // Collisions still possible for different repos, but astronomically unlikely.
        let root_hash = root.as_bytes().iter()
            .fold(14695981039346656037u64, |h, &b| {
                h.wrapping_mul(1099511628211u64) ^ b as u64
            });
        write!(s, "_{:016x}", root_hash).unwrap();
    }
    s
}

// ── DB read/write ─────────────────────────────────────────────────────────────

/// Load or create a protocol session record for the given key.
pub fn load_or_create(conn: &Connection, session_key: &str) -> Result<ProtocolSession> {
    let existing: Option<ProtocolSession> = conn.query_row(
        "SELECT session_key, protocol_mode, delta_retrieved, preferences_loaded,
                anti_patterns_loaded, context_loaded, bootstrap_complete,
                closeout_run, outcome_type, closed_at, knowledge_markers_flushed,
                inline_approved, graph_snapshot_written
         FROM protocol_sessions WHERE session_key = ?1",
        params![session_key],
        row_to_session,
    ).optional()?;

    if let Some(s) = existing {
        return Ok(s);
    }

    // Create a new row.
    let now = Utc::now().timestamp();
    conn.execute(
        "INSERT OR IGNORE INTO protocol_sessions (session_key, started_at) VALUES (?1, ?2)",
        params![session_key, now],
    ).context("failed to insert protocol_sessions row")?;

    Ok(ProtocolSession {
        session_key: session_key.to_string(),
        ..Default::default()
    })
}

/// Save (upsert) a protocol session record.
pub fn save(conn: &Connection, s: &ProtocolSession) -> Result<()> {
    conn.execute(
        "INSERT INTO protocol_sessions
         (session_key, protocol_mode, delta_retrieved, preferences_loaded,
          anti_patterns_loaded, context_loaded, bootstrap_complete,
          closeout_run, outcome_type, closed_at, knowledge_markers_flushed,
          inline_approved, graph_snapshot_written)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(session_key) DO UPDATE SET
           protocol_mode              = excluded.protocol_mode,
           delta_retrieved            = excluded.delta_retrieved,
           preferences_loaded         = excluded.preferences_loaded,
           anti_patterns_loaded       = excluded.anti_patterns_loaded,
           context_loaded             = excluded.context_loaded,
           bootstrap_complete         = excluded.bootstrap_complete,
           closeout_run               = excluded.closeout_run,
           outcome_type               = excluded.outcome_type,
           closed_at                  = excluded.closed_at,
           knowledge_markers_flushed  = excluded.knowledge_markers_flushed,
           inline_approved            = excluded.inline_approved,
           graph_snapshot_written     = excluded.graph_snapshot_written",
        params![
            s.session_key,
            s.protocol_mode as i64,
            s.delta_retrieved as i64,
            s.preferences_loaded as i64,
            s.anti_patterns_loaded as i64,
            s.context_loaded as i64,
            s.bootstrap_complete as i64,
            s.closeout_run as i64,
            s.outcome_type.as_deref(),
            s.closed_at,
            s.knowledge_markers_flushed as i64,
            s.inline_approved as i64,
            s.graph_snapshot_written as i64,
        ],
    ).context("failed to upsert protocol_sessions")?;
    Ok(())
}

/// Record one bootstrap step as completed and recompute bootstrap_complete.
pub fn record_step(conn: &Connection, session_key: &str, step: ProtocolStep) -> Result<()> {
    let mut s = load_or_create(conn, session_key)?;

    match step {
        ProtocolStep::GetDelta          => s.delta_retrieved = true,
        ProtocolStep::GetPreferences    => s.preferences_loaded = true,
        ProtocolStep::GetAntiPatterns   => s.anti_patterns_loaded = true,
        ProtocolStep::GetContext        => s.context_loaded = true,
    }

    s.bootstrap_complete = s.delta_retrieved
        && s.preferences_loaded
        && s.anti_patterns_loaded
        && s.context_loaded;

    save(conn, &s)
}

/// Set protocol_mode=true (PROTOCOL session activated).
pub fn activate_protocol_mode(conn: &Connection, session_key: &str) -> Result<()> {
    let mut s = load_or_create(conn, session_key)?;
    s.protocol_mode = true;
    save(conn, &s)
}

/// Check whether the bootstrap phase is complete for the given session.
pub fn is_bootstrap_complete(conn: &Connection, session_key: &str) -> Result<bool> {
    let s = load_or_create(conn, session_key)?;
    Ok(s.bootstrap_complete)
}

/// Check whether the session is in PROTOCOL mode.
pub fn is_protocol_mode(conn: &Connection, session_key: &str) -> Result<bool> {
    let s = load_or_create(conn, session_key)?;
    Ok(s.protocol_mode)
}

/// Generate a human-readable gate error message for the agent.
pub fn gate_error_message(session_key: &str, conn: &Connection) -> Result<String> {
    let s = load_or_create(conn, session_key)?;

    let mut missing = Vec::new();
    if !s.delta_retrieved      { missing.push("get_delta"); }
    if !s.preferences_loaded   { missing.push("get_preferences"); }
    if !s.anti_patterns_loaded { missing.push("get_anti_patterns"); }
    if !s.context_loaded       { missing.push("get_context"); }

    Ok(format!(
        "PROTOCOL_PHASE_0_INCOMPLETE — work tools are blocked until bootstrap finishes.\n\
         Missing steps: {}\n\
         Run: begin_protocol_session first, then call the missing tools above.\n\
         Use get_protocol_status to see the full session health.",
        missing.join(", ")
    ))
}

/// Generate the full status report returned by get_protocol_status and get_session_health.
pub fn status_report(
    conn: &Connection,
    session_key: &str,
    pending_obs_count: usize,
    marker_count: Option<(usize, usize, usize)>,  // (patterns, anti_patterns, corrections)
    top_gaps: &[(String, i64)],
    pattern_health: &PatternHealthSummary,
    pending_proposals: usize,
) -> Result<String> {
    let s = load_or_create(conn, session_key)?;

    let phase0_status = if s.bootstrap_complete { "✓ complete" } else { "✗ INCOMPLETE" };
    let delta_mark    = if s.delta_retrieved      { "✓" } else { "✗" };
    let prefs_mark    = if s.preferences_loaded   { "✓" } else { "✗" };
    let ap_mark       = if s.anti_patterns_loaded { "✓" } else { "✗" };
    let ctx_mark      = if s.context_loaded       { "✓" } else { "✗" };
    let mode_str      = if s.protocol_mode        { "PROTOCOL" } else { "casual" };
    let closeout_str  = if s.closeout_run         { "✓ complete" } else { "✗ not yet run" };

    let mut out = format!(
        "SESSION HEALTH\nMode: {} | session_key: {}\n\n",
        mode_str, session_key
    );

    out.push_str(&format!(
        "Phase 0 (Bootstrap): {} (delta {} prefs {} anti-patterns {} context {})\n",
        phase0_status, delta_mark, prefs_mark, ap_mark, ctx_mark
    ));

    if let Some((patterns, anti_patterns, corrections)) = marker_count {
        out.push_str(&format!(
            "Knowledge markers written: {} patterns, {} anti-patterns, {} corrections\n",
            patterns, anti_patterns, corrections
        ));
    }

    out.push_str(&format!("Pending observations: {}", pending_obs_count));
    if pending_obs_count > 0 {
        out.push_str(" (run `cortex review` to crystallize)");
    }
    out.push('\n');

    out.push_str(&format!("Closeout: {}\n", closeout_str));

    if !top_gaps.is_empty() {
        out.push_str("Top query gaps (7d):\n");
        for (query, count) in top_gaps.iter().take(3) {
            out.push_str(&format!("  \"{}\" ({} misses)\n", query, count));
        }
    }

    out.push_str(&format!(
        "Pattern health: {} total, {} below 40% survival\n",
        pattern_health.total, pattern_health.low_survival
    ));

    if pending_proposals > 0 {
        out.push_str(&format!(
            "Cross-session proposals: {} pending review (run `cortex.ps1 review-proposals`)\n",
            pending_proposals
        ));
    }

    // Recommendation
    if !s.bootstrap_complete && s.protocol_mode {
        let mut missing = Vec::new();
        if !s.delta_retrieved      { missing.push("get_delta"); }
        if !s.preferences_loaded   { missing.push("get_preferences"); }
        if !s.anti_patterns_loaded { missing.push("get_anti_patterns"); }
        if !s.context_loaded       { missing.push("get_context"); }
        out.push_str(&format!(
            "\nRecommendation: complete Phase 0 — call: {}",
            missing.join(", ")
        ));
    } else if !s.closeout_run {
        out.push_str("\nRecommendation: call closeout_session(outcome_type=\"build_pass|build_fail|...\")");
    } else {
        out.push_str("\nSession is closed. ✓");
    }

    Ok(out)
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ProtocolSession {
    pub session_key:               String,
    pub protocol_mode:             bool,
    pub delta_retrieved:           bool,
    pub preferences_loaded:        bool,
    pub anti_patterns_loaded:      bool,
    pub context_loaded:            bool,
    pub bootstrap_complete:        bool,
    pub closeout_run:              bool,
    pub outcome_type:              Option<String>,
    pub closed_at:                 Option<i64>,
    pub knowledge_markers_flushed: bool,
    pub inline_approved:           bool,
    pub graph_snapshot_written:    bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolStep {
    GetDelta,
    GetPreferences,
    GetAntiPatterns,
    GetContext,
}

#[derive(Debug, Default)]
pub struct PatternHealthSummary {
    pub total:        usize,
    pub low_survival: usize,
}

// ── Row mapper ────────────────────────────────────────────────────────────────

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProtocolSession> {
    Ok(ProtocolSession {
        session_key:               row.get(0)?,
        protocol_mode:             row.get::<_, i64>(1)? != 0,
        delta_retrieved:           row.get::<_, i64>(2)? != 0,
        preferences_loaded:        row.get::<_, i64>(3)? != 0,
        anti_patterns_loaded:      row.get::<_, i64>(4)? != 0,
        context_loaded:            row.get::<_, i64>(5)? != 0,
        bootstrap_complete:        row.get::<_, i64>(6)? != 0,
        closeout_run:              row.get::<_, i64>(7)? != 0,
        outcome_type:              row.get(8)?,
        closed_at:                 row.get(9)?,
        knowledge_markers_flushed: row.get::<_, i64>(10)? != 0,
        inline_approved:           row.get::<_, i64>(11)? != 0,
        graph_snapshot_written:    row.get::<_, i64>(12)? != 0,
    })
}

// ── Queries used by status / get_session_health ───────────────────────────────

/// Return top N query gaps from the last 7 days.
pub fn top_query_gaps(conn: &Connection, limit: usize) -> Result<Vec<(String, i64)>> {
    let cutoff = Utc::now().timestamp() - 7 * 86400;
    let mut stmt = conn.prepare(
        "SELECT query_text, seen_count FROM query_gap_log
         WHERE last_seen_at >= ?1
         ORDER BY seen_count DESC LIMIT ?2"
    )?;
    let rows = stmt.query_map(params![cutoff, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Count pending proposals (Tier 2 cross-session proposals).
pub fn pending_proposal_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM proposals WHERE status = 'pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Summarise pattern health (total count and low-survival count).
pub fn pattern_health_summary(conn: &Connection) -> Result<PatternHealthSummary> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM patterns", [], |r| r.get(0),
    )?;
    let low: i64 = conn.query_row(
        "SELECT COUNT(*) FROM patterns WHERE survival_rate < 0.4", [], |r| r.get(0),
    )?;
    Ok(PatternHealthSummary {
        total: total as usize,
        low_survival: low as usize,
    })
}

/// Count knowledge markers written this session (by type).
pub fn session_marker_counts(
    conn: &Connection,
    session_key: &str,
) -> Result<(usize, usize, usize)> {
    let patterns: i64 = conn.query_row(
        "SELECT COUNT(*) FROM knowledge_markers
         WHERE session_key = ?1 AND marker_type = 'pattern'",
        params![session_key], |r| r.get(0),
    ).unwrap_or(0);
    let aps: i64 = conn.query_row(
        "SELECT COUNT(*) FROM knowledge_markers
         WHERE session_key = ?1 AND marker_type = 'anti_pattern'",
        params![session_key], |r| r.get(0),
    ).unwrap_or(0);
    let corrections: i64 = conn.query_row(
        "SELECT COUNT(*) FROM knowledge_markers
         WHERE session_key = ?1 AND marker_type = 'correction'",
        params![session_key], |r| r.get(0),
    ).unwrap_or(0);
    Ok((patterns as usize, aps as usize, corrections as usize))
}
