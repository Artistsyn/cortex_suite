//! The ssnebula command, found and run. It is run rather than linked because its repository is
//! private, and a dependency on it would stop cortex building for anyone without access.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Ssnebula;

impl Ssnebula {
    /// Where ssnebula may be, most specific first: `CORTEX_SSNEBULA`, a checkout beside the
    /// projects of `root` or of any folder above it (so a repository inside the workspace finds
    /// the workspace's copy), then `PATH`.
    pub fn candidates(root: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::env::var_os("CORTEX_SSNEBULA").map(PathBuf::from).into_iter().collect();
        let builds = root.ancestors().flat_map(|dir| ["release", "debug"].map(|profile| dir.join("ssnebula/target").join(profile).join("ssnebula")));
        found.extend(builds.filter(|binary| binary.is_file()));
        found.push(PathBuf::from("ssnebula"));
        found
    }

    /// What the first build to run `args` successfully printed, if `accept` takes it. A build
    /// too old to know the subcommand fails or prints something else, and is passed over.
    pub fn output<S: AsRef<OsStr>>(root: &Path, args: &[S], accept: impl Fn(&str) -> bool) -> Option<String> {
        Self::output_with_input(root, args, None, accept)
    }

    /// `output`, with `input` written to the command's standard input.
    pub fn output_with_input<S: AsRef<OsStr>>(root: &Path, args: &[S], input: Option<&str>, accept: impl Fn(&str) -> bool) -> Option<String> {
        Self::candidates(root).iter().find_map(|binary| Self::run(binary, args, input).filter(|out| accept(out)))
    }

    fn run<S: AsRef<OsStr>>(binary: &Path, args: &[S], input: Option<&str>) -> Option<String> {
        let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
        let mut child = Command::new(binary).args(args).stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        if let (Some(text), Some(mut pipe)) = (input, child.stdin.take()) {
            pipe.write_all(text.as_bytes()).ok()?;
        }
        let output = child.wait_with_output().ok()?;
        output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_failing_build_gives_nothing() {
        let d = crate::test_support::TempDir::new("ssnebula_none").unwrap();
        assert_eq!(Ssnebula::run(&d.join("no-such-binary"), &["rules"], None), None);
        assert_eq!(Ssnebula::run(Path::new("/usr/bin/false"), &["rules"], None), None);
        assert_eq!(Ssnebula::run(Path::new("/bin/echo"), &["[]"], None).as_deref(), Some("[]\n"));
    }

    #[test]
    fn input_reaches_the_command() {
        assert_eq!(Ssnebula::run(Path::new("/bin/cat"), &[] as &[&str], Some("staged text")).as_deref(), Some("staged text"));
    }

    #[test]
    fn a_build_in_a_folder_above_is_found() {
        let d = crate::test_support::TempDir::new("ssnebula_above").unwrap();
        let binary = d.join("ssnebula/target/release/ssnebula");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();
        let repo = d.join("some_repo/crate");
        assert!(Ssnebula::candidates(&repo).contains(&binary));
    }
}
