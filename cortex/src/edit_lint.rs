//! The Rust code rules an edit just broke, told to the agent while the code is still in front of it.
//!
//! Only the lines the edit wrote are checked, so a file's older findings never appear and a fix
//! is always to the agent's own text. Nothing is blocked: temporary debug output has to stay
//! writable, and the gate on unfinished work is closeout, not the edit.

use std::path::{Path, PathBuf};

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

    /// The hook's whole job: check the edit, record that the check ran, and word what it found.
    pub fn after_edit(store: &Store, root: &'a Path, file_path: &str, added: &str) -> Option<String> {
        let lint = Self::new(root, file_path)?;
        let findings = lint.check(added)?;
        let _ = crate::corrections::beat_named(store, Self::HEARTBEAT, !findings.is_empty());
        lint.message(&findings)
    }

    /// `after_edit` for each file of a call. The lint reads each file as the edit left it, so a
    /// host that judges edits before they land (VS Code) calls this once they have.
    pub fn after_edits(store: &Store, root: &'a Path, edits: Vec<(String, String)>) -> Vec<String> {
        edits.into_iter().filter_map(|(file, added)| Self::after_edit(store, root, &file, &added)).collect()
    }

    /// The findings on the lines the edit wrote, or `None` when ssnebula could not run or the
    /// written lines cannot be told apart.
    pub fn check(&self, added: &str) -> Option<Vec<Finding>> {
        let text = std::fs::read_to_string(&self.file).ok()?;
        let lines = Self::written_lines(&text, added)?;
        let spec = lines.iter().map(|(first, last)| format!("{first}-{last}")).collect::<Vec<_>>().join(",");
        let args = ["check".as_ref(), self.file.as_os_str(), "--lines".as_ref(), spec.as_ref()];
        let json = Ssnebula::output(self.root, &args, |out| out.trim_start().starts_with('['))?;
        let mut findings: Vec<Finding> = serde_json::from_str(&json).ok()?;
        findings.sort_by_key(|f| (f.level != "deny", f.line));
        Some(findings)
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
            let level = if f.level == "deny" { "error" } else { "warning" };
            let fix = f.help.as_deref().map(|h| format!(" Fix: {h}.")).unwrap_or_default();
            format!("- line {} {level} `{}`: {}.{fix}", f.line, f.lint, f.message)
        }));
        if findings.len() > shown.len() {
            lines.push(format!("- and {} more: `ssnebula check {display}`", findings.len() - shown.len()));
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(lint: &str, level: &str, at_line: usize) -> Finding {
        Finding { lint: lint.into(), level: level.into(), message: format!("{lint} here"), line: at_line, help: Some("do better".into()) }
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
}
