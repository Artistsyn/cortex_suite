/// Self-learning scoreboard: the system's answer to "are the agents getting smarter,
/// and what does the memory cost?"
///
/// v2 (2026-09-27). The first version reported five KPIs, and an audit against
/// the raw tables found that four of them measured something other than their
/// label:
///
///   - "build pass rate 100%" came from closeouts, which are only ever run after
///     a success. No failure had been recorded that way in thirty days, while
///     the hook-observed runs in the same window passed 65% of the time.
///   - "pattern reuse 38/session ↑6.5×" counted ROWS: 144 of its 228 came from
///     one loop of 36 identical get_context calls in nine minutes.
///   - "gaps/session ↓ improving" counted distinct queries; the only active gap
///     was one name missed 36 times in that same loop.
///   - "compaction ~686k tokens saved" was counterfactual -- a hook cannot
///     replace a Bash result, and not one compacted copy reached an agent.
///
/// Every number below is therefore one of three kinds, and says which:
///   OBSERVED   recorded automatically by hooks, with no one choosing when
///   DELIVERED  what reached an agent's context, in a form the host shows it
///   SELF-REPORTED  written by an agent at closeout (kept, labelled, not trusted)
///
/// Denominators are sessions that WORKED, not sessions that closed out --
/// closeout covers a minority of sessions, and dividing everything-that-happened
/// by closeouts-only inflated every rate.
///
/// The token ledger reads Claude Code's own transcripts (their `usage` fields),
/// so it reports the bill that was actually paid, not a projection.
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use crate::memory::Store;

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize)]
pub struct WindowStats {
    // Work
    /// Sessions with any hook or tool activity -- the denominator.
    pub sessions_worked: i64,
    /// Sessions that ran closeout (SELF-REPORTED coverage).
    pub sessions_closed: i64,

    // OBSERVED outcomes: every build/test run the Bash hook classified.
    pub runs: i64,
    pub runs_passed: i64,
    pub run_pass_rate: f32,
    pub sessions_with_runs: i64,
    /// Sessions whose LAST run in the window was green.
    pub sessions_ended_green: i64,

    // SELF-REPORTED outcomes (closeout only; biased toward success by design).
    pub closeout_pass: i64,
    pub closeout_fail: i64,

    // OBSERVED repeat failures -- the "is it learning" number.
    /// Distinct failure signatures hit in the window.
    pub failures_distinct: i64,
    /// Of those, first seen BEFORE the window: an old failure that came back,
    /// which is exactly what a memory is supposed to prevent.
    pub failures_returned: i64,
    /// Of those returned, still unrecorded (no trap, not dismissed).
    pub returned_unrecorded: i64,
    /// Failures hit again after a trap or dismissal recorded them.
    pub hit_after_recorded: i64,

    // DELIVERY: what reached agents.
    /// Targeted lookups (recall, get_context, semantic_search, get_item, ...),
    /// identical calls within ten minutes counted once.
    pub lookups_targeted: i64,
    /// Identical repeats that were NOT counted above -- a loop detector.
    pub lookups_repeated: i64,
    /// Bulk listings (get_anti_patterns, list_patterns, get_preferences).
    pub lookups_bulk: i64,
    /// Pushes delivered as hook additionalContext, by mechanism.
    pub pushes_delivered: i64,
    pub pushes_by_mechanism: Vec<(String, i64)>,
    /// Edit-guard fires with no delivered push -- computed and never seen.
    pub guard_fires_undelivered: i64,
    /// Distinct lookup queries that missed, and how many of them are looping.
    pub gap_queries: i64,
    pub gap_loops: i64,

    // Capture
    pub markers_captured: i64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct TokenWindow {
    pub api_calls: i64,
    pub input_fresh: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    pub output: i64,
    pub compactions: i64,
    /// Median context size of a session's first call -- the fixed prompt.
    pub first_call_context_median: i64,
    /// Tokens of tool results injected, by source (chars / 4).
    pub injected_cortex: i64,
    pub injected_structure: i64,
    pub injected_bash: i64,
    /// Re-reads of that content: each call re-reads everything resident since
    /// the last compaction (chars / 4, summed per call).
    pub reread_cortex: i64,
    pub reread_bash: i64,
    /// cortex pushes the HOST recorded as delivered context
    /// (`hook_additional_context` attachments) -- confirmation from the other
    /// side of the boundary, not cortex's own claim.
    pub cortex_contexts_delivered: i64,
    /// cortex hook calls the host reported as failed (server not connected,
    /// unknown tool, unparseable output).
    pub cortex_hook_errors: i64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct TokenLedger {
    pub transcripts_dir: String,
    pub files_scanned: usize,
    pub current: TokenWindow,
    pub previous: TokenWindow,
}

#[derive(Debug, Default, Serialize)]
pub struct Scoreboard {
    pub generated_at: String,
    pub window_days: u32,
    pub current: WindowStats,
    pub previous: WindowStats,
    // Point-in-time knowledge-store health. LIVE rows only: retired entries are
    // never served and must not pad the totals.
    pub patterns_live: i64,
    pub patterns_retired: i64,
    pub patterns_with_usage: i64,
    pub telemetry_coverage: f32,
    pub anti_patterns_live: i64,
    pub anti_patterns_retired: i64,
    pub challenges_unsettled: i64,
    pub skills_candidate: i64,
    pub skills_drafted: i64,
    pub skills_approved: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenLedger>,
}

// ── Computation ───────────────────────────────────────────────────────────────

const TARGETED_TOOLS: &str =
    "'recall','get_context','semantic_search','get_item','expand_memory','query_graph','get_usage_examples'";
const BULK_TOOLS: &str = "'get_anti_patterns','list_patterns','get_preferences'";

fn window_stats(store: &Store, start: i64, end: i64) -> WindowStats {
    let one = |sql: &str| -> i64 {
        store.conn().query_row(sql, rusqlite::params![start, end], |r| r.get(0)).unwrap_or(0)
    };

    let sessions_worked = one(
        "SELECT COUNT(*) FROM (
             SELECT session_id AS s FROM test_outcomes
              WHERE observed_at >= ?1 AND observed_at < ?2
             UNION SELECT session_key FROM compression_savings
              WHERE saved_at >= ?1 AND saved_at < ?2
             UNION SELECT session_id FROM session_retrieval_log
              WHERE retrieved_at >= ?1 AND retrieved_at < ?2
             UNION SELECT logical_session_key FROM mcp_calls
              WHERE logical_session_key IS NOT NULL
                AND unixepoch(called_at) >= ?1 AND unixepoch(called_at) < ?2)",
    );
    let sessions_closed = one(
        "SELECT COUNT(*) FROM protocol_sessions
         WHERE closeout_run = 1 AND closed_at >= ?1 AND closed_at < ?2",
    );

    let runs = one("SELECT COUNT(*) FROM test_outcomes WHERE observed_at >= ?1 AND observed_at < ?2");
    let runs_passed = one(
        "SELECT COALESCE(SUM(passed), 0) FROM test_outcomes WHERE observed_at >= ?1 AND observed_at < ?2",
    );
    let (sessions_with_runs, sessions_ended_green) = store
        .conn()
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(passed), 0) FROM test_outcomes t
             WHERE t.observed_at >= ?1 AND t.observed_at < ?2
               AND t.id = (SELECT MAX(t2.id) FROM test_outcomes t2
                           WHERE t2.session_id = t.session_id
                             AND t2.observed_at >= ?1 AND t2.observed_at < ?2)",
            rusqlite::params![start, end],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap_or((0, 0));

    let closeout_pass = one(
        "SELECT COUNT(*) FROM outcome_log
         WHERE outcome_type = 'build_pass' AND created_at >= ?1 AND created_at < ?2",
    );
    let closeout_fail = one(
        "SELECT COUNT(*) FROM outcome_log
         WHERE outcome_type IN ('build_fail','test_fail') AND created_at >= ?1 AND created_at < ?2",
    );

    // A failure's LAST sighting decides which window it counts in, so a past
    // window's figures shrink as its failures recur later. Only the current
    // window is exact; the previous one is a floor, not a record.
    let failures_distinct =
        one("SELECT COUNT(*) FROM recurring_errors WHERE last_seen_at >= ?1 AND last_seen_at < ?2");
    let failures_returned = one(
        "SELECT COUNT(*) FROM recurring_errors
         WHERE last_seen_at >= ?1 AND last_seen_at < ?2 AND first_seen_at < ?1
           AND json_array_length(sessions) >= 2",
    );
    let returned_unrecorded = one(
        "SELECT COUNT(*) FROM recurring_errors
         WHERE last_seen_at >= ?1 AND last_seen_at < ?2 AND first_seen_at < ?1
           AND json_array_length(sessions) >= 2 AND proposed = 0",
    );
    let hit_after_recorded = one(
        "SELECT COUNT(*) FROM recurring_errors
         WHERE handled_at IS NOT NULL AND last_seen_at > handled_at
           AND last_seen_at >= ?1 AND last_seen_at < ?2",
    );

    let targeted_total = one(&format!(
        "SELECT COUNT(*) FROM mcp_calls WHERE tool IN ({TARGETED_TOOLS})
           AND unixepoch(called_at) >= ?1 AND unixepoch(called_at) < ?2"
    ));
    let lookups_targeted = one(&format!(
        "SELECT COUNT(*) FROM (
             SELECT DISTINCT tool, args, COALESCE(logical_session_key, ''), unixepoch(called_at) / 600
               FROM mcp_calls WHERE tool IN ({TARGETED_TOOLS})
                AND unixepoch(called_at) >= ?1 AND unixepoch(called_at) < ?2)"
    ));
    let lookups_bulk = one(&format!(
        "SELECT COUNT(*) FROM mcp_calls WHERE tool IN ({BULK_TOOLS})
           AND unixepoch(called_at) >= ?1 AND unixepoch(called_at) < ?2"
    ));

    let pushes_by_mechanism: Vec<(String, i64)> = store
        .conn()
        .prepare(
            "SELECT mechanism, COUNT(*) FROM push_log
             WHERE pushed_at >= ?1 AND pushed_at < ?2 GROUP BY mechanism ORDER BY 2 DESC",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![start, end], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    let pushes_delivered = pushes_by_mechanism.iter().map(|(_, n)| n).sum();
    let guard_fires_undelivered = one(
        "SELECT COUNT(*) FROM edit_guard_fires f
         WHERE unixepoch(f.fired_at) >= ?1 AND unixepoch(f.fired_at) < ?2
           AND NOT EXISTS (SELECT 1 FROM push_log p
                           WHERE p.session_id = f.session_id AND p.mechanism = 'edit_guard'
                             AND p.key = CAST(f.anti_pattern_id AS TEXT))",
    );
    let gap_queries =
        one("SELECT COUNT(*) FROM query_gap_log WHERE last_seen_at >= ?1 AND last_seen_at < ?2");
    let gap_loops = one(
        "SELECT COUNT(*) FROM query_gap_log
         WHERE last_seen_at >= ?1 AND last_seen_at < ?2 AND seen_count >= 10",
    );

    let markers_captured =
        one("SELECT COUNT(*) FROM knowledge_markers WHERE extracted_at >= ?1 AND extracted_at < ?2");

    WindowStats {
        sessions_worked,
        sessions_closed,
        runs,
        runs_passed,
        run_pass_rate: if runs > 0 { runs_passed as f32 / runs as f32 } else { 0.0 },
        sessions_with_runs,
        sessions_ended_green,
        closeout_pass,
        closeout_fail,
        failures_distinct,
        failures_returned,
        returned_unrecorded,
        hit_after_recorded,
        lookups_targeted,
        lookups_repeated: (targeted_total - lookups_targeted).max(0),
        lookups_bulk,
        pushes_delivered,
        pushes_by_mechanism,
        guard_fires_undelivered,
        gap_queries,
        gap_loops,
        markers_captured,
    }
}

/// Compute the scoreboard from the store: current window vs the previous one.
/// The token ledger is separate (`token_ledger`) because it reads transcripts.
pub fn compute(store: &Store, window_days: u32) -> Result<Scoreboard> {
    let now = Utc::now().timestamp();
    let w = window_days as i64 * 86400;

    let current = window_stats(store, now - w, now + 1);
    let previous = window_stats(store, now - 2 * w, now - w);

    let count = |sql: &str| -> i64 { store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let patterns_live = count("SELECT COUNT(*) FROM patterns WHERE superseded_by IS NULL");
    let patterns_with_usage =
        count("SELECT COUNT(*) FROM patterns WHERE superseded_by IS NULL AND use_count > 0");

    Ok(Scoreboard {
        generated_at: Utc::now().to_rfc3339(),
        window_days,
        current,
        previous,
        patterns_live,
        patterns_retired: count("SELECT COUNT(*) FROM patterns WHERE superseded_by IS NOT NULL"),
        patterns_with_usage,
        telemetry_coverage: if patterns_live > 0 {
            patterns_with_usage as f32 / patterns_live as f32
        } else {
            0.0
        },
        anti_patterns_live: count("SELECT COUNT(*) FROM anti_patterns WHERE superseded_by IS NULL"),
        anti_patterns_retired: count(
            "SELECT COUNT(*) FROM anti_patterns WHERE superseded_by IS NOT NULL",
        ),
        challenges_unsettled: count("SELECT COUNT(*) FROM challenges WHERE verdict IS NULL"),
        skills_candidate: count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'candidate'"),
        skills_drafted: count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'drafted'"),
        skills_approved: count("SELECT COUNT(*) FROM skill_candidates WHERE status = 'approved'"),
        tokens: None,
    })
}

// ── Token ledger (Claude Code transcripts) ───────────────────────────────────

/// Where Claude Code keeps this workspace's transcripts: `~/.claude/projects/`
/// plus the workspace path with every non-alphanumeric character replaced by
/// `-` (`/Users/u/FlowMake` -> `-Users-u-FlowMake`). `CORTEX_TRANSCRIPTS_DIR`
/// overrides it.
pub fn transcripts_dir_for(repo_root: &Path) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CORTEX_TRANSCRIPTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    // `.cortex/memory.db` has an EMPTY grandparent, and canonicalize("") fails.
    let base = if repo_root.as_os_str().is_empty() { Path::new(".") } else { repo_root };
    let root = base.canonicalize().ok()?;
    let slug: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let dir = PathBuf::from(home).join(".claude").join("projects").join(slug);
    dir.is_dir().then_some(dir)
}

/// The unix time of a transcript line, from its `"timestamp":"..."` field,
/// without parsing the (often megabyte) line.
fn line_time(line: &str) -> Option<i64> {
    let i = line.find("\"timestamp\":\"")? + 13;
    let ts = line.get(i..i + 19)?;
    chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S").ok().map(|t| t.and_utc().timestamp())
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Cortex,
    Structure,
    Bash,
    Other,
}

fn source_of(tool: &str) -> Source {
    if tool.starts_with("mcp__cortex__") {
        Source::Cortex
    } else if tool.starts_with("mcp__quartz-ctx__") || tool.starts_with("mcp__graphify__") {
        Source::Structure
    } else if tool == "Bash" {
        Source::Bash
    } else {
        Source::Other
    }
}

fn text_len(content: &serde_json::Value) -> i64 {
    match content {
        serde_json::Value::String(s) => s.len() as i64,
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(|t| t.len() as i64)
            .sum(),
        _ => 0,
    }
}

/// Read the bill that was actually paid, per window, from transcript `usage`
/// fields. Lines before the previous window are skipped without parsing, so the
/// cost is proportional to recent activity, not to transcript size.
pub fn token_ledger(dir: &Path, window_days: u32) -> Result<TokenLedger> {
    let now = Utc::now().timestamp();
    let w = window_days as i64 * 86400;
    let (prev_start, cur_start) = (now - 2 * w, now - w);

    let mut ledger = TokenLedger { transcripts_dir: dir.display().to_string(), ..Default::default() };
    let mut first_calls: [Vec<i64>; 2] = [Vec::new(), Vec::new()];
    let mut seen_msgs: HashSet<String> = HashSet::new();

    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let recent = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() as i64 >= prev_start);
        if !recent {
            continue;
        }
        ledger.files_scanned += 1;

        let reader = std::io::BufReader::new(std::fs::File::open(&path)?);
        let mut tool_names: HashMap<String, Source> = HashMap::new();
        // Resident tool-result chars by source since the last compaction.
        let (mut res_cortex, mut res_bash) = (0i64, 0i64);
        let mut first_call_seen = false;

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let Some(t) = line_time(&line) else { continue };
            if t < prev_start {
                continue;
            }
            let win = if t >= cur_start { &mut ledger.current } else { &mut ledger.previous };
            let slot = usize::from(t >= cur_start);

            if line.contains("\"subtype\":\"compact_boundary\"") {
                win.compactions += 1;
                res_cortex = 0;
                res_bash = 0;
                continue;
            }
            // Hook attachments: what the host did with cortex's hook output.
            if line.contains("\"type\":\"attachment\"") {
                if line.contains("\"hook_additional_context\"") && line.contains("[cortex]") {
                    win.cortex_contexts_delivered += 1;
                } else if line.contains("\"hook_non_blocking_error\"") && line.contains("cortex") {
                    win.cortex_hook_errors += 1;
                }
                continue;
            }
            let is_usage = line.contains("\"usage\":{");
            let is_result = line.contains("\"tool_result\"");
            if !is_usage && !is_result {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let Some(msg) = v.get("message") else { continue };
            let content = msg.get("content").and_then(|c| c.as_array());

            if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(blocks) = content {
                    for b in blocks {
                        if b.get("type").and_then(|x| x.as_str()) == Some("tool_use") {
                            if let (Some(id), Some(name)) = (
                                b.get("id").and_then(|x| x.as_str()),
                                b.get("name").and_then(|x| x.as_str()),
                            ) {
                                tool_names.insert(id.to_string(), source_of(name));
                            }
                        }
                    }
                }
                // One API response is split over several records; count it once.
                let id = msg.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if id.is_empty() || !seen_msgs.insert(id) {
                    continue;
                }
                let Some(u) = msg.get("usage") else { continue };
                let g = |k: &str| u.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                let (fresh, write, read) =
                    (g("input_tokens"), g("cache_creation_input_tokens"), g("cache_read_input_tokens"));
                win.api_calls += 1;
                win.input_fresh += fresh;
                win.cache_write += write;
                win.cache_read += read;
                win.output += g("output_tokens");
                win.reread_cortex += res_cortex / 4;
                win.reread_bash += res_bash / 4;
                if !first_call_seen {
                    first_call_seen = true;
                    first_calls[slot].push(fresh + write + read);
                }
            } else if let Some(blocks) = content {
                for b in blocks {
                    if b.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let src = b
                        .get("tool_use_id")
                        .and_then(|x| x.as_str())
                        .and_then(|id| tool_names.get(id).copied())
                        .unwrap_or(Source::Other);
                    let n = b.get("content").map(text_len).unwrap_or(0);
                    match src {
                        Source::Cortex => {
                            win.injected_cortex += n / 4;
                            res_cortex += n;
                        }
                        Source::Structure => win.injected_structure += n / 4,
                        Source::Bash => {
                            win.injected_bash += n / 4;
                            res_bash += n;
                        }
                        Source::Other => {}
                    }
                }
            }
        }
    }
    for (slot, win) in [(0usize, &mut ledger.previous), (1, &mut ledger.current)] {
        let v = &mut first_calls[slot];
        v.sort_unstable();
        win.first_call_context_median = v.get(v.len() / 2).copied().unwrap_or(0);
    }
    Ok(ledger)
}

// ── Formatting ────────────────────────────────────────────────────────────────

/// Trend marker comparing current vs previous. `higher_is_better` flips polarity.
fn trend(current: f32, previous: f32, higher_is_better: bool) -> &'static str {
    let delta = current - previous;
    if delta.abs() < 0.005 {
        return "→";
    }
    let improving = (delta > 0.0) == higher_is_better;
    if improving { "↑ improving" } else { "↓ regressing" }
}

/// A rate with its denominator, so a percentage over three events cannot pass
/// for one over three hundred.
fn pct(num: i64, den: i64) -> String {
    if den == 0 {
        "n/a".to_string()
    } else {
        format!("{:.0}% ({num}/{den})", 100.0 * num as f32 / den as f32)
    }
}

fn ratio(num: i64, den: i64) -> f32 {
    if den == 0 { 0.0 } else { num as f32 / den as f32 }
}

/// Tokens, human-sized.
fn tok(n: i64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.2}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.0}k", f / 1e3)
    } else {
        n.to_string()
    }
}

pub fn format_text(sb: &Scoreboard) -> String {
    let (c, p) = (&sb.current, &sb.previous);
    let mut o = format!(
        "SELF-LEARNING SCOREBOARD v2 ({}d window vs previous {}d)\n\
         Every number is OBSERVED (hooks, no one chooses when), DELIVERED (reached an\n\
         agent's context), or SELF-REPORTED (closeout; biased, shown for reference).\n\n",
        sb.window_days, sb.window_days
    );

    o.push_str(&format!(
        "  Sessions worked:        {} (prev {})   closed out: {}\n",
        c.sessions_worked,
        p.sessions_worked,
        pct(c.sessions_closed, c.sessions_worked)
    ));

    o.push_str("\n  OUTCOMES (observed)\n");
    o.push_str(&format!(
        "    Build/test runs green: {}  (prev {})  {}\n",
        pct(c.runs_passed, c.runs),
        pct(p.runs_passed, p.runs),
        trend(c.run_pass_rate, p.run_pass_rate, true)
    ));
    o.push_str(&format!(
        "    Sessions ending green: {}  (prev {})\n",
        pct(c.sessions_ended_green, c.sessions_with_runs),
        pct(p.sessions_ended_green, p.sessions_with_runs)
    ));
    o.push_str(&format!(
        "    Self-reported closeouts: {} pass / {} fail (only run after success; not a pass rate)\n",
        c.closeout_pass, c.closeout_fail
    ));

    o.push_str("\n  REPEAT FAILURES (observed) — is the store preventing anything?\n");
    // No trend arrow here, deliberately. A failure is counted in the window
    // where it was LAST seen, so the previous window loses every failure that
    // later came back: its figure is a floor, and an arrow drawn against a
    // floor points at "regressing" by construction.
    o.push_str(&format!(
        "    Distinct failures: {}   came back from before this window: {}\n",
        c.failures_distinct,
        pct(c.failures_returned, c.failures_distinct),
    ));
    o.push_str(&format!(
        "    Came back with nothing recorded: {}   hit again after being recorded: {}\n",
        c.returned_unrecorded, c.hit_after_recorded
    ));

    o.push_str("\n  DELIVERY — what reached agents\n");
    o.push_str(&format!(
        "    Targeted lookups: {} (prev {})   identical repeats not counted: {}{}\n",
        c.lookups_targeted,
        p.lookups_targeted,
        c.lookups_repeated,
        if c.lookups_repeated >= 10 { "  ! a caller is looping" } else { "" }
    ));
    o.push_str(&format!("    Bulk listings: {} (prev {})\n", c.lookups_bulk, p.lookups_bulk));
    let mech = if c.pushes_by_mechanism.is_empty() {
        String::new()
    } else {
        format!(
            "  [{}]",
            c.pushes_by_mechanism.iter().map(|(m, n)| format!("{m} {n}")).collect::<Vec<_>>().join(", ")
        )
    };
    o.push_str(&format!(
        "    Pushes delivered (hook additionalContext): {} (prev {}){mech}\n",
        c.pushes_delivered, p.pushes_delivered
    ));
    if c.guard_fires_undelivered > 0 {
        o.push_str(&format!(
            "    ! Edit-guard fires never delivered: {} (plain hook stdout is not shown to the model)\n",
            c.guard_fires_undelivered
        ));
    }
    o.push_str(&format!(
        "    Lookup misses: {} distinct queries{}\n",
        c.gap_queries,
        if c.gap_loops > 0 { format!(", {} missed 10+ times (a loop, not a gap)", c.gap_loops) } else { String::new() }
    ));

    o.push_str("\n  CAPTURE\n");
    o.push_str(&format!(
        "    Markers committed: {} (prev {})   per worked session: {:.1}\n",
        c.markers_captured,
        p.markers_captured,
        ratio(c.markers_captured, c.sessions_worked)
    ));

    o.push_str(&format!(
        "\n  STORE (live entries only): {} patterns ({} retired), {} anti-patterns ({} retired)\n\
         \x20   Usage signal on {} of live patterns   unsettled challenges: {}\n\
         \x20   Skill pipeline: {} candidate → {} drafted → {} approved\n",
        sb.patterns_live,
        sb.patterns_retired,
        sb.anti_patterns_live,
        sb.anti_patterns_retired,
        pct(sb.patterns_with_usage, sb.patterns_live),
        sb.challenges_unsettled,
        sb.skills_candidate,
        sb.skills_drafted,
        sb.skills_approved
    ));

    if let Some(t) = &sb.tokens {
        o.push_str(&format_tokens(t));
    }
    o
}

fn format_tokens(t: &TokenLedger) -> String {
    let (c, p) = (&t.current, &t.previous);
    let input = |w: &TokenWindow| w.input_fresh + w.cache_write + w.cache_read;
    let avg = |w: &TokenWindow| if w.api_calls > 0 { input(w) / w.api_calls } else { 0 };
    let mut o = format!(
        "\n  TOKEN BILL (observed: Claude Code transcripts, {} file(s))\n",
        t.files_scanned
    );
    o.push_str(&format!(
        "    API calls: {} (prev {})   avg context per call: {} (prev {})   compactions: {}\n",
        c.api_calls,
        p.api_calls,
        tok(avg(c)),
        tok(avg(p)),
        c.compactions
    ));
    o.push_str(&format!(
        "    Input: cache reads {} ({:.1}%), cache writes {}, fresh {}   Output: {}\n",
        tok(c.cache_read),
        100.0 * c.cache_read as f64 / input(c).max(1) as f64,
        tok(c.cache_write),
        tok(c.input_fresh),
        tok(c.output)
    ));
    o.push_str(&format!(
        "    Fixed prompt at session start (median): {}\n",
        tok(c.first_call_context_median)
    ));
    o.push_str(&format!(
        "    Tool results injected: cortex {}, quartz-ctx/graphify {}, Bash {}\n",
        tok(c.injected_cortex),
        tok(c.injected_structure),
        tok(c.injected_bash)
    ));
    o.push_str(&format!(
        "    Re-read until compaction: cortex {} ({:.1}% of cache reads), Bash {} ({:.1}%)\n",
        tok(c.reread_cortex),
        100.0 * c.reread_cortex as f64 / c.cache_read.max(1) as f64,
        tok(c.reread_bash),
        100.0 * c.reread_bash as f64 / c.cache_read.max(1) as f64
    ));
    o.push_str(
        "    Cost is context size × calls: what sits in context is re-read on every later call.\n",
    );
    o.push_str(&format!(
        "    Host-confirmed cortex context deliveries: {} (prev {})   cortex hook errors: {} (prev {})\n",
        c.cortex_contexts_delivered, p.cortex_contexts_delivered, c.cortex_hook_errors, p.cortex_hook_errors
    ));
    o
}

/// One-line scoreboard for get_session_health. Store-only (no transcripts).
pub fn compact_line(store: &Store) -> String {
    match compute(store, 14) {
        Ok(sb) => {
            let (c, p) = (&sb.current, &sb.previous);
            let mut line = format!(
                "Scoreboard (14d): runs green {} {} | sessions ending green {} | failures that came back {} ({} unrecorded, {} after being recorded) | targeted lookups {} | pushes delivered {} | closed out {}",
                pct(c.runs_passed, c.runs),
                trend(c.run_pass_rate, p.run_pass_rate, true),
                pct(c.sessions_ended_green, c.sessions_with_runs),
                pct(c.failures_returned, c.failures_distinct),
                c.returned_unrecorded,
                c.hit_after_recorded,
                c.lookups_targeted,
                c.pushes_delivered,
                pct(c.sessions_closed, c.sessions_worked),
            );
            if c.lookups_repeated >= 10 {
                line.push_str(&format!(" | ! {} identical repeat lookups (a loop)", c.lookups_repeated));
            }
            line
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
        assert_eq!(sb.current.sessions_worked, 0);
        assert_eq!(sb.current.run_pass_rate, 0.0);
        assert_eq!(sb.patterns_live, 0);
        assert!(format_text(&sb).contains("n/a"), "no runs must read n/a, never 0% or 100%");
    }

    #[test]
    fn the_pass_rate_comes_from_observed_runs_not_closeouts() {
        let store = test_store("observed_rate");
        // Closeouts say everything passed -- they only run after success.
        for i in 0..3 {
            store.log_outcome(&format!("s{i}"), "build_pass", None, None).unwrap();
        }
        // The hook saw three greens and one red.
        for (s, ok) in [("a", true), ("a", false), ("b", true), ("b", true)] {
            crate::test_signal::observe(&store, s, "cargo test", ok).unwrap();
        }
        let sb = compute(&store, 14).unwrap();
        assert_eq!((sb.current.runs, sb.current.runs_passed), (4, 3));
        assert_eq!((sb.current.sessions_with_runs, sb.current.sessions_ended_green), (2, 1));
        assert_eq!(sb.current.closeout_pass, 3, "kept, and labelled self-reported");
    }

    #[test]
    fn identical_lookups_in_one_burst_count_once() {
        let store = test_store("dedupe_lookups");
        let now = Utc::now().to_rfc3339();
        for _ in 0..36 {
            store.conn().execute(
                "INSERT INTO mcp_calls (tool, args, called_at, logical_session_key)
                 VALUES ('get_context', '{\"hint\":\"shadow map\"}', ?1, 'loop')",
                rusqlite::params![now],
            ).unwrap();
        }
        store.conn().execute(
            "INSERT INTO mcp_calls (tool, args, called_at, logical_session_key)
             VALUES ('recall', '{\"topic\":\"ssr\"}', ?1, 'loop')",
            rusqlite::params![now],
        ).unwrap();
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.lookups_targeted, 2);
        assert_eq!(sb.current.lookups_repeated, 35);
        assert!(format_text(&sb).contains("looping"));
    }

    #[test]
    fn retired_entries_do_not_pad_the_store() {
        let store = test_store("live_only");
        let a = crate::crystallizer::add_anti_pattern(&store, "old trap", "w", "c", vec![]).unwrap();
        let b = crate::crystallizer::add_anti_pattern(&store, "new trap", "w2", "c2", vec![]).unwrap();
        store.supersede("anti_patterns", a, b).unwrap();
        let sb = compute(&store, 14).unwrap();
        assert_eq!((sb.anti_patterns_live, sb.anti_patterns_retired), (1, 1));
    }

    #[test]
    fn a_failure_hit_after_its_trap_was_recorded_is_counted() {
        let store = test_store("hit_after");
        let out = "error[E0599]: no method named `set_glow` found\n --> src/a.rs:1:1";
        crate::test_signal::note_failure(&store, "s1", "cargo build", out).unwrap();
        crate::test_signal::note_failure(&store, "s2", "cargo build", out).unwrap();
        let sig = crate::test_signal::error_signature(out).unwrap();
        // First seen three weeks ago, recorded an hour ago, seen again now.
        store.conn().execute(
            "UPDATE recurring_errors SET proposed = 1, handled_at = unixepoch() - 3600,
                                         first_seen_at = unixepoch() - 21 * 86400
             WHERE signature = ?1",
            rusqlite::params![sig],
        ).unwrap();
        crate::test_signal::note_failure(&store, "s3", "cargo build", out).unwrap();
        // A brand-new failure in this window is not a return.
        crate::test_signal::note_failure(&store, "s3", "cargo build",
            "error[E0599]: no method named `brand_new_thing` found\n --> src/b.rs:1:1").unwrap();
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.failures_distinct, 2);
        assert_eq!(sb.current.failures_returned, 1);
        assert_eq!(sb.current.returned_unrecorded, 0);
        assert_eq!(sb.current.hit_after_recorded, 1);
    }

    #[test]
    fn an_undelivered_guard_fire_is_called_out() {
        let store = test_store("undelivered");
        store.record_edit_guard_fire("s1", 7, "a.rs").unwrap();
        store.record_edit_guard_fire("s1", 8, "b.rs").unwrap();
        store.record_push("s1", "edit_guard", "8", Some(8), 120).unwrap();
        let sb = compute(&store, 14).unwrap();
        assert_eq!(sb.current.guard_fires_undelivered, 1);
        assert_eq!(sb.current.pushes_delivered, 1);
    }

    #[test]
    fn the_token_ledger_reads_usage_and_attributes_rereads() {
        let dir = std::env::temp_dir().join(format!("cortex_ledger_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let lines = [
            // call 1 issues a cortex lookup
            format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"m1","content":[{{"type":"tool_use","id":"t1","name":"mcp__cortex__recall","input":{{}}}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":0,"output_tokens":5}}}}}}"#),
            // its 400-char result
            format!(r#"{{"type":"user","timestamp":"{ts}","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"{}"}}]}}}}"#, "x".repeat(400)),
            // two later calls each re-read it
            format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"m2","content":[],"usage":{{"input_tokens":0,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":5}}}}}}"#),
            format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"m2","content":[],"usage":{{"input_tokens":0,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":5}}}}}}"#),
            format!(r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{ts}"}}"#),
            format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"m3","content":[],"usage":{{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":500,"output_tokens":5}}}}}}"#),
            // What the host did with hook output: one delivered, one failed,
            // and someone else's delivered context that must not be counted.
            format!(r#"{{"type":"attachment","timestamp":"{ts}","attachment":{{"type":"hook_additional_context","content":["[cortex] a.rs touches a recorded trap #1"],"hookName":"PostToolUse:Edit"}}}}"#),
            format!(r#"{{"type":"attachment","timestamp":"{ts}","attachment":{{"type":"hook_non_blocking_error","stderr":"MCP server 'cortex' not connected","hookName":"PostToolUse:Bash"}}}}"#),
            format!(r#"{{"type":"attachment","timestamp":"{ts}","attachment":{{"type":"hook_additional_context","content":["No preview server is running."],"hookName":"PostToolUse:Edit"}}}}"#),
        ];
        std::fs::write(dir.join("s.jsonl"), lines.join("\n")).unwrap();
        let l = token_ledger(&dir, 14).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let c = &l.current;
        assert_eq!(c.api_calls, 3, "a response split over records counts once");
        assert_eq!(c.cache_read, 1500);
        assert_eq!(c.injected_cortex, 100);
        assert_eq!(c.reread_cortex, 100, "re-read by the one later call before compaction");
        assert_eq!(c.compactions, 1);
        assert_eq!(c.first_call_context_median, 1010);
        assert_eq!(c.cortex_contexts_delivered, 1, "only cortex's own delivered context counts");
        assert_eq!(c.cortex_hook_errors, 1);
    }

    #[test]
    fn trend_polarity() {
        assert_eq!(trend(0.8, 0.5, true), "↑ improving");
        assert_eq!(trend(0.3, 0.5, true), "↓ regressing");
        assert_eq!(trend(0.3, 0.5, false), "↑ improving");
        assert_eq!(trend(0.5, 0.5, false), "→");
    }
}
