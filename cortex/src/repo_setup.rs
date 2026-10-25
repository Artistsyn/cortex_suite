//! What cortex needs in a git repository it works in, kept out of the repository itself. The
//! pre-commit hook lives in the hooks folder git never commits, and cortex's own files are listed
//! in `.git/info/exclude`, the ignore list that stays on this machine: a `.gitignore` entry would
//! itself be a change someone commits.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub struct RepoSetup {
    top: PathBuf,
    hooks: PathBuf,
    exclude: PathBuf,
}

impl RepoSetup {
    const HOOK_MARK: &'static str = "# installed by cortex (repo setup)";
    const EXCLUDE_MARK: &'static str = "# cortex_suite: this machine's working files, never part of the repository";
    /// Files cortex and its tools write into a project for this machine alone.
    const EXCLUDES: &'static [&'static str] = &[
        ".cortex/",
        ".agent-memory/",
        ".graphify-output/",
        ".claude/settings.local.json",
        ".claude/rules/rust-style.md",
        ".github/instructions/rust-style.instructions.md",
        "SSN_report.txt",
    ];
    /// Folders a search for repositories does not enter: build output, dependencies, and
    /// cortex's own stores, which are repositories of their own.
    const SKIPPED: &'static [&'static str] = &["target", "node_modules", ".venv", ".cortex", ".agent-memory", ".claude"];

    /// The repository holding `path`, or `None` outside one.
    pub fn of(path: &Path) -> Option<Self> {
        let dir = if path.is_dir() { path } else { path.parent()? };
        let output = Command::new("git").arg("-C").arg(dir)
            .args(["rev-parse", "--show-toplevel", "--git-path", "hooks", "--git-path", "info/exclude"])
            .output().ok()?;
        let text = String::from_utf8(output.stdout).ok().filter(|_| output.status.success())?;
        let mut lines = text.lines().map(PathBuf::from);
        let top = lines.next()?;
        let absolute = |p: PathBuf| if p.is_absolute() { p } else { dir.join(p) };
        Some(Self { hooks: absolute(lines.next()?), exclude: absolute(lines.next()?), top })
    }

    /// Starting to serve a folder: set up its repository and bring its Rust code rules up to
    /// date. `CORTEX_NO_AUTO_HOOKS` turns this off with cortex's other automatic setup.
    pub fn on_serve(root: &Path) {
        if std::env::var_os("CORTEX_NO_AUTO_HOOKS").is_some() {
            return;
        }
        let rules = crate::style_rules::StyleRules::load(root).sync(&crate::instructions::Opts { check: false, force: false, adopt: false });
        let written = rules.iter().flatten().filter(|o| o.changed).map(|o| format!("{}: {}", o.file, o.action));
        for line in Self::after_edit(root).into_iter().chain(written) {
            eprintln!("  setup: {line}");
        }
    }

    /// Set up the repository holding `path` unless it already is, without asking git when the
    /// files show it is: this runs after every edit.
    pub fn after_edit(path: &Path) -> Vec<String> {
        if Self::looks_ready(path) {
            return Vec::new();
        }
        Self::of(path).map(|setup| setup.ensure()).unwrap_or_default()
    }

    /// Whether the plain `.git` folder above `path` already has the exclude block and a pre-commit
    /// hook (cortex's or the repository's own, which is left alone).
    fn looks_ready(path: &Path) -> bool {
        let Some(git) = path.ancestors().map(|dir| dir.join(".git")).find(|git| git.exists()) else { return true };
        if !git.is_dir() {
            return false;
        }
        let exclude = std::fs::read_to_string(git.join("info/exclude")).unwrap_or_default();
        Self::missing_excludes(&exclude).is_empty() && git.join("hooks/pre-commit").is_file()
    }

    /// Bring the repository up to date, saying what changed; failures are reported, not raised.
    pub fn ensure(&self) -> Vec<String> {
        [self.ensure_excludes(), self.ensure_hook()]
            .into_iter()
            .filter_map(|step| step.unwrap_or_else(|e| Some(format!("{}: {e:#}", self.top.display()))))
            .collect()
    }

    fn missing_excludes(text: &str) -> Vec<&'static str> {
        let present: Vec<&str> = text.lines().map(str::trim).collect();
        Self::EXCLUDES.iter().copied().filter(|entry| !present.contains(entry)).collect()
    }

    fn ensure_excludes(&self) -> Result<Option<String>> {
        let text = std::fs::read_to_string(&self.exclude).unwrap_or_default();
        let missing = Self::missing_excludes(&text);
        if missing.is_empty() {
            return Ok(None);
        }
        let mut lines = vec![text.trim_end().to_owned()];
        if !text.contains(Self::EXCLUDE_MARK) {
            lines.extend([String::new(), Self::EXCLUDE_MARK.to_owned()]);
        }
        lines.extend(missing.iter().map(|entry| (*entry).to_owned()));
        let updated = format!("{}\n", lines.join("\n").trim_start());
        std::fs::create_dir_all(self.exclude.parent().unwrap_or(&self.top))?;
        std::fs::write(&self.exclude, updated).with_context(|| format!("writing {}", self.exclude.display()))?;
        Ok(Some(format!("{}: {} of cortex's files kept out of git (.git/info/exclude)", self.top.display(), missing.len())))
    }

    fn ensure_hook(&self) -> Result<Option<String>> {
        let path = self.hooks.join("pre-commit");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let cortex = std::env::current_exe().context("finding the cortex binary")?;
        let script = Self::script(&cortex);
        if existing == script || (!existing.is_empty() && !Self::is_ours(&existing)) {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.hooks)?;
        std::fs::write(&path, &script).with_context(|| format!("writing {}", path.display()))?;
        Self::make_executable(&path)?;
        Ok(Some(format!("{}: pre-commit hook refuses commits adding error-level ssnebula findings", self.top.display())))
    }

    /// A hook cortex wrote, under this mark or the one `commit-check --install` used first.
    fn is_ours(script: &str) -> bool {
        script.contains(Self::HOOK_MARK) || script.contains("# installed by `cortex commit-check --install`")
    }

    /// A hook that is not cortex's is left alone; this says what to add to it.
    pub fn foreign_hook_note(&self) -> Option<String> {
        let path = self.hooks.join("pre-commit");
        let existing = std::fs::read_to_string(&path).ok().filter(|text| !Self::is_ours(text))?;
        let cortex = std::env::current_exe().ok()?;
        (!existing.contains("commit-check")).then(|| {
            format!("{} is the repository's own; add `\"{}\" commit-check || exit 1` to it", path.display(), cortex.display())
        })
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?)
    }

    /// Git for Windows runs hooks through its own sh, which needs no mode bit.
    #[cfg(not(unix))]
    fn make_executable(_path: &Path) -> Result<()> {
        Ok(())
    }

    fn script(cortex: &Path) -> String {
        format!(
            "#!/bin/sh\n{}: refuses a commit whose added Rust lines break an\n\
             # error-level ssnebula rule. `git commit --no-verify` skips it.\n\
             CORTEX=\"{}\"\n[ -x \"$CORTEX\" ] || exit 0\nexec \"$CORTEX\" commit-check\n",
            Self::HOOK_MARK,
            cortex.display()
        )
    }

    /// Every repository at or below `root`, outside build output and cortex's own stores.
    pub fn all_under(root: &Path) -> Vec<PathBuf> {
        let walk = walkdir::WalkDir::new(root).max_depth(6).into_iter().filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            entry.depth() == 0 || !Self::SKIPPED.contains(&name.as_ref())
        });
        walk.flatten().filter(|entry| entry.file_name() == ".git").filter_map(|entry| entry.path().parent().map(Path::to_path_buf)).collect()
    }

    /// The `cortex repo-setup` command: the repository holding `dir`, or with `all` every one below it.
    pub fn main(dir: Option<PathBuf>, all: bool) -> Result<()> {
        let dir = dir.unwrap_or_else(|| PathBuf::from("."));
        let repos = if all { Self::all_under(&dir) } else { vec![dir.clone()] };
        if repos.is_empty() {
            bail!("no git repository at or below {}", dir.display());
        }
        for repo in repos {
            let Some(setup) = Self::of(&repo) else { continue };
            let done = setup.ensure();
            println!("{}: {}", setup.top.display(), if done.is_empty() { "already set up".to_owned() } else { format!("{} change(s)", done.len()) });
            if let Some(note) = setup.foreign_hook_note() {
                println!("  {note}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str) -> crate::test_support::TempDir {
        let d = crate::test_support::TempDir::new(tag).unwrap();
        assert!(Command::new("git").arg("-C").arg(d.path()).args(["init", "-q"]).status().unwrap().success());
        d
    }

    #[test]
    fn setup_keeps_cortex_files_out_of_git_and_installs_the_hook_once() {
        let d = repo("setup_new");
        let setup = RepoSetup::of(d.path()).unwrap();
        assert_eq!(setup.ensure().len(), 2);
        assert!(setup.ensure().is_empty(), "a second run changes nothing");
        std::fs::create_dir_all(d.join(".claude/rules")).unwrap();
        std::fs::write(d.join(".claude/rules/rust-style.md"), "x").unwrap();
        std::fs::write(d.join("SSN_report.txt"), "x").unwrap();
        let status = Command::new("git").arg("-C").arg(d.path()).args(["status", "--porcelain"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&status.stdout), "", "nothing cortex wrote shows up to commit");
        assert!(RepoSetup::looks_ready(&d.join("src/lib.rs")));
        assert!(std::fs::read_to_string(d.join(".git/hooks/pre-commit")).unwrap().contains(RepoSetup::HOOK_MARK));
    }

    #[test]
    fn a_repositorys_own_hook_and_excludes_are_kept() {
        let d = repo("setup_own");
        std::fs::write(d.join(".git/hooks/pre-commit"), "#!/bin/sh\nrun-our-checks\n").unwrap();
        std::fs::write(d.join(".git/info/exclude"), "local-notes/\n").unwrap();
        RepoSetup::of(d.path()).unwrap().ensure();
        assert_eq!(std::fs::read_to_string(d.join(".git/hooks/pre-commit")).unwrap(), "#!/bin/sh\nrun-our-checks\n");
        let exclude = std::fs::read_to_string(d.join(".git/info/exclude")).unwrap();
        assert!(exclude.starts_with("local-notes/\n\n# cortex_suite") && exclude.contains(".cortex/\n"), "{exclude}");
        assert!(RepoSetup::of(d.path()).unwrap().foreign_hook_note().is_some());
    }

    #[test]
    fn outside_a_repository_there_is_nothing_to_do() {
        let d = crate::test_support::TempDir::new("setup_none").unwrap();
        assert!(RepoSetup::looks_ready(&d.join("a.rs")) || RepoSetup::of(d.path()).is_none());
    }

    #[test]
    fn the_search_skips_build_output_and_cortex_stores() {
        let d = crate::test_support::TempDir::new("setup_all").unwrap();
        for dir in ["a/.git", "a/target/x/.git", "b/c/.git", ".cortex/.git"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        let mut found = RepoSetup::all_under(d.path());
        found.sort();
        assert_eq!(found, vec![d.join("a"), d.join("b/c")]);
    }
}
