/// Self-learning scoreboard: the system's answer to "are the agents getting smarter?"
///
/// Computes a small set of KPIs over a rolling window and compares against the
/// previous window of the same length, so every number carries a trend:
///
///   1. build_pass rate      — outcomes ending in build_pass vs build/test failures
///   2. gap rate / session   — retrieval misses per closed session (falling = smarter)
///   3. marker rate / session— knowledge markers captured per closed session (rising = learning)
///   4. pattern reuse rate   — targeted pattern retrievals (recall/get_context) per session
///   5. telemetry coverage   — % of patterns with any real usage signal
///
/// Plus point-in-time skill pipeline counts (candidate → drafted → approved).
///
/// Exposed as the `cortex scoreboard` CLI subcommand and as a compact line in
/// the `get_session_health` MCP tool so agents see the trend every session.
use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use crate::memory::Store;

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize)]
pub struct WindowStats {
    pub sessions_closed:   i64,
    pub outcomes_total:    i64,
    pub build_pass:        i64,
    pub build_fail:        i64,
    /// build_pass / (build_pass + build_fail + test_fail); 0.0 when no signal.
    pub pass_rate:         f32,
    pub gaps_active:       i64,
    pub gap_rate:          f32,
    pub markers_captured:  i64,
    pub marker_rate:       f32,
    pub pattern_retrievals: i64,
    pub reuse_rate:        f32,
    /// Lossless output-compaction telemetry (the accuracy/savings instrument).
    pub compact_calls:     i64,
    /// Mean filtered/original ratio — LOWER is better (more redundancy removed).
    /// Calls whose output actually shrank. The remainder are correct no-ops.
    pub compact_applicable: i64,
    /// Mean ratio over `compact_applicable` calls only — see compute().
    pub avg_compression_ratio: f32,
    /// Estimated tokens saved this window: (original - filtered) chars / 4.
    pub est_tokens_saved:  i64,
}

#[derive(Debug, Default, Serialize)]
pub struct Scoreboard {
    pub generated_at:        String,
    pub window_days:         u32,
    pub current:             WindowStats,
    pub previous:            WindowStats,
    // Point-in-time knowledge-store health.
    pub patterns_total:      i64,
    pub patterns_with_usage: i64,
    pub telemetry_coverage:  f32,
    pub anti_patterns_total: i64,
    pub skills_candidate:    i64,
    pub skills_drafted:      i64,
    pub skills_approved:     i64,
}

// ── Computation ───────────────────────────────────────────────────────────────

fn window_stats(store: &Store, start: i64, end: i64) -> WindowStats {
    let one = |sql: &str| -> i64 {
        store.conn().query_row(sql, rusqlite::params![start, end], |r| r.get(0)).unwrap_or(0)
    };

    let sessions_closed = one(
        "SELECT COUNT(*) FROM protocol_sessions
         WHERE closeout_run = 1 AND closed_at >= ?1 AND closed_at < ?2");

    let outcomes_total = one(
        "SELECT COUNT(*) FROM outcome_log WHERE created_at >= ?1 AND created_at < ?2");
    let build_pass = one(
        "SELECT COUNT(*) FROM outcome_log
         WHERE outcome_type = 'build_pass' AND created_at >= ?1 AND created_at < ?2");
    let build_fail = one(
        "SELECT COUNT(*) FROM outcome_log
         WHERE outcome_type IN ('build_fail','test_fail') AND created_at >= ?1 AND created_at < ?2");

    let gaps_active = one(
        "SELECT COUNT(*) FROM query_gap_log WHERE last_seen_at >= ?1 AND last_seen_at < ?2");

    let markers_captured = one(
        "SELECT COUNT(*) FROM knowledge_markers WHERE extracted_at >= ?1 AND extracted_at < ?2");

    // Targeted retrievals only — recall topic matches and get_context relevance hits.
    // Bulk listings (list_patterns / get_anti_patterns) are browsing, not reuse.
    let pattern_retrievals = one(
        "SELECT COUNT(*) FROM session_retrieval_log
         WHERE entry_table = 'patterns' AND tool_name IN ('recall','get_context')
         AND retrieved_at >= ?1 AND retrieved_at < ?2");

    let denom_sessions = sessions_closed.max(1) as f32;
    let denom_outcomes = (build_pass + build_fail).max(1) as f32;

    // Compression telemetry over the window.
    let compact_calls = one(
        "SELECT COUNT(*) FROM compression_savings WHERE saved_at >= ?1 AND saved_at < ?2");
    let real = |sql: &str| -> f64 {
        store.conn().query_row(sql, rusqlite::params![start, end], |r| r.get(0)).unwrap_or(0.0)
    };
    // Ratio is averaged over calls that actually had something to drop. Averaging
    // across every call instead buries the signal: the filter is lossless by
    // construction, so it correctly no-ops on output with no noise (grep, sed,
    // git), and those 1.0 ratios drag the mean to ~0.99 no matter how well it
    // compresses the cargo/build output it is actually for. Applicability is
    // reported separately because it measures the workload, not the filter.
    let avg_compression_ratio = real(
        "SELECT COALESCE(AVG(ratio), 0.0) FROM compression_savings
         WHERE saved_at >= ?1 AND saved_at < ?2 AND filtered_chars < original_chars") as f32;
    let compact_applicable = one(
        "SELECT COUNT(*) FROM compression_savings
         WHERE saved_at >= ?1 AND saved_at < ?2 AND filtered_chars < original_chars");
    let saved_chars = one(
        "SELECT COALESCE(SUM(original_chars - filtered_chars), 0) FROM compression_savings
         WHERE saved_at >= ?1 AND saved_at < ?2");

    WindowStats {
        sessions_closed,
        outcomes_total,
        build_pass,
        build_fail,
        pass_rate: build_pass as f32 / denom_outcomes,
        gaps_active,
        gap_rate: gaps_active as f32 / denom_sessions,
        markers_captured,
        marker_rate: markers_captured as f32 / denom_sessions,
        pattern_retrievals,
        reuse_rate: pattern_retrievals as f32 / denom_sessions,
        compact_calls,
        compact_applicable,
        avg_compression_ratio,
        est_tokens_saved: saved_chars / 4,
    }
}

/// Compute the full scoreboard: current window vs the previous window.
pub fn compute(store: &Store, window_days: u32) -> Result<Scoreboard> {
    let now = Utc::now().timestamp();
    let w   = window_days as i64 * 86400;

    let current  = window_stats(store, now - w, now + 1);
    let previous = window_stats(store, now - 2 * w, now - w);

    let count = |sql: &str| -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0)
    };

    let patterns_total      = count("SELECT COUNT(*) FROM patterns");
    let patterns_with_usage = count("SELECT COUNT(*) FROM patterns WHERE use_count > 0");
    let anti_patterns_total = count("SELECT COUNT(*) FROM anti_patterns");
    let skills_candidate    = count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'candidate'");
    let skills_drafted      = count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'drafted'");
    let skills_approved     = count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'approved'");

    Ok(Scoreboard {
        generated_at: Utc::now().to_rfc3339(),
        window_days,
        current,
        previous,
        patterns_total,
        patterns_with_usage,
        telemetry_coverage: if patterns_total > 0 {
            patterns_with_usage as f32 / patterns_total as f32
        } else { 0.0 },
        anti_patterns_total,
        skills_candidate,
        skills_drafted,
        skills_approved,
    })
}

// ── Formatting ────────────────────────────────────────────────────────────────

/// Trend marker comparing current vs previous. `higher_is_better` flips polarity.
fn trend(current: f32, previous: f32, higher_is_better: bool) -> &'static str {
    let delta = current - previous;
    if delta.abs() < 0.005 { return "→"; }
    let improving = (delta > 0.0) == higher_is_better;
    if improving { "↑ improving" } else { "↓ regressing" }
}

pub fn format_text(sb: &Scoreboard) -> String {
    let c = &sb.current;
    let p = &sb.previous;
    let mut out = format!(
        "SELF-LEARNING SCOREBOARD ({}d window vs previous {}d)\n\n",
        sb.window_days, sb.window_days
    );
    out.push_str(&format!(
        "  Sessions closed:     {} (prev {})\n", c.sessions_closed, p.sessions_closed));
    out.push_str(&format!(
        "  Build pass rate:     {:.0}%  (prev {:.0}%)  {}\n",
        c.pass_rate * 100.0, p.pass_rate * 100.0,
        trend(c.pass_rate, p.pass_rate, true)));
    out.push_str(&format!(
        "  Gaps / session:      {:.2} (prev {:.2})  {}   [{} active gaps]\n",
        c.gap_rate, p.gap_rate, trend(c.gap_rate, p.gap_rate, false), c.gaps_active));
    out.push_str(&format!(
        "  Markers / session:   {:.2} (prev {:.2})  {}   [{} captured]\n",
        c.marker_rate, p.marker_rate, trend(c.marker_rate, p.marker_rate, true),
        c.markers_captured));
    out.push_str(&format!(
        "  Pattern reuse rate:  {:.2} (prev {:.2})  {}   [{} targeted retrievals]\n",
        c.reuse_rate, p.reuse_rate, trend(c.reuse_rate, p.reuse_rate, true),
        c.pattern_retrievals));
    out.push_str(&format!(
        "  Compaction:          {}/{} calls compressible ({}%), avg ratio {:.2} on those \
         (prev {:.2}) {}   [~{} tokens saved, lossless]\n",
        c.compact_applicable, c.compact_calls,
        if c.compact_calls > 0 { 100 * c.compact_applicable / c.compact_calls } else { 0 },
        c.avg_compression_ratio, p.avg_compression_ratio,
        trend(c.avg_compression_ratio, p.avg_compression_ratio, false), c.est_tokens_saved));
    out.push_str(&format!(
        "\n  Knowledge store: {} patterns ({} with usage signal — {:.0}% telemetry coverage), {} anti-patterns\n",
        sb.patterns_total, sb.patterns_with_usage,
        sb.telemetry_coverage * 100.0, sb.anti_patterns_total));
    out.push_str(&format!(
        "  Skill pipeline:  {} candidate → {} drafted → {} approved\n",
        sb.skills_candidate, sb.skills_drafted, sb.skills_approved));
    out
}

/// One-line compact scoreboard for embedding in get_session_health.
pub fn compact_line(store: &Store) -> String {
    match compute(store, 14) {
        Ok(sb) => {
            let c = &sb.current;
            let p = &sb.previous;
            format!(
                "Scoreboard (14d): pass {:.0}% {} | gaps/ses {:.1} {} | markers/ses {:.1} {} | reuse/ses {:.1} {} | telemetry {:.0}%",
                c.pass_rate * 100.0, trend(c.pass_rate, p.pass_rate, true),
                c.gap_rate,          trend(c.gap_rate, p.gap_rate, false),
                c.marker_rate,       trend(c.marker_rate, p.marker_rate, true),
                c.reuse_rate,        trend(c.reuse_rate, p.reuse_rate, true),
                sb.telemetry_coverage * 100.0,
            ) + &if c.compact_calls > 0 {
                format!(
                    " | compaction {} calls, ratio {:.2}, ~{} tok saved",
                    c.compact_calls, c.avg_compression_ratio, c.est_tokens_saved
                )
            } else {
                String::new()
            }
        }
        Err(_) => "Scoreboard: unavailable".to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A store of its own, removed when the test ends -- see test_support.
    fn test_store(name: &str) -> crate::test_support::TempStore {
        crate::test_support::TempStore::new(name).unwrap()
    }

    #[test]
    fn empty_db_scoreboard() {
        let store = test_store("empty");
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.sessions_closed, 0);
        assert_eq!(sb.current.pass_rate, 0.0);
        assert_eq!(sb.patterns_total, 0);
    }

    #[test]
    fn pass_rate_counts_only_build_outcomes() {
        let store = test_store("passrate");
        // 2 pass, 1 fail, 1 research_only (excluded from rate denominator).
        for (i, ot) in ["build_pass", "build_pass", "build_fail", "research_only"].iter().enumerate() {
            store.log_outcome(&format!("s{i}"), ot, None, None).unwrap();
        }
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.build_pass, 2);
        assert_eq!(sb.current.build_fail, 1);
        assert!((sb.current.pass_rate - 2.0 / 3.0).abs() < 0.01);
        assert_eq!(sb.current.outcomes_total, 4);
    }

    #[test]
    fn compression_savings_roll_into_window() {
        let store = test_store("compaction");
        // 90% reduction on a 4000-char output twice.
        store.log_compression_saving("s0", "cargo test", 4000, 400).unwrap();
        store.log_compression_saving("s1", "cargo build", 2000, 200).unwrap();
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.compact_calls, 2);
        assert!((sb.current.avg_compression_ratio - 0.10).abs() < 0.01, "avg ratio ~0.10");
        // (4000-400)+(2000-200) = 5400 chars / 4 = 1350 tokens.
        assert_eq!(sb.current.est_tokens_saved, 1350);
    }

    #[test]
    fn trend_polarity() {
        assert_eq!(trend(0.8, 0.5, true), "↑ improving");
        assert_eq!(trend(0.3, 0.5, true), "↓ regressing");
        // Lower-is-better metric (gap rate).
        assert_eq!(trend(0.3, 0.5, false), "↑ improving");
        assert_eq!(trend(0.5, 0.5, false), "→");
    }
}
