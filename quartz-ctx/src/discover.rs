//! Find the crates under a directory, so a whole workspace can be served
//! without listing each `src` by hand.
//!
//! Handles both layouts that occur in practice: a real Cargo workspace whose
//! members are subdirectories, and a plain directory holding several standalone
//! crates side by side. Both look the same from here — a `Cargo.toml` next to a
//! `src/` — so neither needs special casing, and no TOML parser is required.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// One discovered crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCrate {
    /// The crate's `src` directory — what actually gets parsed.
    pub src: PathBuf,
    /// Package name from `Cargo.toml`, falling back to the directory name.
    pub name: String,
    /// Identifier-safe form of `name`, used as the origin tag / scope.
    pub scope: String,
}

/// How deep to look for crates. Deep enough for `workspace/crates/foo`, shallow
/// enough not to wander into unrelated vendored trees.
const MAX_DEPTH: usize = 4;

/// Find every crate under `root`, sorted for a stable, reproducible order.
///
/// A directory is a crate when it holds both `Cargo.toml` and `src/`. Build
/// output and dependency caches are pruned by the same rules the parser uses, so
/// a vendored copy of a crate inside `target/` is never mistaken for a member.
pub fn discover_crates(root: &Path) -> Vec<DiscoveredCrate> {
    let mut found = Vec::new();

    for entry in WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| !crate::parser::is_excluded_walk_entry(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name() == "Cargo.toml")
    {
        let manifest = entry.path();
        let Some(dir) = manifest.parent() else { continue };
        let src = dir.join("src");
        if !src.is_dir() {
            // A virtual workspace manifest has no src of its own; its members
            // are found on their own.
            continue;
        }

        let name = package_name(manifest)
            .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "crate".to_string());
        let scope = sanitise_scope(&name);

        found.push(DiscoveredCrate { src, name, scope });
    }

    found.sort_by(|a, b| a.src.cmp(&b.src));
    found.dedup_by(|a, b| a.src == b.src);
    found
}

/// Read `name = "..."` from the `[package]` section.
///
/// A line scan rather than a TOML dependency: the field is the only thing needed
/// and the format is unambiguous, so the extra crate would not earn its place.
/// Stops at the next section header so a `name` under `[dependencies]` or
/// `[[bin]]` is never mistaken for the package name.
fn package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_package = false;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let value = rest.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Make a crate name usable as a scope: lowercase, `-` to `_`.
///
/// Kept identifier-safe on purpose — a scope is prefixed onto module paths and
/// must round-trip as an identifier, which is why `-` becomes `_` rather than
/// the other way around.
fn sanitise_scope(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join("quartz-ctx-discover").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    const MANIFEST: &str = "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n";

    #[test]
    fn finds_standalone_crates_side_by_side() {
        let dir = fixture("standalone", &[
            ("engine/Cargo.toml", "[package]\nname = \"engine\"\n"),
            ("engine/src/lib.rs", "pub struct A;\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
            ("app/src/main.rs", "fn main() {}\n"),
        ]);
        let found = discover_crates(&dir);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["app", "engine"], "expected both crates, sorted");
    }

    #[test]
    fn finds_workspace_members_and_skips_the_virtual_root() {
        let dir = fixture("workspace", &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
            ("crates/core/src/lib.rs", "pub struct A;\n"),
        ]);
        let found = discover_crates(&dir);
        assert_eq!(found.len(), 1, "virtual root has no src and must be skipped");
        assert_eq!(found[0].name, "core");
    }

    #[test]
    fn build_output_is_never_mistaken_for_a_crate() {
        let dir = fixture("vendored", &[
            ("Cargo.toml", "[package]\nname = \"real\"\n"),
            ("src/lib.rs", "pub struct A;\n"),
            ("target/debug/build/dep-1/Cargo.toml", "[package]\nname = \"dep\"\n"),
            ("target/debug/build/dep-1/src/lib.rs", "pub struct Junk;\n"),
        ]);
        let found = discover_crates(&dir);
        assert_eq!(found.len(), 1, "picked up a crate from target/: {found:?}");
        assert_eq!(found[0].name, "real");
    }

    #[test]
    fn a_hyphenated_package_name_becomes_an_identifier_safe_scope() {
        let dir = fixture("scope", &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub struct A;\n"),
        ]);
        let found = discover_crates(&dir);
        assert_eq!(found[0].name, "my-crate");
        assert_eq!(found[0].scope, "my_crate", "scope must round-trip as an identifier");
    }

    /// A `name` under another section must not be read as the package name.
    #[test]
    fn only_the_package_section_supplies_the_name() {
        let dir = fixture("sections", &[
            ("Cargo.toml", "[package]\nname = \"right\"\n\n[[bin]]\nname = \"wrong\"\n"),
            ("src/lib.rs", "pub struct A;\n"),
        ]);
        assert_eq!(discover_crates(&dir)[0].name, "right");
    }

    #[test]
    fn a_manifest_without_a_name_falls_back_to_the_directory() {
        let dir = fixture("noname", &[
            ("thing/Cargo.toml", "[dependencies]\nserde = \"1\"\n"),
            ("thing/src/lib.rs", "pub struct A;\n"),
        ]);
        assert_eq!(discover_crates(&dir)[0].name, "thing");
    }
}
