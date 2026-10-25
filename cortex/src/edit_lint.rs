//! The Rust code rules an edit just broke, told to the agent while the code is still in front of it.
//!
//! Only the lines the edit wrote are checked, so a file's older findings never appear and a fix
//! is always to the agent's own text. Nothing is blocked: temporary debug output has to stay
//! writable. The gate on unfinished work is `OpenErrors` at closeout, and the commit check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Deserialize;

use crate::memory::Store;
use crate::ssnebula::Ssnebula;

/// One finding as `ssnebula check` prints it.
#[derive(Debug, Deserialize)]
pub struct Finding {
    lint: String,
    level: String,
    message: String,
    line: usize,
    help: Option<String>,
    #[serde(default)]
    snippet: String,
}

impl Finding {
    pub fn is_error(&self) -> bool {
        self.level == "deny"
    }

    /// `file:line `lint`: message`, for lists that name where each finding is.
    pub fn located(&self, display: &str) -> String {
        format!("{display}:{} `{}`: {}", self.line, self.lint, self.message)
    }
}

/// One run of the check: what it found and how many lines the edit wrote.
pub struct Checked {
    pub findings: Vec<Finding>,
    pub lines: usize,
}

pub struct EditLint<'a> {
    root: &'a Path,
    file: PathBuf,
}

impl<'a> EditLint<'a> {
    /// Findings listed in full; the rest are counted.
    const SHOWN: usize = 5;
    /// More places than this holding the edit's text means it cannot tell which one it wrote.
    const MAX_PLACES: usize = 3;
    const HEARTBEAT: &'static str = "edit_lint";

    /// A lint of `file_path`, or `None` for anything but Rust.
    pub fn new(root: &'a Path, file_path: &str) -> Option<Self> {
        let file = root.join(file_path);
        (file.extension()? == "rs").then_some(Self { root, file })
    }

    /// The hook's whole job: set up the file's repository if it is new to cortex, check the edit,
    /// record that the check ran and any error it found, and word what it found.
    pub fn after_edit(store: &Store, session_id: &str, root: &'a Path, file_path: &str, added: &str) -> Option<String> {
        crate::repo_setup::RepoSetup::after_edit(&root.join(file_path));
        let lint = Self::new(root, file_path)?;
        let checked = lint.check(added)?;
        let _ = crate::corrections::beat_named(store, Self::HEARTBEAT, !checked.findings.is_empty());
        lint.remember_errors(store, session_id, &checked.findings);
        lint.log(store, session_id, &checked);
        lint.message(&checked.findings)
    }

    /// The run, for the scoreboard, and each rule broken, counted once per session as a
    /// recurring failure: a rule agents keep breaking reaches the review queue like an error
    /// that keeps coming back.
    fn log(&self, store: &Store, session_id: &str, checked: &Checked) {
        let file = self.file.to_string_lossy();
        let logged = store.conn().execute(
            "INSERT INTO edit_lint_checks (session_id, file, lines, findings) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, file, checked.lines, checked.findings.len()],
        );
        let check_id = logged.map(|_| store.conn().last_insert_rowid()).unwrap_or_default();
        for f in &checked.findings {
            let _ = store.conn().execute(
                "INSERT INTO edit_lint_findings (check_id, lint, level) VALUES (?1, ?2, ?3)",
                params![check_id, f.lint, f.level],
            );
            let sample = format!("{}: {}", f.message, f.snippet.trim());
            let _ = crate::test_signal::RecurringErrors::note(store, session_id, &format!("ssnebula:{}", f.lint), &sample, "ssnebula (edit lint)");
        }
    }

    /// `after_edit` for each file of a call. The lint reads each file as the edit left it, so a
    /// host that judges edits before they land (VS Code) calls this once they have.
    pub fn after_edits(store: &Store, session_id: &str, root: &'a Path, edits: Vec<(String, String)>) -> Vec<String> {
        edits.into_iter().filter_map(|(file, added)| Self::after_edit(store, session_id, root, &file, &added)).collect()
    }

    /// Error-level findings go on record for closeout; a week later they are dropped.
    fn remember_errors(&self, store: &Store, session_id: &str, findings: &[Finding]) {
        let now = chrono::Utc::now();
        let stale = (now - chrono::Duration::days(7)).to_rfc3339();
        let _ = store.conn().execute("DELETE FROM edit_lint_errors WHERE reported_at < ?1", params![stale]);
        for f in findings.iter().filter(|f| f.is_error()) {
            let _ = store.conn().execute(
                "INSERT OR IGNORE INTO edit_lint_errors (session_id, file, lint, snippet, reported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, self.file.to_string_lossy(), f.lint, f.snippet.trim(), now.to_rfc3339()],
            );
        }
    }

    /// The findings on the lines the edit wrote, or `None` when ssnebula could not run or the
    /// written lines cannot be told apart.
    pub fn check(&self, added: &str) -> Option<Checked> {
        let text = std::fs::read_to_string(&self.file).ok()?;
        let lines = Self::written_lines(&text, added)?;
        let count = lines.iter().map(|(first, last)| last + 1 - first).sum();
        let spec = lines.iter().map(|(first, last)| format!("{first}-{last}")).collect::<Vec<_>>().join(",");
        let args = ["check".as_ref(), self.file.as_os_str(), "--lines".as_ref(), spec.as_ref()];
        let mut findings = Self::run_check(self.root, &args, None)?;
        findings.sort_by_key(|f| (!f.is_error(), f.line));
        Some(Checked { findings, lines: count })
    }

    /// `ssnebula check` with `args` (and `input` on standard input), parsed; `None` when no
    /// ssnebula could run it.
    pub fn run_check<S: AsRef<std::ffi::OsStr>>(root: &Path, args: &[S], input: Option<&str>) -> Option<Vec<Finding>> {
        let json = Ssnebula::output_with_input(root, args, input, |out| out.trim_start().starts_with('['))?;
        serde_json::from_str(&json).ok()
    }

    /// Where the edit's text now sits, as inclusive line ranges. The text of an Edit can recur in
    /// a file; past a few places it is too common to say which one was written.
    fn written_lines(text: &str, added: &str) -> Option<Vec<(usize, usize)>> {
        let added = added.trim_end_matches('\n');
        if added.trim().is_empty() {
            return None;
        }
        let span = added.matches('\n').count();
        let places: Vec<(usize, usize)> = text
            .match_indices(added)
            .map(|(at, _)| text[..at].matches('\n').count() + 1)
            .map(|first| (first, first + span))
            .take(Self::MAX_PLACES + 1)
            .collect();
        (!places.is_empty() && places.len() <= Self::MAX_PLACES).then_some(places)
    }

    /// The note for the agent, or `None` when the written lines follow every rule.
    pub fn message(&self, findings: &[Finding]) -> Option<String> {
        let shown = findings.get(..Self::SHOWN.min(findings.len())).filter(|f| !f.is_empty())?;
        let display = self.file.strip_prefix(self.root).unwrap_or(&self.file).display();
        let mut lines = vec![format!(
            "[ssnebula] {display}: the lines just written break {} Rust code rule{} \
             (full text in .claude/rules/rust-style.md). Fix them now, in this file:",
            findings.len(),
            if findings.len() == 1 { "" } else { "s" },
        )];
        lines.extend(shown.iter().map(|f| {
            let level = if f.is_error() { "error" } else { "warning" };
            let fix = f.help.as_deref().map(|h| format!(" Fix: {h}.")).unwrap_or_default();
            format!("- line {} {level} `{}`: {}.{fix}", f.line, f.lint, f.message)
        }));
        if findings.len() > shown.len() {
            lines.push(format!("- and {} more: `ssnebula check {display}`", findings.len() - shown.len()));
        }
        Some(lines.join("\n"))
    }
}

/// Error-level findings an edit lint reported that are still in their files: what a closeout
/// claiming `build_pass` refuses on. A finding is matched by its lint and its line's text, so it
/// is found wherever later edits moved it, and is gone once the line is fixed or deleted.
pub struct OpenErrors;

impl OpenErrors {
    /// Hook sessions (`vscode:...`) are not the MCP session that closes out, so their records
    /// from this long ago count too.
    const HOOK_WINDOW_HOURS: i64 = 12;

    /// Each error still present, as `file:line lint: message`.
    pub fn find(store: &Store, session_id: &str, root: &Path) -> Vec<String> {
        Self::recorded(store, session_id)
            .into_iter()
            .flat_map(|(file, wanted)| Self::still_present(root, &file, &wanted))
            .collect()
    }

    fn recorded(store: &Store, session_id: &str) -> BTreeMap<String, Vec<(String, String)>> {
        let since = (chrono::Utc::now() - chrono::Duration::hours(Self::HOOK_WINDOW_HOURS)).to_rfc3339();
        let mut by_file: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let Ok(mut query) = store.conn().prepare(
            "SELECT file, lint, snippet FROM edit_lint_errors
             WHERE session_id = ?1 OR (instr(session_id, ':') > 0 AND reported_at >= ?2)",
        ) else {
            return by_file;
        };
        let rows = query.query_map(params![session_id, since], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)));
        for (file, lint, snippet) in rows.into_iter().flatten().flatten() {
            by_file.entry(file).or_default().push((lint, snippet));
        }
        by_file
    }

    /// The recorded errors that a fresh check of `file` still finds. Without ssnebula, or once
    /// the file is gone, nothing can be shown to remain, and nothing is.
    fn still_present(root: &Path, file: &str, wanted: &[(String, String)]) -> Vec<String> {
        let args = ["check", file];
        let findings = EditLint::run_check(root, &args, None).unwrap_or_default();
        let display = Path::new(file).strip_prefix(root).unwrap_or(Path::new(file)).display().to_string();
        Self::kept(&findings, wanted, &display)
    }

    fn kept(findings: &[Finding], wanted: &[(String, String)], display: &str) -> Vec<String> {
        findings
            .iter()
            .filter(|f| f.is_error() && wanted.iter().any(|(lint, snippet)| *lint == f.lint && snippet == f.snippet.trim()))
            .map(|f| f.located(display))
            .collect()
    }

    /// The refusal closeout returns, naming each error.
    pub fn refusal(open: &[String]) -> String {
        format!(
            "closeout refused: build_pass waits until the error-level ssnebula findings written this \
             session are gone (fine while debugging, never in finished work). Nothing was logged.\n{}\n\
             Fix them, then call closeout_session again.",
            open.iter().map(|line| format!("- {line}")).collect::<Vec<_>>().join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(lint: &str, level: &str, at_line: usize) -> Finding {
        Finding { lint: lint.into(), level: level.into(), message: format!("{lint} here"), line: at_line, help: Some("do better".into()), snippet: String::new() }
    }

    #[test]
    fn only_rust_files_are_linted() {
        let root = Path::new("/w");
        assert!(EditLint::new(root, "src/a.rs").is_some());
        assert!(EditLint::new(root, "README.md").is_none() && EditLint::new(root, "Makefile").is_none());
    }

    #[test]
    fn written_lines_are_where_the_text_now_sits() {
        let text = "fn a() {}\nfn b() {\n    1\n}\nfn c() {}\n";
        assert_eq!(EditLint::written_lines(text, "fn b() {\n    1\n}\n"), Some(vec![(2, 4)]));
        assert_eq!(EditLint::written_lines(text, text), Some(vec![(1, 5)]));
        assert_eq!(EditLint::written_lines(text, "fn d() {}"), None, "not written yet");
        assert_eq!(EditLint::written_lines(text, "  \n"), None, "nothing written");
        assert_eq!(EditLint::written_lines("}\n}\n}\n}\n", "}"), None, "too common to place");
    }

    #[test]
    fn the_message_puts_errors_first_and_counts_the_overflow() {
        let lint = EditLint { root: Path::new("/w"), file: PathBuf::from("/w/src/a.rs") };
        assert_eq!(lint.message(&[]), None);
        let mut findings: Vec<Finding> = (1..=6).map(|line| sample("vague_name", "warn", line)).collect();
        findings.push(sample("dbg_macro", "deny", 9));
        findings.sort_by_key(|f| (f.level != "deny", f.line));
        let text = lint.message(&findings).unwrap();
        assert!(text.starts_with("[ssnebula] src/a.rs: the lines just written break 7 Rust code rules"), "{text}");
        assert!(text.lines().nth(1).unwrap().starts_with("- line 9 error `dbg_macro`"), "{text}");
        assert!(text.ends_with("- and 2 more: `ssnebula check src/a.rs`"), "{text}");
    }

    fn scratch_store(tag: &str) -> (crate::test_support::TempDir, Store) {
        let d = crate::test_support::TempDir::new(tag).unwrap();
        let db = d.join("memory.db");
        std::fs::File::create(&db).unwrap();
        let store = Store::open(&db).unwrap();
        (d, store)
    }

    #[test]
    fn errors_are_remembered_and_warnings_are_not() {
        let (_d, store) = scratch_store("lint_remember");
        let lint = EditLint { root: Path::new("/w"), file: PathBuf::from("/w/src/a.rs") };
        let mut error = sample("dbg_macro", "deny", 3);
        error.snippet = "    dbg!(x);".into();
        lint.remember_errors(&store, "session_a", &[error, sample("vague_name", "warn", 4)]);
        let mine = OpenErrors::recorded(&store, "session_a");
        assert_eq!(mine.get("/w/src/a.rs"), Some(&vec![("dbg_macro".to_string(), "dbg!(x);".to_string())]));
        assert!(OpenErrors::recorded(&store, "session_b").is_empty(), "another MCP session's records are its own");
        lint.remember_errors(&store, "vscode:chat1", &[sample("dbg_macro", "deny", 9)]);
        assert_eq!(OpenErrors::recorded(&store, "session_b").len(), 1, "a recent hook session's record counts");
    }

    #[test]
    fn an_error_is_open_while_its_line_is_still_there_wherever_it_moved() {
        let wanted = vec![("dbg_macro".to_string(), "dbg!(x);".to_string())];
        let mut moved = sample("dbg_macro", "deny", 40);
        moved.snippet = "        dbg!(x);".into();
        let open = OpenErrors::kept(&[moved], &wanted, "src/a.rs");
        assert_eq!(open, vec!["src/a.rs:40 `dbg_macro`: dbg_macro here".to_string()]);
        let mut other = sample("dbg_macro", "deny", 41);
        other.snippet = "dbg!(y);".into();
        assert!(OpenErrors::kept(&[other], &wanted, "src/a.rs").is_empty(), "a different line is not the one reported");
        assert!(OpenErrors::refusal(&open).starts_with("closeout refused"));
    }

    #[test]
    fn each_run_is_logged_and_a_broken_rule_recurs_once_per_session() {
        let (_d, store) = scratch_store("lint_log");
        let lint = EditLint { root: Path::new("/w"), file: PathBuf::from("/w/src/a.rs") };
        let run = || Checked { findings: vec![sample("vague_name", "warn", 2), sample("vague_name", "warn", 3)], lines: 4 };
        lint.log(&store, "session_a", &run());
        lint.log(&store, "session_a", &run());
        lint.log(&store, "session_b", &run());
        let count = |sql: &str| -> i64 { store.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM edit_lint_checks"), 3);
        assert_eq!(count("SELECT COUNT(*) FROM edit_lint_findings"), 6);
        assert_eq!(count("SELECT seen_count FROM recurring_errors WHERE signature = 'ssnebula:vague_name'"), 2, "sessions, not sightings");
    }
}
