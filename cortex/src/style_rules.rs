//! The Rust code rules, put where each host loads them on its own whenever Rust is in play.
//!
//! The text comes from ssnebula's lints (`ssnebula rules`), so a limit or word list an agent reads
//! is the one the check enforces. Claude Code loads `.claude/rules/rust-style.md` through its `paths`
//! frontmatter; VS Code Copilot applies `.github/instructions/rust-style.instructions.md`
//! through `applyTo`. Both files are generated whole: the marker's stamp tells a stale copy,
//! which is replaced, from one a person edited, which is kept until `--force`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::instructions::{self, Opts, Outcome};
use crate::memory::Store;
use crate::scoreboard::LintWindow;
use crate::ssnebula::Ssnebula;

/// One host's copy: where it goes and the frontmatter that scopes it to Rust files.
struct Target {
    rel: &'static str,
    front: &'static str,
}

pub struct StyleRules {
    root: PathBuf,
    /// The generated rules, or `None` when no ssnebula with a `rules` command was found.
    body: Option<String>,
}

impl StyleRules {
    const TARGETS: [Target; 2] = [
        Target { rel: ".claude/rules/rust-style.md", front: "---\npaths:\n  - \"**/*.rs\"\n---" },
        Target { rel: ".github/instructions/rust-style.instructions.md", front: "---\napplyTo: \"**/*.rs\"\n---" },
    ];
    const MARKER: &'static str = "<!-- cortex_suite:generated stamp=";
    const NOTE: &'static str = "written by `cortex instructions` from SSNebula's lints; \
        change ssnebula.toml or SSNebula, not this file, which an update rewrites -->";
    const POLICY: &'static str = "Every line of Rust you write or rewrite in this workspace follows \
        these rules, even beside older code that does not: here the rules win over matching the \
        surrounding style. Do not refactor code you are not otherwise changing just to satisfy them.";
    /// How far back "broken most often" looks.
    const RECENT_DAYS: i64 = 14;

    /// The rules under the workspace's `ssnebula.toml`, or ssnebula's defaults without one.
    pub fn load(root: &Path) -> Self {
        let args = [root.as_os_str(), "rules".as_ref()];
        let guide = Ssnebula::output(root, &args, |text| text.starts_with("# Rust code rules"));
        Self { root: root.to_path_buf(), body: guide.map(|text| Self::policy_under_title(&text)) }
    }

    /// The rules the edit check found broken most in the last `RECENT_DAYS`, most first, from
    /// the store at `db`; none when there is no store yet.
    pub fn recent_from(db: &Path) -> Vec<(String, i64)> {
        if !db.is_file() {
            return Vec::new();
        }
        let now = chrono::Utc::now().timestamp();
        let since = now - Self::RECENT_DAYS * 86_400;
        Store::open(db).map(|store| LintWindow::measure(&store, since, now + 1).top).unwrap_or_default()
    }

    /// Puts the rules broken most lately ahead of the full list, each as its own rule line with
    /// how often, so a session reads them before the rest.
    pub fn emphasise(mut self, recent: &[(String, i64)]) -> Self {
        let section = self.body.as_deref().and_then(|body| Self::most_broken(body, recent));
        if let (Some(body), Some(section)) = (self.body.as_mut(), section) {
            let at = body.find("\n## ").unwrap_or(body.len());
            body.insert_str(at, &format!("\n{section}\n"));
        }
        self
    }

    fn most_broken(body: &str, recent: &[(String, i64)]) -> Option<String> {
        let rules: Vec<String> = recent
            .iter()
            .filter_map(|(lint, times)| {
                let rule = body.lines().find(|line| line.starts_with(&format!("**`{lint}`**")))?;
                Some(format!("- {rule} Broken {times} time{} lately.", if *times == 1 { "" } else { "s" }))
            })
            .collect();
        (!rules.is_empty()).then(|| {
            format!(
                "## Broken most often in the last {} days\n\nThe edit check found these rules broken most in this \
                 workspace lately, so they come first. Each is also in its place below, with its limits and \
                 word lists.\n\n{}",
                Self::RECENT_DAYS,
                rules.join("\n")
            )
        })
    }

    /// ssnebula's guide with this workspace's rule on older code under its title.
    fn policy_under_title(guide: &str) -> String {
        let (title, rest) = guide.split_once('\n').unwrap_or((guide, ""));
        format!("{title}\n\n{}\n{rest}", Self::POLICY)
    }

    /// Bring every host's copy up to date, reported like the instruction files.
    pub fn sync(&self, opts: &Opts) -> Result<Vec<Outcome>> {
        Self::TARGETS.iter().map(|target| self.sync_one(target, opts)).collect()
    }

    /// The file as written: frontmatter, the stamped marker, then the rules.
    fn document(target: &Target, body: &str) -> String {
        let stamp = instructions::stamp_of(&Self::unstamped(target.front, body));
        format!("{}\n\n{}{stamp} · {}\n\n{body}", target.front, Self::MARKER, Self::NOTE)
    }

    /// Everything the stamp covers: the file without its marker line.
    fn unstamped(front: &str, body: &str) -> String {
        format!("{front}\n\n{body}")
    }

    /// Whether a copy on disk is still exactly as `document` wrote it, whatever version it was.
    fn is_untouched(text: &str) -> bool {
        let Some(at) = text.find(Self::MARKER) else { return false };
        let line_end = text[at..].find('\n').map_or(text.len(), |n| at + n);
        let stamp = text[at + Self::MARKER.len()..line_end].split_whitespace().next().unwrap_or_default();
        let without = format!("{}{}", &text[..at], text[line_end..].trim_start_matches('\n'));
        instructions::stamp_of(&without) == stamp
    }

    fn sync_one(&self, target: &Target, opts: &Opts) -> Result<Outcome> {
        let path = self.root.join(target.rel);
        let Some(body) = &self.body else { return Ok(Self::unavailable(target)) };
        let wanted = Self::document(target, body);
        let existing = std::fs::read_to_string(&path).ok();
        let state = match existing.as_deref() {
            None => "missing",
            Some(text) if instructions::stamp_of(text) == instructions::stamp_of(&wanted) => "current",
            Some(text) if Self::is_untouched(text) => "outdated",
            Some(_) => "edited",
        };
        let mut outcome = Outcome {
            file: target.rel,
            state,
            action: "up to date".into(),
            changed: false,
            attention: false,
            backup: None,
        };
        if state == "current" {
            return Ok(outcome);
        }
        if state == "edited" && !opts.force {
            outcome.attention = true;
            outcome.action = "edited by hand, so kept; rerun with --force to regenerate it (a backup is kept)".into();
            return Ok(outcome);
        }
        outcome.changed = true;
        if opts.check {
            outcome.action = format!("would change: {}", Self::verb(state));
            return Ok(outcome);
        }
        if let (Some(text), "edited") = (existing.as_deref(), state) {
            outcome.backup = Some(instructions::backup(&self.root, &path, text)?.display().to_string());
        }
        Self::write(&path, &wanted)?;
        outcome.action = Self::verb(state).into();
        Ok(outcome)
    }

    /// Without ssnebula nothing is written and nothing waits on a person: the workspace has no
    /// generated rules, and any copy already there is left as it is.
    fn unavailable(target: &Target) -> Outcome {
        Outcome {
            file: target.rel,
            state: "unavailable",
            action: "left as it is: no SSNebula with a `rules` command was found (build ssnebula/ in the \
                     workspace, put it on PATH, or set CORTEX_SSNEBULA)"
                .into(),
            changed: false,
            attention: false,
            backup: None,
        }
    }

    fn verb(state: &str) -> &'static str {
        match state {
            "missing" => "written",
            "outdated" => "regenerated",
            _ => "regenerated over a hand edit",
        }
    }

    fn write(path: &Path, text: &str) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUIDE: &str = "# Rust code rules (SSNebula)\n\nSSNebula 0.1.0 checks these rules.\n\n## Structure\n\n**`long_function`** (warning). Keep it short.\n- Limit: 60 lines\n";

    fn plain() -> Opts {
        Opts { check: false, force: false, adopt: false }
    }

    fn rules(root: &Path, guide: Option<&str>, opts: &Opts) -> Vec<Outcome> {
        let rules = StyleRules { root: root.to_path_buf(), body: guide.map(StyleRules::policy_under_title) };
        rules.sync(opts).unwrap()
    }

    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn each_host_gets_its_scoped_copy_and_a_second_run_changes_nothing() {
        let d = crate::test_support::TempDir::new("style_new").unwrap();
        let first = rules(d.path(), Some(GUIDE), &plain());
        assert!(first.iter().all(|o| o.state == "missing" && o.changed), "{first:?}");
        let claude = read(d.path(), ".claude/rules/rust-style.md");
        assert!(claude.starts_with("---\npaths:\n  - \"**/*.rs\"\n---\n"), "{claude}");
        assert!(read(d.path(), ".github/instructions/rust-style.instructions.md").starts_with("---\napplyTo: \"**/*.rs\"\n---\n"));
        let policy_then_rules = claude.find(StyleRules::POLICY).zip(claude.find("SSNebula 0.1.0 checks"));
        assert!(policy_then_rules.is_some_and(|(policy, rules)| policy < rules), "{claude}");
        let second = rules(d.path(), Some(GUIDE), &plain());
        assert!(second.iter().all(|o| o.state == "current" && !o.changed), "{second:?}");
    }

    #[test]
    fn a_stale_copy_is_replaced_and_a_hand_edit_waits_for_force() {
        let d = crate::test_support::TempDir::new("style_edit").unwrap();
        let rel = ".claude/rules/rust-style.md";
        rules(d.path(), Some("# Rust code rules (SSNebula)\n\nolder text\n"), &plain());
        assert_eq!(rules(d.path(), Some(GUIDE), &plain())[0].action, "regenerated");
        let edited = read(d.path(), rel).replace("Keep it short", "Keep it long");
        std::fs::write(d.join(rel), &edited).unwrap();
        let kept = &rules(d.path(), Some(GUIDE), &plain())[0];
        assert_eq!((kept.state, kept.changed, kept.attention), ("edited", false, true));
        assert_eq!(read(d.path(), rel), edited);
        let forced = &rules(d.path(), Some(GUIDE), &Opts { force: true, ..plain() })[0];
        assert_eq!(std::fs::read_to_string(forced.backup.clone().unwrap()).unwrap(), edited);
        assert!(read(d.path(), rel).contains("Keep it short"));
    }

    #[test]
    fn check_writes_nothing() {
        let d = crate::test_support::TempDir::new("style_check").unwrap();
        let o = rules(d.path(), Some(GUIDE), &Opts { check: true, ..plain() });
        assert!(o.iter().all(|o| o.changed && o.action.starts_with("would change")), "{o:?}");
        assert!(!d.join(".claude/rules/rust-style.md").exists());
    }

    #[test]
    fn without_ssnebula_a_copy_already_there_is_left_alone_and_nothing_waits() {
        let d = crate::test_support::TempDir::new("style_none").unwrap();
        rules(d.path(), Some(GUIDE), &plain());
        let before = read(d.path(), ".claude/rules/rust-style.md");
        let o = rules(d.path(), None, &plain());
        assert!(o.iter().all(|o| o.state == "unavailable" && !o.changed && !o.attention), "{o:?}");
        assert_eq!(read(d.path(), ".claude/rules/rust-style.md"), before);
    }

    #[test]
    fn the_rules_broken_most_lately_come_before_the_full_list() {
        let guide = "# Rust code rules (SSNebula)\n\nSSNebula 0.1.0 checks these rules.\n\n## Structure\n\n**`long_function`** (warning). Keep it short.\n- Limit: 60 lines\n\n**`vague_name`** (warning). Name it well.\n";
        let rules = StyleRules { root: PathBuf::from("/w"), body: Some(StyleRules::policy_under_title(guide)) };
        let recent = [("vague_name".to_string(), 7), ("gone_lint".to_string(), 3), ("long_function".to_string(), 1)];
        let body = rules.emphasise(&recent).body.unwrap();
        let first = body.find("## Broken most often in the last 14 days").unwrap();
        assert!(body.find("SSNebula 0.1.0 checks").unwrap() < first && first < body.find("## Structure").unwrap(), "{body}");
        assert!(body.contains("- **`vague_name`** (warning). Name it well. Broken 7 times lately.\n- **`long_function`** (warning). Keep it short. Broken 1 time lately."), "{body}");
        assert!(!body.contains("gone_lint"), "a rule no longer in the guide is left out");
        let plain = StyleRules { root: PathBuf::from("/w"), body: Some(StyleRules::policy_under_title(guide)) };
        assert_eq!(plain.emphasise(&[]).body.unwrap(), StyleRules::policy_under_title(guide));
    }

    #[test]
    fn without_a_store_nothing_is_emphasised() {
        assert!(StyleRules::recent_from(Path::new("/nonexistent/memory.db")).is_empty());
    }
}
