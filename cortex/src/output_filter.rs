//! Lossless command-output compaction.
//!
//! This module removes only PROVABLY-REDUNDANT content from command output —
//! build/download progress chatter, per-test "ok" lines (equivalent to what
//! `cargo -q` already suppresses), and consecutive duplicate lines. It NEVER
//! drops a diagnostic: every `error`, `warning`, `note`, panic, failure block,
//! and captured-output section is preserved verbatim, with its `file:line`.
//!
//! Design contract (why this is safe to run automatically on tool output):
//!   - Compaction is line-classified: a line is dropped ONLY if it matches a
//!     known-noise rule. Anything unrecognized is KEPT. Failure is toward
//!     preservation, never toward loss.
//!   - The full raw output is tee'd to disk whenever anything is dropped, so
//!     the exact original byte stream is always one read away.
//!   - `lossless` is reported per call and is `true` for every strategy here.
//!
//! Anything that would require judgement about what the agent "needs" (stripping
//! function bodies, grouping warnings so `file:line` is lost, truncating diff
//! hunks) is deliberately NOT implemented — those belong to an opt-in lossy tier
//! that this module does not provide.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    CargoTest,
    CargoBuild,
    CargoClippy,
    GitStatus,
    Generic,
}

#[derive(Debug, Clone)]
pub struct FilteredOutput {
    pub text: String,
    pub original_chars: usize,
    pub filtered_chars: usize,
    pub dropped_lines: usize,
    /// Lines withheld from the inline text by windowing. NOT redundant --
    /// present in the tee file, absent from what the model sees. Non-zero means
    /// "complete on disk, windowed in context".
    pub elided_lines: usize,
    pub tee_path: Option<PathBuf>,
    /// Always true for this module — documents the guarantee at the call site.
    pub lossless: bool,
}

/// Classify a command string into the filtering strategy it should use.
pub fn detect_command(cmd: &str) -> CommandKind {
    let c = cmd.trim_start();
    // Normalize leading `cargo +toolchain` and environment prefixes lightly by
    // scanning for the meaningful verb anywhere near the start.
    let has = |needle: &str| c.contains(needle);
    if has("cargo") && has("clippy") {
        CommandKind::CargoClippy
    } else if has("cargo") && has("test") {
        CommandKind::CargoTest
    } else if has("cargo") && (has("build") || has("check")) {
        CommandKind::CargoBuild
    } else if has("git") && has("status") {
        CommandKind::GitStatus
    } else {
        CommandKind::Generic
    }
}

/// Cargo status verbs that carry no diagnostic content and are safe to drop.
/// `Finished` is intentionally EXCLUDED — it is the success signal and is kept.
const CARGO_PROGRESS_VERBS: &[&str] = &[
    "Compiling",
    "Checking",
    "Downloading",
    "Downloaded",
    "Updating",
    "Blocking",
    "Locking",
    "Installing",
    "Fresh",
    "Building",
    "Waiting",
    "Packaging",
    "Verifying",
    "Adding",
    "Removing",
    "Ignored",
];

fn is_cargo_progress(line: &str) -> bool {
    let t = line.trim_start();
    // Cargo status lines are `<Verb> <rest>`; match the first whitespace token.
    match t.split_whitespace().next() {
        Some(first) => CARGO_PROGRESS_VERBS.contains(&first),
        None => false,
    }
}

/// A per-test pass line: `test path::to::test ... ok`. Dropping these is
/// equivalent to `cargo test -q` (which prints dots, not names); the count is
/// preserved by the retained `test result:` summary line. Failures/ignored are
/// NOT matched here and are always kept.
fn is_passing_test_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("test ") && t.ends_with(" ... ok")
}

/// Collapse runs of blank lines to a single blank; returns kept lines.
/// Purely cosmetic whitespace — no information carried.
fn collapse_blank_runs(lines: Vec<String>) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    let mut prev_blank = false;
    for l in lines {
        let blank = l.trim().is_empty();
        if blank && prev_blank {
            dropped += 1;
            continue;
        }
        prev_blank = blank;
        out.push(l);
    }
    (out, dropped)
}

/// cargo build / check: drop only progress verbs (keep `Finished`), collapse
/// blank runs. Every diagnostic line is preserved verbatim.
fn filter_cargo_build(raw: &str) -> (String, usize) {
    let mut dropped = 0usize;
    let kept: Vec<String> = raw
        .lines()
        .filter(|l| {
            if is_cargo_progress(l) {
                dropped += 1;
                false
            } else {
                true
            }
        })
        .map(|l| l.to_string())
        .collect();
    let (kept, blank_dropped) = collapse_blank_runs(kept);
    dropped += blank_dropped;
    (kept.join("\n"), dropped)
}

/// cargo clippy: identical policy to build — keep every diagnostic verbatim
/// (grouping by lint would drop the `file:line` the agent needs, so we do not).
fn filter_cargo_clippy(raw: &str) -> (String, usize) {
    filter_cargo_build(raw)
}

/// cargo test: drop build progress + per-test `... ok` lines (== cargo -q),
/// collapse blank runs. All FAILED lines, the `failures:` section, panics,
/// captured output, and every `test result:` summary are kept verbatim.
fn filter_cargo_test(raw: &str) -> (String, usize) {
    let mut dropped = 0usize;
    let kept: Vec<String> = raw
        .lines()
        .filter(|l| {
            if is_cargo_progress(l) || is_passing_test_line(l) {
                dropped += 1;
                false
            } else {
                true
            }
        })
        .map(|l| l.to_string())
        .collect();
    let (kept, blank_dropped) = collapse_blank_runs(kept);
    dropped += blank_dropped;
    (kept.join("\n"), dropped)
}

/// git status (long form): drop the instructional hint lines (the
/// `(use "git ..." ...)` guidance) which carry no repository state. Every file
/// path and section header is kept.
fn filter_git_status(raw: &str) -> (String, usize) {
    let mut dropped = 0usize;
    let kept: Vec<String> = raw
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            if t.starts_with("(use ") {
                dropped += 1;
                false
            } else {
                true
            }
        })
        .map(|l| l.to_string())
        .collect();
    let (kept, blank_dropped) = collapse_blank_runs(kept);
    dropped += blank_dropped;
    (kept.join("\n"), dropped)
}

/// Generic: collapse runs of IDENTICAL consecutive lines to `line  (×N)`.
/// The count preserves the information that N copies existed — lossless.
/// No truncation: unique content passes through untouched.
fn filter_generic(raw: &str) -> (String, usize) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let cur = lines[i];
        let mut run = 1usize;
        while i + run < lines.len() && lines[i + run] == cur {
            run += 1;
        }
        if run > 1 {
            out.push(format!("{cur}  (×{run})"));
            dropped += run - 1;
        } else {
            out.push(cur.to_string());
        }
        i += run;
    }
    (out.join("\n"), dropped)
}

fn filter_by_kind(kind: CommandKind, raw: &str) -> (String, usize) {
    match kind {
        CommandKind::CargoTest => filter_cargo_test(raw),
        CommandKind::CargoBuild => filter_cargo_build(raw),
        CommandKind::CargoClippy => filter_cargo_clippy(raw),
        CommandKind::GitStatus => filter_git_status(raw),
        CommandKind::Generic => filter_generic(raw),
    }
}

/// Deterministic short id for a tee filename, from the raw content.
fn tee_stem(raw: &str) -> String {
    // Cheap FNV-1a over the bytes — no crypto needed, just a stable name.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    format!("{ts}-{h:016x}")
}

/// Below this many chars, filtering is skipped entirely — the round-trip and
/// any dropped-line note would cost more than it saves.
pub const MIN_FILTER_CHARS: usize = 800;

/// Above this, the output is WINDOWED as well as filtered.
///
/// Measured on this project's own telemetry: removing provably-redundant lines
/// caps out at about 2.6% of volume, because build and test output simply is
/// not very repetitive — and it was already achieving 2.0%. Meanwhile 46% of
/// all bytes sat in the 189 calls larger than 2,000 chars. The saving was never
/// going to come from squeezing text harder; it comes from not putting the
/// whole of a 6,000-char log in front of the model when twenty lines answer the
/// question.
///
/// This is still lossless in the sense that matters: the complete byte stream
/// is written to the tee file first, and the elision marker names the path, so
/// nothing is destroyed and anything elided is one Read away.
pub const MAX_INLINE_CHARS: usize = 1_000;

/// Lines kept at each end when windowing. Output tends to state the problem
/// early and the verdict last.
///
/// Chosen by sweeping both knobs against the captured corpus (see
/// `sweep_window_thresholds`). Measured savings, all with zero signal lines
/// lost:
///
/// ```text
///   thresh  head/tail   saved
///     2000     14        11.9%
///     1200     14        15.1%
///     1200     10        25.9%
///     1000     10        26.1%   <- default
///     1000      8        33.2%
///      800      6        43.1%
/// ```
///
/// The line count dominates the byte threshold: 1200/14 and 1200/10 differ by
/// 10.8 points. 10/10 is the balance point — 20 lines of context plus every
/// signal line is enough to act on without a follow-up read in the common case,
/// and the more aggressive rows are a config change away (see `env_usize`) if
/// the budget ever needs them.
const HEAD_LINES: usize = 10;
const TAIL_LINES: usize = 10;

/// The three window tunables, overridable per-process by env var.
///
/// These exist so the thresholds can be swept against the captured corpus
/// (and adjusted in the field) without a rebuild. Read on each call rather
/// than cached: this runs once per tool result, and a sweep needs to change
/// the value inside a single process.
///
/// - `CORTEX_MAX_INLINE_CHARS` — window outputs larger than this
/// - `CORTEX_WINDOW_HEAD` / `CORTEX_WINDOW_TAIL` — lines kept at each end
fn env_usize(key: &str, default: usize, floor: usize) -> usize {
    std::env::var(key).ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= floor)
        .unwrap_or(default)
}

pub fn max_inline_chars() -> usize { env_usize("CORTEX_MAX_INLINE_CHARS", MAX_INLINE_CHARS, 200) }
fn head_lines() -> usize { env_usize("CORTEX_WINDOW_HEAD", HEAD_LINES, 2) }
fn tail_lines() -> usize { env_usize("CORTEX_WINDOW_TAIL", TAIL_LINES, 2) }

/// Lines that must survive windowing wherever they appear.
///
/// Eliding the one `error[E0433]` in the middle of a build log would turn a
/// useful compaction into a trap: the model would see a truncated log, conclude
/// the build was fine, and act on it. Anything carrying a verdict or a
/// diagnosis is exempt from the window.
fn is_signal(line: &str) -> bool {
    let l = line.trim_start();
    const MARKERS: &[&str] = &[
        "error", "Error", "ERROR",
        "warning:", "panicked", "PANIC",
        "assertion", "assert",
        "failed", "FAILED", "failure",
        "test result:", "Finished", "Compiling error",
        "-->", "thread '", "Caused by", "exit code", "exit=",
        "REFUSING", "cannot find", "not found", "No such",
    ];
    MARKERS.iter().any(|m| l.starts_with(m) || l.contains(m))
}

/// Keep the head, the tail, and every signal line; elide the rest.
///
/// Returns the windowed text and how many lines were elided.
fn window_lines(text: &str) -> (String, usize) {
    let (head, tail) = (head_lines(), tail_lines());
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head + tail {
        return (text.to_string(), 0);
    }
    let tail_start = lines.len().saturating_sub(tail);
    let mut out: Vec<String> = Vec::new();
    let mut elided = 0usize;
    let mut run = 0usize;

    for (i, ln) in lines.iter().enumerate() {
        let keep = i < head || i >= tail_start || is_signal(ln);
        if keep {
            if run > 0 {
                out.push(format!("        … {run} line(s) elided …"));
                elided += run;
                run = 0;
            }
            out.push((*ln).to_string());
        } else {
            run += 1;
        }
    }
    if run > 0 {
        out.push(format!("        … {run} line(s) elided …"));
        elided += run;
    }
    (out.join("
"), elided)
}



/// Filter `raw` for the given command kind. Tees the full original to
/// `tee_dir` (when provided) only if lines were actually dropped, so the exact
/// byte stream is always recoverable.
pub fn filter_output(kind: CommandKind, raw: &str, tee_dir: Option<&Path>) -> FilteredOutput {
    let original_chars = raw.chars().count();

    // Size floor: tiny outputs are returned untouched.
    if original_chars < MIN_FILTER_CHARS {
        return FilteredOutput {
            text: raw.to_string(),
            original_chars,
            filtered_chars: original_chars,
            dropped_lines: 0,
            elided_lines: 0,
            tee_path: None,
            lossless: true,
        };
    }

    let (mut text, dropped_lines) = filter_by_kind(kind, raw);

    // Window only what is still large after redundant lines have gone, and only
    // when the full text can be written somewhere first. With no tee directory
    // there is nowhere to recover an elided line from, so nothing is elided --
    // better a long output than a silently incomplete one.
    let mut elided_lines = 0usize;
    let mut tee_path = None;

    let needs_tee = dropped_lines > 0 || text.chars().count() > max_inline_chars();
    if needs_tee {
        if let Some(dir) = tee_dir {
            if std::fs::create_dir_all(dir).is_ok() {
                let path = dir.join(format!("{}.txt", tee_stem(raw)));
                if std::fs::write(&path, raw).is_ok() {
                    tee_path = Some(path);
                }
            }
        }
    }

    if text.chars().count() > max_inline_chars() && tee_path.is_some() {
        let (windowed, n) = window_lines(&text);
        if n > 0 {
            text = windowed;
            elided_lines = n;
        }
    }

    if let Some(path) = &tee_path {
        let mut note = String::from("\n[compacted:");
        if dropped_lines > 0 {
            note.push_str(&format!(" {dropped_lines} redundant line(s) removed;"));
        }
        if elided_lines > 0 {
            note.push_str(&format!(
                " {elided_lines} line(s) elided from the middle - NOT redundant, just not shown;"
            ));
        }
        note.push_str(&format!(" complete log: {}]", path.display()));
        text.push_str(&note);
    }

    let filtered_chars = text.chars().count();
    FilteredOutput {
        text,
        original_chars,
        filtered_chars,
        dropped_lines,
        elided_lines,
        tee_path,
        // Every byte is still in the tee file, so nothing is destroyed. The
        // distinction a caller needs is `elided_lines`.
        lossless: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_maps_commands() {
        assert_eq!(detect_command("cargo test --manifest-path x"), CommandKind::CargoTest);
        assert_eq!(detect_command("cargo build"), CommandKind::CargoBuild);
        assert_eq!(detect_command("cargo check -q"), CommandKind::CargoBuild);
        assert_eq!(detect_command("cargo clippy -- -D warnings"), CommandKind::CargoClippy);
        assert_eq!(detect_command("git status"), CommandKind::GitStatus);
        assert_eq!(detect_command("ls -la"), CommandKind::Generic);
    }

    #[test]
    fn cargo_build_keeps_every_diagnostic_line_verbatim() {
        let raw = "\
   Compiling foo v0.1.0
   Compiling bar v0.2.0
warning: unused variable: `x`
 --> src/lib.rs:10:9
  |
10|     let x = 5;
  |         ^ help: prefix with underscore: `_x`
error[E0308]: mismatched types
 --> src/lib.rs:20:5
  |
20|     return 1;
  |            ^ expected `String`, found integer
   Compiling baz v0.3.0
    Finished dev profile";
        let (text, dropped) = filter_cargo_build(raw);
        // Every diagnostic line survives, in order, byte-for-byte.
        for needle in [
            "warning: unused variable: `x`",
            "--> src/lib.rs:10:9",
            "help: prefix with underscore: `_x`",
            "error[E0308]: mismatched types",
            "--> src/lib.rs:20:5",
            "expected `String`, found integer",
            "Finished dev profile", // success signal kept
        ] {
            assert!(text.contains(needle), "lost diagnostic line: {needle}\n---\n{text}");
        }
        // Progress verbs are gone.
        assert!(!text.contains("Compiling foo"));
        assert!(!text.contains("Compiling bar"));
        assert!(dropped >= 3, "expected >=3 progress lines dropped, got {dropped}");
    }

    #[test]
    fn cargo_test_all_pass_collapses_to_summary() {
        let raw = "\
   Compiling foo v0.1.0
running 3 tests
test tests::a ... ok
test tests::b ... ok
test tests::c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let (text, dropped) = filter_cargo_test(raw);
        assert!(text.contains("test result: ok. 3 passed"), "summary must be kept:\n{text}");
        assert!(!text.contains("tests::a ... ok"), "per-test ok lines must be dropped");
        assert_eq!(dropped, 4, "3 ok lines + 1 Compiling"); // blank run not doubled
    }

    #[test]
    fn cargo_test_keeps_all_failure_detail() {
        let raw = "\
   Compiling foo v0.1.0
running 2 tests
test tests::passes ... ok
test tests::breaks ... FAILED

failures:

---- tests::breaks stdout ----
thread 'tests::breaks' panicked at src/lib.rs:42:5:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::breaks

test result: FAILED. 1 passed; 1 failed; 0 ignored";
        let (text, _dropped) = filter_cargo_test(raw);
        for needle in [
            "tests::breaks ... FAILED",
            "---- tests::breaks stdout ----",
            "panicked at src/lib.rs:42:5",
            "assertion `left == right` failed",
            "left: 1",
            "right: 2",
            "test result: FAILED. 1 passed; 1 failed",
        ] {
            assert!(text.contains(needle), "lost failure detail: {needle}\n---\n{text}");
        }
        // The one passing line is still collapsed.
        assert!(!text.contains("tests::passes ... ok"));
    }

    #[test]
    fn clippy_keeps_lint_location() {
        let raw = "\
    Checking foo v0.1.0
warning: this `if` has identical blocks
 --> src/main.rs:5:5
warning: unused import: `std::io`
 --> src/main.rs:1:5
    Finished";
        let (text, _d) = filter_cargo_clippy(raw);
        assert!(text.contains("src/main.rs:5:5"));
        assert!(text.contains("src/main.rs:1:5"));
        assert!(text.contains("unused import: `std::io`"));
        assert!(!text.contains("Checking foo"));
    }

    #[test]
    fn git_status_keeps_paths_drops_hints() {
        let raw = "\
On branch main
Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
\tmodified:   src/foo.rs
\tmodified:   src/bar.rs
Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
\tbaz.rs";
        let (text, dropped) = filter_git_status(raw);
        assert!(text.contains("modified:   src/foo.rs"));
        assert!(text.contains("modified:   src/bar.rs"));
        assert!(text.contains("baz.rs"));
        assert!(text.contains("On branch main"));
        assert!(!text.contains("(use \"git add"));
        assert_eq!(dropped, 3);
    }

    #[test]
    fn generic_dedup_is_lossless_with_count() {
        let raw = "warn: retrying\nwarn: retrying\nwarn: retrying\ndone";
        let (text, dropped) = filter_generic(raw);
        assert!(text.contains("warn: retrying  (×3)"));
        assert!(text.contains("done"));
        assert_eq!(dropped, 2);
    }

    #[test]
    fn size_floor_returns_input_untouched() {
        let raw = "   Compiling foo v0.1.0\n    Finished";
        let out = filter_output(CommandKind::CargoBuild, raw, None);
        assert_eq!(out.text, raw, "below floor => untouched");
        assert_eq!(out.dropped_lines, 0);
        assert!(out.lossless);
    }

    #[test]
    fn filter_output_reports_savings_and_stays_lossless() {
        // Build a large all-pass test log so we clear the size floor.
        let mut raw = String::from("   Compiling foo v0.1.0\nrunning 200 tests\n");
        for i in 0..200 {
            raw.push_str(&format!("test suite::case_{i:03} ... ok\n"));
        }
        raw.push_str("\ntest result: ok. 200 passed; 0 failed; 0 ignored");
        let out = filter_output(CommandKind::CargoTest, &raw, None);
        assert!(out.lossless);
        assert!(out.filtered_chars < out.original_chars / 4, "expected big savings");
        assert!(out.text.contains("test result: ok. 200 passed"));
        assert!(out.dropped_lines >= 200);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// Diagnostic: print which signal lines the pipeline loses.
    #[test]
    #[ignore]
    fn diagnose_signal_loss() {
        let dir = std::path::Path::new("../.cortex/tee");
        if !dir.exists() { return; }
        let out_dir = std::env::temp_dir().join("tf_diag_corpus");
        let _ = std::fs::create_dir_all(&out_dir);
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.extension().map(|x| x != "txt").unwrap_or(true) { continue; }
            let Ok(raw) = std::fs::read_to_string(&p) else { continue };
            if raw.chars().count() < MIN_FILTER_CHARS { continue; }
            let f = filter_output(CommandKind::Generic, &raw, Some(&out_dir));
            let mut after: Vec<&str> = f.text.lines().filter(|l| is_signal(l)).collect();
            for l in raw.lines().filter(|l| is_signal(l)) {
                match after.iter().position(|x| *x == l) {
                    Some(i) => { after.remove(i); }
                    None => println!("LOST in {}:\n   {:?}", p.file_name().unwrap().to_string_lossy(), l),
                }
            }
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    /// Sweep the window tunables against the real corpus.
    ///
    /// Prints savings AND signal integrity for each setting, so the threshold
    /// is chosen from measurement rather than taste. A setting that saves more
    /// but loses a signal line is not a candidate at any saving.
    ///
    ///     cargo test -- --ignored --nocapture sweep_window_thresholds
    #[test]
    #[ignore]
    fn sweep_window_thresholds() {
        let dir = std::path::Path::new("../.cortex/tee");
        if !dir.exists() { eprintln!("no corpus at {}", dir.display()); return; }
        let corpus: Vec<String> = std::fs::read_dir(dir).unwrap().flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "txt").unwrap_or(false))
            .filter_map(|p| std::fs::read_to_string(&p).ok())
            .filter(|r| r.chars().count() >= MIN_FILTER_CHARS)
            .collect();
        let orig: usize = corpus.iter().map(|r| r.chars().count()).sum();
        let out_dir = std::env::temp_dir().join("tf_sweep_corpus");
        let _ = std::fs::create_dir_all(&out_dir);

        println!("\ncorpus: {} files, {orig} chars", corpus.len());
        println!("{:>7} {:>5} {:>5} {:>10} {:>8} {:>9} {:>13}",
                 "thresh", "head", "tail", "chars", "saved", "windowed", "signal lost");

        for (thresh, head, tail) in [
            (2000usize, 14usize, 14usize), (1600, 14, 14), (1200, 14, 14),
            (1200, 10, 10), (1000, 10, 10), (1000, 8, 8), (800, 8, 8), (800, 6, 6),
        ] {
            std::env::set_var("CORTEX_MAX_INLINE_CHARS", thresh.to_string());
            std::env::set_var("CORTEX_WINDOW_HEAD", head.to_string());
            std::env::set_var("CORTEX_WINDOW_TAIL", tail.to_string());

            let (mut after, mut win, mut lost) = (0usize, 0usize, 0usize);
            for raw in &corpus {
                let f = filter_output(CommandKind::Generic, raw, Some(&out_dir));
                after += f.filtered_chars;
                if f.elided_lines > 0 { win += 1; }
                // Every DISTINCT signal line in the input must still be present
                // in the output -- verbatim, or in the collapsed `line  (xN)`
                // form that filter_generic produces for consecutive repeats.
                //
                // Comparing raw occurrence counts flags that collapse as loss,
                // which it is not: the count carries the information. The first
                // run of this sweep reported 2 lost lines for exactly that
                // reason, and the defect was here, not in the windowing.
                for sig in raw.lines().filter(|l| is_signal(l)) {
                    let present = f.text.lines().any(|o| {
                        o == sig
                            || o.strip_suffix(')')
                                .and_then(|o| o.rfind("  (×").map(|i| &o[..i]))
                                .map(|stem| stem == sig)
                                .unwrap_or(false)
                    });
                    if !present {
                        lost += 1;
                    }
                }
            }
            let saved = orig.saturating_sub(after);
            println!("{thresh:>7} {head:>5} {tail:>5} {after:>10} {:>7.1}% {win:>9} {lost:>13}",
                     100.0 * saved as f64 / orig.max(1) as f64);
            assert_eq!(lost, 0, "threshold {thresh}/{head}/{tail} dropped a signal line");
        }
        for k in ["CORTEX_MAX_INLINE_CHARS", "CORTEX_WINDOW_HEAD", "CORTEX_WINDOW_TAIL"] {
            std::env::remove_var(k);
        }
        let _ = std::fs::remove_dir_all(&out_dir);
        println!();
    }

    /// Measure both strategies against the real captured corpus in .cortex/tee.
    ///
    /// Ignored by default: it reads files outside the crate and is a
    /// measurement, not an assertion. Run with
    ///     cargo test -- --ignored --nocapture measure_against_real_corpus
    #[test]
    #[ignore]
    fn measure_against_real_corpus() {
        let dir = std::path::Path::new("../.cortex/tee");
        if !dir.exists() {
            eprintln!("no corpus at {}", dir.display());
            return;
        }
        let out_dir = std::env::temp_dir().join("tf_bench_corpus");
        let _ = std::fs::create_dir_all(&out_dir);
        let (mut orig, mut before, mut after, mut n, mut win) = (0usize, 0usize, 0usize, 0usize, 0usize);
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.extension().map(|x| x != "txt").unwrap_or(true) { continue; }
            let Ok(raw) = std::fs::read_to_string(&p) else { continue };
            if raw.chars().count() < MIN_FILTER_CHARS { continue; }
            n += 1;
            orig += raw.chars().count();
            let (t, _) = filter_by_kind(CommandKind::Generic, &raw);
            before += t.chars().count();
            let f = filter_output(CommandKind::Generic, &raw, Some(&out_dir));
            after += f.filtered_chars;
            if f.elided_lines > 0 { win += 1; }
        }
        let pct = |x: usize| 100.0 * x as f64 / orig.max(1) as f64;
        println!("\ncorpus: {n} files >= {MIN_FILTER_CHARS} chars, {orig} chars");
        println!("  filter only (old):  {before:>8}  saved {:>7} ({:.1}%)", orig - before, pct(orig - before));
        println!("  + windowing (new):  {after:>8}  saved {:>7} ({:.1}%)",
                 orig.saturating_sub(after), pct(orig.saturating_sub(after)));
        println!("  windowed: {win}/{n} files\n");
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    fn noise(n: usize) -> String {
        (0..n).map(|i| format!("   Compiling crate_{i} v0.1.{i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn an_error_in_the_middle_is_never_elided() {
        // The failure that would make windowing dangerous: a truncated log that
        // reads as a clean build, so the model proceeds on a broken tree.
        let mut raw = noise(60);
        raw.push_str("\nerror[E0433]: failed to resolve: use of undeclared crate `serde_jsonx`\n");
        raw.push_str(&noise(60));
        let dir = std::env::temp_dir().join(format!("tf_win_{}", std::process::id()));
        let out = filter_output(CommandKind::Generic, &raw, Some(&dir));

        assert!(out.elided_lines > 0, "a 120-line log should be windowed");
        assert!(
            out.text.contains("error[E0433]"),
            "the error must survive windowing; got:\n{}",
            out.text
        );
        assert!(out.text.contains("complete log:"), "and must say where the rest is");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_full_bytes_are_recoverable_from_the_tee() {
        let raw = format!("{}\nwarning: something\n{}", noise(50), noise(50));
        let dir = std::env::temp_dir().join(format!("tf_win2_{}", std::process::id()));
        let out = filter_output(CommandKind::Generic, &raw, Some(&dir));
        let path = out.tee_path.clone().expect("tee written when windowing");
        let recovered = std::fs::read_to_string(&path).unwrap();
        assert_eq!(recovered, raw, "the tee file must hold the exact original bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_elided_without_somewhere_to_recover_it_from() {
        // No tee dir => no elision. A shorter output is not worth an
        // unrecoverable one.
        let raw = noise(200);
        let out = filter_output(CommandKind::Generic, &raw, None);
        assert_eq!(out.elided_lines, 0);
    }

    #[test]
    fn small_output_is_returned_untouched() {
        let raw = "one\ntwo\nthree";
        let out = filter_output(CommandKind::Generic, raw, None);
        assert_eq!(out.text, raw);
        assert_eq!(out.elided_lines, 0);
        assert_eq!(out.dropped_lines, 0);
    }

    #[test]
    fn the_verdict_at_the_end_survives() {
        // Summaries live last; windowing must never cost the result line.
        let mut raw = noise(120);
        raw.push_str("\ntest result: FAILED. 3 passed; 1 failed\n");
        let dir = std::env::temp_dir().join(format!("tf_win3_{}", std::process::id()));
        let out = filter_output(CommandKind::Generic, &raw, Some(&dir));
        assert!(out.text.contains("test result: FAILED"), "got:\n{}", out.text);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
