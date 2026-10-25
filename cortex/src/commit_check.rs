//! The commit-time half of the gate on unfinished work: a commit whose added Rust lines break an
//! error-level ssnebula rule is refused, the rule closeout holds an agent to. It reads the staged
//! text, not the working copy, and only the lines the commit adds. Warnings are counted, never
//! refused; `git commit --no-verify` skips the check.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::edit_lint::{EditLint, Finding};

pub struct CommitCheck {
    repo: PathBuf,
}

/// What the staged Rust files hold, as the check saw it.
#[derive(Default)]
struct Verdict {
    errors: Vec<String>,
    warnings: usize,
    unchecked: Vec<String>,
}

impl CommitCheck {
    /// The repository holding `dir` (the current directory when `None`).
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = dir.unwrap_or_else(|| PathBuf::from("."));
        let top = Self::git_in(&dir, &["rev-parse", "--show-toplevel"])?;
        Ok(Self { repo: PathBuf::from(top.trim()) })
    }

    /// The `cortex commit-check` command: exits 1 when the commit must not go ahead. `install`
    /// sets the repository up instead (`cortex repo-setup`).
    pub fn main(dir: Option<PathBuf>, install: bool) -> Result<()> {
        if install {
            return crate::repo_setup::RepoSetup::main(dir, false);
        }
        let check = Self::new(dir)?;
        let verdict = check.verdict()?;
        eprint!("{}", verdict.report());
        if !verdict.errors.is_empty() {
            std::process::exit(1);
        }
        Ok(())
    }

    fn verdict(&self) -> Result<Verdict> {
        let diff = self.git(&["-c", "core.quotePath=false", "diff", "--cached", "--no-color", "--no-ext-diff", "-U0", "--diff-filter=AM", "--", "*.rs"])?;
        let mut verdict = Verdict::default();
        for (rel, ranges) in Self::added_lines(&diff) {
            match self.findings(&rel, &ranges) {
                Some(found) => verdict.add(&rel, &found),
                None => verdict.unchecked.push(rel),
            }
        }
        Ok(verdict)
    }

    /// The lines each file's staged diff adds, from `git diff -U0` output.
    fn added_lines(diff: &str) -> Vec<(String, Vec<(usize, usize)>)> {
        let mut files: Vec<(String, Vec<(usize, usize)>)> = Vec::new();
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ b/") {
                files.push((path.to_owned(), Vec::new()));
            } else if let (Some(hunk), Some((_, ranges))) = (line.strip_prefix("@@ "), files.last_mut()) {
                ranges.extend(Self::hunk_range(hunk));
            }
        }
        files.retain(|(_, ranges)| !ranges.is_empty());
        files
    }

    /// `-a,b +c,d @@` adds lines c to c+d-1; a missing `d` means one line, zero means none.
    fn hunk_range(hunk: &str) -> Option<(usize, usize)> {
        let added = hunk.split_whitespace().find_map(|part| part.strip_prefix('+'))?;
        let (start, count) = added.split_once(',').unwrap_or((added, "1"));
        let (start, count): (usize, usize) = (start.parse().ok()?, count.parse().ok()?);
        (count > 0).then(|| (start, start + count - 1))
    }

    fn findings(&self, rel: &str, ranges: &[(usize, usize)]) -> Option<Vec<Finding>> {
        let staged = self.git(&["show", &format!(":{rel}")]).ok()?;
        let spec = ranges.iter().map(|(first, last)| format!("{first}-{last}")).collect::<Vec<_>>().join(",");
        let file = self.repo.join(rel);
        let args = ["check".as_ref(), file.as_os_str(), "--stdin".as_ref(), "--lines".as_ref(), spec.as_ref()];
        EditLint::run_check(&self.repo, &args, Some(&staged))
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        Self::git_in(&self.repo, args)
    }

    fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
        if !output.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Verdict {
    fn add(&mut self, rel: &str, found: &[Finding]) {
        for finding in found {
            if finding.is_error() {
                self.errors.push(finding.located(rel));
            } else {
                self.warnings += 1;
            }
        }
    }

    /// What the committer reads: nothing when every staged line passed.
    fn report(&self) -> String {
        let mut lines = Vec::new();
        if !self.errors.is_empty() {
            lines.push("commit refused: the lines being committed break error-level ssnebula rules (fine while debugging, never in a commit):".to_owned());
            lines.extend(self.errors.iter().map(|e| format!("  {e}")));
            lines.push("fix them and stage the fix; `git commit --no-verify` skips this check".to_owned());
        }
        if self.warnings > 0 {
            lines.push(format!("ssnebula: {} warning(s) in the lines being committed; `ssnebula check FILE --lines ...` lists them", self.warnings));
        }
        if !self.unchecked.is_empty() {
            lines.push(format!("ssnebula could not check {} (not found, or too old for `check --stdin`)", self.unchecked.join(", ")));
        }
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_lines_come_from_each_files_hunks() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn x\n+one\n+two\n@@ -10 +12 @@\n-old\n+new\n@@ -20,3 +23,0 @@\n-gone\n\
                    diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1,2 +0,0 @@\n-x\n-y\n";
        assert_eq!(CommitCheck::added_lines(diff), vec![("src/a.rs".to_string(), vec![(4, 5), (12, 12)])]);
    }

    #[test]
    fn a_clean_commit_says_nothing() {
        assert_eq!(Verdict::default().report(), "");
        let refused = Verdict { errors: vec!["src/a.rs:3 `dbg_macro`: x".into()], warnings: 2, unchecked: vec![] }.report();
        assert!(refused.starts_with("commit refused") && refused.contains("2 warning(s)"), "{refused}");
    }
}
