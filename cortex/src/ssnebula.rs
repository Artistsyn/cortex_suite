//! The ssnebula command, found and run. It is run rather than linked because its repository is
//! private, and a dependency on it would stop cortex building for anyone without access.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Ssnebula;

impl Ssnebula {
    /// Where ssnebula may be, most specific first: `CORTEX_SSNEBULA`, a checkout beside the
    /// workspace's other projects, then `PATH`.
    fn candidates(root: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::env::var_os("CORTEX_SSNEBULA").map(PathBuf::from).into_iter().collect();
        found.extend(["release", "debug"].map(|profile| root.join("ssnebula/target").join(profile).join("ssnebula")));
        found.push(PathBuf::from("ssnebula"));
        found
    }

    /// What the first build to run `args` successfully printed, if `accept` takes it. A build
    /// too old to know the subcommand fails or prints something else, and is passed over.
    pub fn output<S: AsRef<OsStr>>(root: &Path, args: &[S], accept: impl Fn(&str) -> bool) -> Option<String> {
        Self::candidates(root).iter().find_map(|binary| Self::run(binary, args).filter(|out| accept(out)))
    }

    fn run<S: AsRef<OsStr>>(binary: &Path, args: &[S]) -> Option<String> {
        let output = Command::new(binary).args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_failing_build_gives_nothing() {
        let d = crate::test_support::TempDir::new("ssnebula_none").unwrap();
        assert_eq!(Ssnebula::run(&d.join("no-such-binary"), &["rules"]), None);
        assert_eq!(Ssnebula::run(Path::new("/usr/bin/false"), &["rules"]), None);
        assert_eq!(Ssnebula::run(Path::new("/bin/echo"), &["[]"]).as_deref(), Some("[]\n"));
    }
}
