//! Scoring knowledge by whether the build went green, using output we already see.
//!
//! Survival used to be decided at closeout: the agent had to call
//! `closeout_session` and name an outcome. Measured on this workspace, fifteen
//! sessions did real work and never closed — so their retrievals produced no
//! signal at all, and 82% of patterns carried a 100% survival rate computed
//! from nothing.
//!
//! Meanwhile the compaction hook receives stdout AND stderr for every Bash call,
//! reads them, and records how many characters it saved. In one session slice
//! that was twenty-four build and test runs whose results were thrown away. The
//! evidence was already arriving; nothing consumed it.
//!
//! Nothing here asks the workflow to change. A test run is a test run.

use anyhow::Result;
use rusqlite::params;

use crate::memory::Store;

/// Did this command produce a verdict worth scoring, and what was it?
///
/// `None` means "not a build or test, or it said nothing decisive" — which is
/// the common case and must stay cheap and silent.
pub fn classify(command: &str, output: &str) -> Option<bool> {
    if !is_build_or_test(command) {
        return None;
    }
    // Failure wins over success. A run that prints "test result: ok" for one
    // crate and "error[E0432]" for the next has failed, and reading only the
    // first line would score it green.
    if looks_failed(output) {
        return Some(false);
    }
    if looks_passed(output) {
        return Some(true);
    }
    None
}

/// Commands whose output carries a pass/fail verdict.
///
/// Matched on the whole string rather than the first token, because our commands
/// routinely arrive as `cd <path> && cargo test` or with an env prefix.
fn is_build_or_test(command: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "cargo test", "cargo check", "cargo build", "cargo clippy",
        "npm test", "npm run test", "npm run build", "vitest", "jest",
        "pytest", "go test", "gradlew", "dotnet test",
    ];
    let c = command.to_lowercase();
    NEEDLES.iter().any(|n| c.contains(n))
}

fn looks_failed(out: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "test result: FAILED",
        "error[E",
        "error: could not compile",
        "error: test failed",
        "FAILED (",
        "panicked at",
        "Tests  0 passed",
        "failed |",          // vitest: "Tests  2 failed | 189 passed"
        "BUILD FAILED",
        "FAIL  ",            // vitest per-file failure banner
    ];
    NEEDLES.iter().any(|n| out.contains(n))
}

fn looks_passed(out: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "test result: ok",
        "Finished `dev`",
        "Finished `release`",
        "Finished `test`",
        "Test Files  ",      // vitest summary, only reached when nothing failed
        "ok. ",
        "BUILD SUCCESSFUL",
    ];
    NEEDLES.iter().any(|n| out.contains(n))
}

/// A stable identity for a failure, so the same one can be recognised again.
///
/// Deliberately coarse. The point is not to describe a failure precisely, it is
/// to tell whether THIS failure is one we keep having — so anything that varies
/// between occurrences (paths, line numbers, counts, durations) is stripped.
///
/// Returns `None` for output that failed without a recognisable signature; a
/// failure we cannot name is one we cannot count, and guessing would merge
/// unrelated failures into one bogus "recurring" trap.
pub fn error_signature(output: &str) -> Option<String> {
    // A Rust error code is the best identity available: stable, specific, and
    // already a shared vocabulary.
    if let Some(i) = output.find("error[E") {
        let code: String =
            output[i + 6..].chars().take_while(|c| c.is_alphanumeric()).collect();
        if !code.is_empty() {
            return Some(format!("rust:{code}"));
        }
    }
    // A failing assertion: keep the message, drop the location and the numbers.
    if let Some(i) = output.find("panicked at ") {
        let tail = &output[i + 12..];
        let msg = tail.lines().nth(1).unwrap_or("").trim();
        if !msg.is_empty() {
            let norm: String = msg
                .chars()
                .filter(|c| !c.is_ascii_digit())
                .take(80)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if norm.len() > 12 {
                return Some(format!("assert:{norm}"));
            }
        }
    }
    None
}

/// Record an outcome and bring the session's score into line with it.
///
/// Returns the number of patterns whose counters moved, for logging.
pub fn observe(store: &Store, session_id: &str, command: &str, passed: bool) -> Result<usize> {
    store.conn().execute(
        "INSERT INTO test_outcomes (session_id, command, passed) VALUES (?1, ?2, ?3)",
        params![session_id, command, passed as i64],
    )?;
    apply_verdict(store, session_id, passed)
}

/// Count a failure so a RECURRING one can be told from a one-off.
///
/// This is the filter that makes automatic capture worth having. Most failures
/// are a typo, a missing import, a name I got wrong — real, fixed in seconds,
/// and worthless as knowledge. Recording them all would bury the store.
///
/// A failure that keeps coming back is a different animal. It does not need
/// judging, only counting: noise does not repeat, traps do. So nothing is
/// proposed on a first sighting, and the threshold is crossed only by failures
/// that survived being fixed at least twice.
pub fn note_failure(store: &Store, session_id: &str, command: &str, output: &str) -> Result<()> {
    let Some(sig) = error_signature(output) else { return Ok(()) };
    // One count per session per signature: hitting the same compile error four
    // times while iterating on one fix is one occurrence, not four.
    store.conn().execute(
        "INSERT INTO recurring_errors (signature, sample, command, sessions, seen_count,
                                       first_seen_at, last_seen_at)
         VALUES (?1, ?2, ?3, json_array(?4), 1, unixepoch(), unixepoch())
         ON CONFLICT(signature) DO UPDATE SET
             seen_count   = seen_count + (CASE WHEN instr(sessions, ?4) = 0 THEN 1 ELSE 0 END),
             sessions     = CASE WHEN instr(sessions, ?4) = 0
                                 THEN json_insert(sessions, '$[#]', ?4) ELSE sessions END,
             last_seen_at = unixepoch()",
        params![sig, first_lines(output, 4), command, session_id],
    )?;
    Ok(())
}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().filter(|l| !l.trim().is_empty()).take(n).collect::<Vec<_>>().join("\n")
}

/// Failures seen in at least `min` distinct sessions — worth a human deciding
/// whether they are a trap.
pub fn recurring(store: &Store, min: i64) -> Result<Vec<(String, i64, String)>> {
    let mut stmt = store.conn().prepare(
        "SELECT signature, seen_count, sample FROM recurring_errors
         WHERE seen_count >= ?1 AND proposed = 0
         ORDER BY seen_count DESC",
    )?;
    let rows = stmt.query_map(params![min], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Make the stored counters reflect this session's CURRENT verdict.
///
/// The hard requirement is that twenty-four test runs must not credit a pattern
/// twenty-four times — that would recreate the vacuous signal the targeted-hint
/// design exists to avoid, just with a different inflator. So:
///
///   - the first decisive outcome credits each targeted pattern once
///   - a later change of verdict adjusts ONLY the reverted count, up or down
///   - an unchanged verdict does nothing at all
///
/// The result converges: whatever state the session ends in is what stands, and
/// it needs no closeout to get there.
fn apply_verdict(store: &Store, session_id: &str, passed: bool) -> Result<usize> {
    let previous: Option<(i64, String)> = store
        .conn()
        .query_row(
            "SELECT passed, scored_ids FROM session_verdict WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    // Only TARGETED retrievals count. Crediting a bulk listing would score every
    // row on every call, which is precisely the signal this replaces.
    let pattern_ids: Vec<i64> = {
        let mut stmt = store.conn().prepare(
            "SELECT DISTINCT entry_id FROM session_retrieval_log
             WHERE session_id = ?1 AND entry_table = 'patterns'
               AND tool_name IN ('recall', 'get_context', 'list_patterns_hint')
             LIMIT 12",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if pattern_ids.is_empty() {
        // Nothing was consulted, so nothing is owed credit either way. Still
        // record the verdict so a later change is measured against something.
        store.conn().execute(
            "INSERT INTO session_verdict (session_id, passed, scored_ids) VALUES (?1, ?2, '[]')
             ON CONFLICT(session_id) DO UPDATE SET passed = excluded.passed,
                                                   updated_at = unixepoch()",
            params![session_id, passed as i64],
        )?;
        return Ok(0);
    }

    let mut moved = 0usize;
    match previous {
        None => {
            // First verdict for this session: one use each, plus a revert if red.
            for id in &pattern_ids {
                let n = store.conn().execute(
                    "UPDATE patterns
                     SET use_count = use_count + 1,
                         reverted_count = reverted_count + ?1
                     WHERE id = ?2",
                    params![if passed { 0 } else { 1 }, id],
                )?;
                if n > 0 {
                    let _ = store.recompute_pattern_survival(*id);
                    moved += 1;
                }
            }
        }
        Some((was, ref scored_json)) => {
            let was_passed = was != 0;
            if was_passed == passed {
                return Ok(0); // nothing changed; do not touch the counters
            }
            // Verdict flipped. Adjust only the ids already credited, so a pattern
            // retrieved after the first scoring is not retroactively blamed.
            let already: Vec<i64> = serde_json::from_str(scored_json).unwrap_or_default();
            let delta: i64 = if passed { -1 } else { 1 };
            for id in &already {
                let n = store.conn().execute(
                    "UPDATE patterns
                     SET reverted_count = MAX(0, reverted_count + ?1)
                     WHERE id = ?2",
                    params![delta, id],
                )?;
                if n > 0 {
                    let _ = store.recompute_pattern_survival(*id);
                    moved += 1;
                }
            }
        }
    }

    let ids_json = serde_json::to_string(&pattern_ids).unwrap_or_else(|_| "[]".into());
    store.conn().execute(
        "INSERT INTO session_verdict (session_id, passed, scored_ids) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET passed     = excluded.passed,
                                               scored_ids = excluded.scored_ids,
                                               updated_at = unixepoch()",
        params![session_id, passed as i64, ids_json],
    )?;
    Ok(moved)
}

/// Has this session already been scored from a test result?
///
/// Closeout asks before applying its own outcome, so the two paths cannot both
/// credit the same session.
pub fn already_scored(store: &Store, session_id: &str) -> bool {
    store
        .conn()
        .query_row(
            "SELECT 1 FROM session_verdict WHERE session_id = ?1",
            params![session_id],
            |_| Ok(()),
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_build_command_is_not_a_verdict() {
        assert_eq!(classify("grep -rn foo src/", "whatever"), None);
        assert_eq!(classify("git status", "nothing to commit"), None);
    }

    #[test]
    fn our_actual_command_shapes_are_recognised() {
        // These arrive with a cd prefix and a separator in real use.
        assert_eq!(
            classify("cd \"/c/x/frontend\" && npm test", "Test Files  15 passed"),
            Some(true)
        );
        assert_eq!(classify("cargo test --lib", "test result: ok. 149 passed"), Some(true));
        assert_eq!(
            classify(
                "cargo check --lib --target aarch64-linux-android",
                "    Finished `dev` profile"
            ),
            Some(true)
        );
    }

    #[test]
    fn a_failure_anywhere_beats_a_success_elsewhere() {
        // The exact shape of a multi-crate run: one crate green, the next red.
        let mixed = "test result: ok. 12 passed\nerror[E0432]: unresolved import";
        assert_eq!(classify("cargo test", mixed), Some(false));
        assert_eq!(
            classify("npm test", "Test Files  1 failed | 14 passed\nTests  2 failed |"),
            Some(false)
        );
    }

    #[test]
    fn silence_when_the_output_says_nothing_decisive() {
        assert_eq!(classify("cargo build", "Compiling cortex v0.1.0"), None);
    }

    // ── scoring against a real store ─────────────────────────────────────────

    fn live_store() -> Option<Store> {
        let db = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .join(".cortex")
            .join("memory.db");
        if !db.exists() {
            return None;
        }
        Store::open(&db).ok()
    }

    /// Set up a session that consulted one pattern, and return (store, session, id).
    fn seeded(tag: &str) -> Option<(Store, String, i64)> {
        let store = live_store()?;
        let id: i64 = store
            .conn()
            .query_row("SELECT id FROM patterns LIMIT 1", [], |r| r.get(0))
            .ok()?;
        let session = format!("test_signal_{tag}_{}", std::process::id());
        store
            .conn()
            .execute(
                "INSERT INTO session_retrieval_log (session_id, entry_table, entry_id, tool_name)
                 VALUES (?1, 'patterns', ?2, 'list_patterns_hint')",
                params![session, id],
            )
            .ok()?;
        Some((store, session, id))
    }

    fn counters(store: &Store, id: i64) -> (i64, i64) {
        store
            .conn()
            .query_row(
                "SELECT use_count, reverted_count FROM patterns WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn cleanup(store: &Store, session: &str, id: i64, before: (i64, i64)) {
        let _ = store.conn().execute(
            "UPDATE patterns SET use_count = ?1, reverted_count = ?2 WHERE id = ?3",
            params![before.0, before.1, id],
        );
        let _ = store.recompute_pattern_survival(id);
        for t in ["session_retrieval_log", "test_outcomes", "session_verdict"] {
            let _ = store
                .conn()
                .execute(&format!("DELETE FROM {t} WHERE session_id = ?1"), params![session]);
        }
    }

    /// The requirement that makes this safe to run on every command.
    #[test]
    fn twenty_four_green_runs_credit_a_pattern_exactly_once() {
        let Some((store, session, id)) = seeded("repeat") else { return };
        let before = counters(&store, id);
        for _ in 0..24 {
            observe(&store, &session, "cargo test", true).unwrap();
        }
        let after = counters(&store, id);
        assert_eq!(after.0, before.0 + 1, "use_count must move once, not per run");
        assert_eq!(after.1, before.1, "a green session reverts nothing");
        cleanup(&store, &session, id, before);
    }

    /// A session that goes green then red must have its credit taken back.
    #[test]
    fn a_later_failure_revokes_the_earlier_credit() {
        let Some((store, session, id)) = seeded("flip") else { return };
        let before = counters(&store, id);

        observe(&store, &session, "cargo test", true).unwrap();
        let green = counters(&store, id);
        assert_eq!(green.1, before.1);

        observe(&store, &session, "cargo test", false).unwrap();
        let red = counters(&store, id);
        assert_eq!(red.0, before.0 + 1, "still one use, not two");
        assert_eq!(red.1, before.1 + 1, "the failure has to land");

        // ...and fixing it takes the revert back off again.
        observe(&store, &session, "cargo test", true).unwrap();
        let fixed = counters(&store, id);
        assert_eq!(fixed.1, before.1, "a fixed session should not stay blamed");
        cleanup(&store, &session, id, before);
    }

    // ── recurring-failure detection ──────────────────────────────────────────

    #[test]
    fn the_same_error_code_gets_the_same_signature() {
        let a = "error[E0432]: unresolved import `foo::Bar`\n --> src/a.rs:12:5";
        let b = "error[E0432]: unresolved import `baz::Qux`\n --> src/zzz.rs:99:1";
        assert_eq!(error_signature(a), error_signature(b), "paths must not split the identity");
        assert_eq!(error_signature(a), Some("rust:E0432".into()));
    }

    #[test]
    fn different_failures_do_not_collide() {
        assert_ne!(
            error_signature("error[E0432]: unresolved import"),
            error_signature("error[E0308]: mismatched types"),
        );
    }

    #[test]
    fn an_assertion_keeps_its_message_and_drops_its_numbers() {
        let a = "panicked at src/x.rs:12:9:\nexpected 0.278 to be less than 0.001\n";
        let b = "panicked at src/y.rs:88:1:\nexpected 0.384 to be less than 0.005\n";
        assert_eq!(error_signature(a), error_signature(b), "the numbers vary, the trap does not");
        assert!(error_signature(a).unwrap().starts_with("assert:"));
    }

    #[test]
    fn a_failure_we_cannot_name_is_not_counted() {
        // Guessing here would merge unrelated failures into one bogus "recurring"
        // trap, which is worse than missing it.
        assert_eq!(error_signature("something went wrong"), None);
        assert_eq!(error_signature(""), None);
    }

    #[test]
    fn hitting_one_error_repeatedly_in_a_session_counts_once() {
        let Some(store) = live_store() else { return };
        let session = format!("test_recur_{}", std::process::id());
        let sig_src = "error[E0999]: a signature used only by this test";
        for _ in 0..5 {
            note_failure(&store, &session, "cargo test", sig_src).unwrap();
        }
        let n: i64 = store
            .conn()
            .query_row(
                "SELECT seen_count FROM recurring_errors WHERE signature = 'rust:E0999'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "iterating on one fix is one occurrence, not five");
        let _ = store
            .conn()
            .execute("DELETE FROM recurring_errors WHERE signature = 'rust:E0999'", []);
    }

    #[test]
    fn scoring_marks_the_session_so_closeout_stands_down() {
        let Some((store, session, id)) = seeded("gate") else { return };
        let before = counters(&store, id);
        assert!(!already_scored(&store, &session));
        observe(&store, &session, "cargo test", true).unwrap();
        assert!(already_scored(&store, &session), "closeout must be able to see this");
        cleanup(&store, &session, id, before);
    }
}
