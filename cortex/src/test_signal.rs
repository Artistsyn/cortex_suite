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
/// Coarse about what VARIES, specific about what the failure IS.
///
/// Anything that differs between occurrences of one trap — absolute paths, line
/// numbers, counts, durations — is still stripped, because it was right to:
/// this store spans two machines and the same file is `src\\a.rs` on one and
/// `src/a.rs` on the other.
///
/// But an error CODE alone is not an identity, and keying on one was a real
/// defect rather than a conservative choice. `rust:E0425` accumulated thirteen
/// distinct symbols across four unrelated projects — a parser helper, a config
/// value, a renderer field — and reported them as a single failure "hit in 6
/// sessions". A reviewer reading that invents a mechanism connecting things
/// that have nothing in common but a compiler code. One did.
///
/// So the discriminator is the thing that is actually wrong:
///   - the first backticked identifier, when the message names one. Stable
///     across machines and files, which is why it beats the path.
///   - otherwise the normalised message plus the file's BASENAME — `E0308:
///     mismatched types` says nothing on its own, and the file at least keeps
///     one project's type errors out of another's bucket.
///
/// Returns `None` for output that failed without a recognisable signature; a
/// failure we cannot name is one we cannot count, and guessing would merge
/// unrelated failures into one bogus "recurring" trap.
pub fn error_signature(output: &str) -> Option<String> {
    if let Some(i) = output.find("error[E") {
        let code: String =
            output[i + 6..].chars().take_while(|c| c.is_alphanumeric()).collect();
        if !code.is_empty() {
            let msg_line = output[i..].lines().next().unwrap_or("");
            if let Some(sym) = backticked(msg_line) {
                return Some(format!("rust:{code}:{sym}"));
            }
            // No identifier in the message. Fall back to the message text plus
            // the file it came from.
            let msg = msg_line.split_once(": ").map(|(_, m)| m).unwrap_or(msg_line).trim();
            let norm = normalise(msg);
            return Some(match source_basename(output) {
                Some(f) if !norm.is_empty() => format!("rust:{code}:{norm}@{f}"),
                _ if !norm.is_empty() => format!("rust:{code}:{norm}"),
                Some(f) => format!("rust:{code}:@{f}"),
                None => format!("rust:{code}"),
            });
        }
    }
    // A failing assertion: the message, plus WHERE it fired.
    if let Some(i) = output.find("panicked at ") {
        let tail = &output[i + 12..];
        let msg = tail.lines().nth(1).unwrap_or("").trim();
        let norm = normalise(msg);

        // The discriminator, for the same reason the Rust branch has one. The
        // message alone gave `assert:assertion left == right failed`, which is
        // every `assert_eq!` in every project -- the generic half of the panic
        // is all that survives, because the values that made it specific are
        // digits and the digits are stripped.
        //
        // The failing test's name is the real identity. The panic site's file
        // is the fallback, and one of the two is always present in a panic, so
        // every signature this branch produces carries a discriminator -- which
        // is what lets the migration recognise an old one by shape alone.
        let disc = failing_test_name(output)
            .or_else(|| panic_site_basename(tail))
            .unwrap_or_else(|| "?".to_string());

        // The line after `panicked at` is USUALLY the assertion message and is
        // sometimes the harness's own summary, because the panic block can be
        // the last thing before cargo's tally. Keying on that produced
        // `assert:test result: FAILED. passed; failed; ignored; measured...`,
        // which matched every failing run in every project ever recorded.
        if !norm.is_empty() && norm.len() > 12 && !is_harness_chatter(&norm) {
            return Some(format!("assert:{norm}@{disc}"));
        }
        if disc != "?" {
            return Some(format!("assert:@{disc}"));
        }
    }
    None
}

/// The first `backticked` identifier in a line, if any.
fn backticked(line: &str) -> Option<String> {
    let start = line.find('`')? + 1;
    let rest = &line[start..];
    let end = rest.find('`')?;
    let sym = rest[..end].trim();
    (!sym.is_empty() && sym.len() <= 80).then(|| sym.to_string())
}

/// The file name a `--> path:line:col` pointer names, without its directories.
///
/// The BASENAME only: the same file is reached by different paths on different
/// machines, and splitting one trap in two because a checkout moved is the
/// failure mode the original coarse key existed to avoid.
fn source_basename(output: &str) -> Option<String> {
    let i = output.find("--> ")?;
    let path = output[i + 4..].lines().next()?.trim();
    let path = path.split(':').next()?;
    let name = path.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty() && name.len() <= 60).then(|| name.to_string())
}

/// Digits out, whitespace collapsed, truncated. Numbers vary; traps do not.
fn normalise(msg: &str) -> String {
    msg.chars()
        .filter(|c| !c.is_ascii_digit())
        .take(80)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Is this the test harness talking about itself rather than a failure?
fn is_harness_chatter(norm: &str) -> bool {
    const NOISE: [&str; 5] = [
        "test result:",
        "failures:",
        "running tests",
        "error: test failed",
        "note: run with",
    ];
    NOISE.iter().any(|n| norm.starts_with(n))
}

/// The file a panic fired in, from `panicked at <path>:line:col`.
fn panic_site_basename(tail: &str) -> Option<String> {
    let path = tail.lines().next()?.trim().split(':').next()?;
    let name = path.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty() && name.len() <= 60).then(|| name.to_string())
}

/// The name of the test whose output this is, from cargo's own banner.
fn failing_test_name(output: &str) -> Option<String> {
    let i = output.find("---- ")?;
    let rest = &output[i + 5..];
    let end = rest.find(" stdout")?;
    let name = rest[..end].trim();
    (!name.is_empty() && name.len() <= 120).then(|| name.to_string())
}

/// The part of the output that actually shows the failure.
///
/// NOT the first few lines of whatever was captured, which is what this used to
/// be. A captured block often opens with something else entirely — a file read,
/// a grep result, a build log — so the stored exemplar could be text that has
/// nothing to do with the error, and because the sample was never updated on
/// conflict it stayed wrong forever. That is how `rust:E0425` came to be
/// illustrated by a function name that never appeared in an E0425.
fn error_excerpt(output: &str) -> String {
    let from = output
        .find("error[E")
        .or_else(|| output.find("panicked at "))
        .or_else(|| output.find("error:"))
        .unwrap_or(0);
    output[from..]
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Record an outcome and bring the session's score into line with it.
///
/// Returns the number of patterns whose counters moved, for logging.
pub fn observe(store: &Store, session_id: &str, command: &str, passed: bool) -> Result<usize> {
    // Third command-capture surface, after mcp_calls.args and
    // compression_savings.command. Only the pass/fail verdict drives anything
    // here; the command text is context for a human reading the ledger, so it
    // has nothing to lose by being scrubbed of credentials.
    let command = crate::redact::redact_command(command);
    store.conn().execute(
        "INSERT INTO test_outcomes (session_id, command, passed) VALUES (?1, ?2, ?3)",
        params![session_id, command.as_str(), passed as i64],
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
             -- The exemplar REFRESHES. It used to be frozen at whatever the
             -- first sighting captured, so a bad one could never heal; with a
             -- signature this specific every member is a fair illustration of
             -- the group, and the newest is the one a reviewer can still find.
             sample       = excluded.sample,
             last_seen_at = unixepoch()",
        params![sig, error_excerpt(output), command, session_id],
    )?;
    Ok(())
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

    /// See `test_support::TempStore` -- one guard for the whole crate, so the
    /// directory goes even when a test panics.
    fn temp_store(tag: &str) -> Option<crate::test_support::TempStore> {
        crate::test_support::TempStore::new(tag).ok()
    }

    /// Set up a session that consulted one pattern, and return (store, session, id).
    ///
    /// The pattern is inserted here rather than borrowed from whatever happened
    /// to be row 1 of the real store. That is not only hygiene: these tests
    /// MUTATE `use_count` and `reverted_count` and put them back afterwards, so
    /// against a live store they were editing real survival telemetry and
    /// depending on their own cleanup to undo it.
    fn seeded(tag: &str) -> Option<(crate::test_support::TempStore, String, i64)> {
        let store = temp_store(tag)?;
        store
            .conn()
            .execute(
                "INSERT INTO patterns (name, intent, body, uses, tags, approved_at)
                 VALUES ('t', 't', 't', '[]', '[]', '2026-01-01')",
                [],
            )
            .ok()?;
        let id = store.conn().last_insert_rowid();
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
    fn one_error_reached_by_two_paths_keeps_one_identity() {
        // The original rule, still enforced: this store spans two machines and
        // the same file is reached by different paths on each. A checkout that
        // moved must not look like a new failure.
        let a = "error[E0432]: unresolved import `foo::Bar`\n --> src/a.rs:12:5";
        let b = "error[E0432]: unresolved import `foo::Bar`\n --> C:\\work\\src\\a.rs:99:1";
        assert_eq!(error_signature(a), error_signature(b), "paths must not split the identity");
        assert_eq!(error_signature(a), Some("rust:E0432:foo::Bar".into()));
    }

    #[test]
    fn two_unrelated_failures_sharing_a_code_are_not_one_recurring_trap() {
        // THE DEFECT THIS REPLACES. `rust:E0425` had collected thirteen
        // distinct symbols across four projects and was being reported as a
        // single failure "hit in 6 sessions" -- which reads as a trap and is
        // not one. An error code is a vocabulary, not an identity.
        let a = "error[E0425]: cannot find function `parse_header` in this scope\n --> src/a.rs:1:1";
        let b = "error[E0425]: cannot find value `retry_budget` in this scope\n --> src/b.rs:9:9";
        assert_ne!(error_signature(a), error_signature(b));
        assert_eq!(error_signature(a), Some("rust:E0425:parse_header".into()));
    }

    #[test]
    fn a_message_with_no_symbol_is_separated_by_its_file() {
        // `mismatched types` says nothing on its own -- thirty-five of them
        // were in one bucket. The basename at least keeps one project's type
        // errors out of another's.
        let a = "error[E0308]: mismatched types\n --> src/render/pass.rs:4:1";
        let b = "error[E0308]: mismatched types\n --> src/import/chunk.rs:7:2";
        assert_ne!(error_signature(a), error_signature(b));
        assert_eq!(error_signature(a), Some("rust:E0308:mismatched types@pass.rs".into()));
    }

    #[test]
    fn the_harness_talking_about_itself_is_not_an_assertion() {
        // `panicked at` is usually followed by the assertion message and is
        // sometimes followed by cargo's own tally, because the panic block can
        // be the last thing before it. That produced the signature
        // `assert:test result: FAILED. passed; failed; ignored; ...`, which
        // matches every failing run in every project.
        let out = "---- suite::a_named_case stdout ----\npanicked at src/x.rs:1:1:\n\
                   test result: FAILED. 0 passed; 1 failed; 0 ignored\n";
        let sig = error_signature(out).expect("a failure this shaped is still countable");
        assert!(!sig.contains("test result"), "harness chatter became the identity: {sig}");
        assert_eq!(sig, "assert:@suite::a_named_case");
    }

    #[test]
    fn two_different_assert_eq_failures_are_not_one_trap() {
        // `assertion \`left == right\` failed` is the generic half of every
        // assert_eq! anywhere -- the values that made it specific are digits,
        // and digits are stripped. Four of them were in one bucket.
        let a = "---- render::a_case stdout ----\npanicked at src/render.rs:4:1:\n\
                 assertion `left == right` failed\n";
        let b = "---- import::b_case stdout ----\npanicked at src/import.rs:9:2:\n\
                 assertion `left == right` failed\n";
        assert_ne!(error_signature(a), error_signature(b));
        assert_eq!(
            error_signature(a),
            Some("assert:assertion `left == right` failed@render::a_case".into()),
        );
    }

    #[test]
    fn an_assertion_outside_a_named_test_still_gets_an_identity() {
        // No cargo banner, so the panic site's file is the discriminator. Every
        // signature this branch emits carries one, which is what lets the
        // migration recognise a legacy row by its shape.
        let out = "panicked at src/solver.rs:31:5:\nindex out of bounds somewhere\n";
        let sig = error_signature(out).unwrap();
        assert!(sig.contains('@'), "no discriminator: {sig}");
        assert_eq!(sig, "assert:index out of bounds somewhere@solver.rs");
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
        // The numbers still go: they vary between occurrences of one trap.
        let a = "panicked at src/x.rs:12:9:\nexpected 0.278 to be less than 0.001\n";
        let b = "panicked at src/x.rs:88:1:\nexpected 0.384 to be less than 0.005\n";
        assert_eq!(error_signature(a), error_signature(b), "the numbers vary, the trap does not");
        assert!(error_signature(a).unwrap().starts_with("assert:"));
    }

    #[test]
    fn the_same_assertion_in_two_files_is_two_rows_now_and_that_is_the_safer_error() {
        // A DELIBERATE CHANGE. This used to be one identity -- the location was
        // stripped on the grounds that a trap is a trap wherever it fires, and
        // that reasoning is not wrong.
        //
        // But the same rule also merged `assertion \`left == right\` failed`
        // from four unrelated projects into a single "recurring failure", with
        // an exemplar drawn from none of them. Between the two errors:
        //
        //   splitting one real trap  -> two rows, both true, both actionable
        //   merging unrelated traps  -> one row that is a fiction
        //
        // The first is the error worth making. A reviewer seeing the same trap
        // twice loses a minute; a reviewer seeing a phantom one invents a
        // mechanism to explain it, which is exactly what happened.
        let a = "panicked at src/x.rs:12:9:\nexpected 0.278 to be less than 0.001\n";
        let b = "panicked at src/y.rs:88:1:\nexpected 0.384 to be less than 0.005\n";
        assert_ne!(error_signature(a), error_signature(b));
    }

    #[test]
    fn the_exemplar_shows_the_error_and_not_whatever_was_captured_first() {
        // THE HALF THAT ACTUALLY MISLED A REVIEWER. The sample used to be the
        // first four non-empty lines of the captured block, which often opens
        // with a file read or a grep result -- and it was never updated on
        // conflict, so a wrong exemplar was permanent. `rust:E0425` ended up
        // illustrated by a function name that never appeared in an E0425, and
        // the reviewer reading it invented a mechanism to explain it.
        let out = "pub fn some_unrelated_helper() -> u32 {\n                       0\n}\n\nerror[E0425]: cannot find value `budget` in this scope\n                    --> src/a.rs:3:9\n";
        let ex = error_excerpt(out);
        assert!(ex.starts_with("error[E0425]"), "the exemplar does not show the error: {ex:?}");
        assert!(!ex.contains("some_unrelated_helper"), "captured noise leaked in: {ex:?}");
    }

    #[test]
    fn a_later_sighting_heals_an_exemplar_already_stored_wrong() {
        // The refresh exists for the rows ALREADY IN THE STORE. They were
        // written by the previous version, whose sample was the first four
        // lines of whatever was captured -- so some of them illustrate their
        // signature with text that has nothing to do with it, and being frozen
        // on conflict they could never recover. A row seeded the old way must
        // come right the next time the failure is seen.
        //
        // Written by seeding the bad row directly, because the current code
        // cannot produce one: an earlier version of this test let both
        // sightings go through `note_failure` and so proved nothing -- removing
        // the refresh entirely left it green.
        let Some(store) = temp_store("heal") else { return };
        let sig = "rust:E0998:only_used_by_this_test";
        let err = "error[E0998]: cannot find value `only_used_by_this_test` in this scope";

        store
            .conn()
            .execute(
                "INSERT INTO recurring_errors (signature, sample, command, sessions, seen_count)
                 VALUES (?1, ?2, 'cargo test', json_array('old'), 1)",
                params![sig, "pub fn something_entirely_unrelated() -> u32 {"],
            )
            .unwrap();

        note_failure(&store, "s2", "cargo test", err).unwrap();

        let (sample, n): (String, i64) = store
            .conn()
            .query_row(
                "SELECT sample, seen_count FROM recurring_errors WHERE signature = ?1",
                params![sig],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            sample.starts_with("error[E0998]"),
            "the stale exemplar was not healed: {sample:?}",
        );
        assert!(
            !sample.contains("something_entirely_unrelated"),
            "the old exemplar is still there: {sample:?}",
        );
        assert_eq!(n, 2, "a second distinct session is a second occurrence");
    }

    #[test]
    fn the_migration_drops_old_rows_and_keeps_the_ones_a_human_judged() {
        // A row somebody already ruled on is theirs, however it was keyed.
        let Some(store) = temp_store("legacy") else { return };
        let rows = [
            ("rust:E0425", 0, "old scheme, untouched -> goes"),
            ("assert:test result: FAILED. passed; failed", 0, "harness chatter -> goes"),
            ("assert:assertion `left == right` failed", 0, "no discriminator -> goes"),
            ("rust:E0308", 1, "old scheme but already judged -> stays"),
            ("rust:E0425:parse_header", 0, "new scheme -> stays"),
            ("assert:index out of bounds@solver.rs", 0, "has a discriminator -> stays"),
        ];
        for (sig, proposed, _) in rows {
            store
                .conn()
                .execute(
                    "INSERT INTO recurring_errors (signature, sample, command, proposed)
                     VALUES (?1, 'x', 'cargo test', ?2)",
                    params![sig, proposed],
                )
                .unwrap();
        }

        let n = store.drop_legacy_failure_signatures().unwrap();
        assert_eq!(n, 3, "expected exactly the legacy-shaped, unjudged rows");

        let left: Vec<String> = store
            .conn()
            .prepare("SELECT signature FROM recurring_errors ORDER BY signature")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            left,
            vec![
                "assert:index out of bounds@solver.rs".to_string(),
                "rust:E0308".to_string(),
                "rust:E0425:parse_header".to_string(),
            ],
        );
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
        let Some(store) = temp_store("recur") else { return };
        let session = "s1";
        let sig_src = "error[E0999]: cannot find value `only_this_test` in this scope";
        for _ in 0..5 {
            note_failure(&store, session, "cargo test", sig_src).unwrap();
        }
        let n: i64 = store
            .conn()
            .query_row(
                "SELECT seen_count FROM recurring_errors WHERE signature = ?1",
                params!["rust:E0999:only_this_test"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "iterating on one fix is one occurrence, not five");
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
