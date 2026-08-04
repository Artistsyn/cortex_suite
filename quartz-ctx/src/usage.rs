//! Harvest real usage snippets for indexed API items.
//!
//! Signatures tell you what an API *is*; they do not show how it is called.
//! Rather than hand-write examples (the approach that made quartz-ctx
//! Quartz-specific and impossible to update without a recompile), this module
//! mines calls out of code that already exists: example programs, integration
//! tests, and `#[test]` bodies.
//!
//! Snippets are captured as whole statements using `syn` spans, so a multi-line
//! builder chain arrives intact instead of truncated at a line boundary.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use walkdir::WalkDir;

/// One harvested call site.
#[derive(Debug, Clone)]
pub struct UsageSnippet {
    pub code: String,
    /// `file:line` the snippet was taken from.
    pub location: String,
}

/// Item name → snippets showing it in use.
pub type UsageIndex = BTreeMap<String, Vec<UsageSnippet>>;

/// Most snippets to keep per item. Enough to show a pattern, few enough that a
/// generated sheet stays readable.
const MAX_PER_ITEM: usize = 3;

/// Longest snippet worth showing, in lines. Past this it is a function, not an
/// example.
const MAX_SNIPPET_LINES: usize = 14;

/// A path to mine, and how much of it counts as usage.
#[derive(Debug, Clone)]
pub struct UsageSource {
    pub path: PathBuf,
    /// True for example programs and test directories, where every statement is
    /// a caller. False for the implementation tree, where only `#[test]` bodies
    /// are — the rest is the API defining itself, not using itself.
    pub all_statements: bool,
}

/// Find the usage sources for a scanned root.
///
/// A crate's runnable examples live next to `src/`, not inside it. The source
/// tree is mined too, but only for its tests: harvesting implementation code as
/// "usage" surfaces things like a `Debug` impl body or a trait's default method
/// under the type's own documentation, which is worse than showing nothing.
pub fn discover_sources(src_root: &Path) -> Vec<UsageSource> {
    let mut out = vec![UsageSource { path: src_root.to_path_buf(), all_statements: false }];
    if let Some(crate_root) = src_root.parent() {
        for candidate in ["examples", "tests", "benches"] {
            let p = crate_root.join(candidate);
            if p.is_dir() {
                out.push(UsageSource { path: p, all_statements: true });
            }
        }
        for candidate in ["example.rs", "main.rs"] {
            let p = crate_root.join(candidate);
            if p.is_file() {
                out.push(UsageSource { path: p, all_statements: true });
            }
        }
    }
    out
}

/// Mine `sources` for statements that mention any of `names`.
pub fn harvest(sources: &[UsageSource], names: &HashSet<String>) -> UsageIndex {
    let mut index: UsageIndex = BTreeMap::new();

    for source in sources {
        let root = &source.path;
        let files: Vec<PathBuf> = if root.is_file() {
            vec![root.clone()]
        } else {
            WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| !crate::parser::is_excluded_walk_entry(e))
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |x| x == "rs"))
                .map(|e| e.path().to_path_buf())
                .collect()
        };

        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let Ok(parsed) = syn::parse_file(&text) else { continue };
            let lines: Vec<&str> = text.lines().collect();
            let label = file.to_string_lossy().replace('\\', "/");

            let mut collector = StmtCollector {
                spans: Vec::new(),
                all_statements: source.all_statements,
                in_test: false,
            };
            syn::visit::Visit::visit_file(&mut collector, &parsed);

            for (start, end) in collector.spans {
                if start == 0 || start > lines.len() || end > lines.len() || end < start {
                    continue;
                }
                let span_lines = end - start + 1;
                if span_lines > MAX_SNIPPET_LINES {
                    continue;
                }
                let snippet = dedent(&lines[start - 1..end]);
                if snippet.trim().is_empty() {
                    continue;
                }

                for name in mentioned(&snippet, names) {
                    let bucket = index.entry(name).or_default();
                    if bucket.len() >= MAX_PER_ITEM {
                        continue;
                    }
                    if bucket.iter().any(|s| s.code == snippet) {
                        continue;
                    }
                    bucket.push(UsageSnippet {
                        code: snippet.clone(),
                        location: format!("{label}:{start}"),
                    });
                }
            }
        }
    }

    index
}

/// Which known item names a snippet actually references.
///
/// Matches on word boundaries so `Camera` does not match `CameraShake`, and
/// skips names shorter than three characters, which produce noise.
fn mentioned(snippet: &str, names: &HashSet<String>) -> Vec<String> {
    let mut found = Vec::new();
    for word in snippet.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.len() < 3 {
            continue;
        }
        if names.contains(word) && !found.iter().any(|f| f == word) {
            found.push(word.to_string());
        }
    }
    found
}

/// Strip the common leading indentation so a snippet reads as standalone code.
fn dedent(lines: &[&str]) -> String {
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Collects the line span of each statement that counts as a call site.
struct StmtCollector {
    spans: Vec<(usize, usize)>,
    /// Every statement in this file is usage (example program / test dir).
    all_statements: bool,
    /// Currently inside a `#[test]` fn or `#[cfg(test)]` module.
    in_test: bool,
}

/// True when an attribute marks a test function or a test-only module.
fn is_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.to_token_stream().to_string().contains("test"))
    })
}

use quote::ToTokens;

impl<'ast> syn::visit::Visit<'ast> for StmtCollector {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let was = self.in_test;
        if is_test_attr(&node.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_fn(self, node);
        self.in_test = was;
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let was = self.in_test;
        if is_test_attr(&node.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_mod(self, node);
        self.in_test = was;
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        // A nested item (fn, struct, impl…) is a definition, not a call site.
        // Without this, a trait's default method body was offered as an example
        // of using the type in its signature.
        let is_definition = matches!(node, syn::Stmt::Item(_));

        if !is_definition && (self.all_statements || self.in_test) {
            let span = node.span();
            self.spans.push((span.start().line, span.end().line));
        }
        syn::visit::visit_stmt(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join("quartz-ctx-usage").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, src) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        }
        dir
    }

    #[test]
    fn captures_a_whole_multiline_builder_chain() {
        let dir = fixture("chain", &[("examples/demo.rs", "\
fn main() {
    let player = GameObject::build(\"player\")
        .position(10.0, 20.0)
        .size(4.0, 4.0)
        .finish();
}
")]);
        let idx = harvest(&[UsageSource { path: dir.join("examples"), all_statements: true }], &names(&["GameObject"]));
        let snips = idx.get("GameObject").expect("no snippet for GameObject");
        assert!(snips[0].code.contains(".position(10.0, 20.0)"),
                "chain was truncated: {}", snips[0].code);
        assert!(snips[0].code.contains(".finish()"), "chain lost its terminal call");
        assert!(snips[0].location.contains("demo.rs:"), "missing location");
    }

    #[test]
    fn does_not_match_a_longer_name_that_merely_contains_the_query() {
        let dir = fixture("bounds", &[("examples/d.rs",
            "fn main() { let s = CameraShake::new(); }\n")]);
        let idx = harvest(&[UsageSource { path: dir.join("examples"), all_statements: true }], &names(&["Camera", "CameraShake"]));
        assert!(idx.get("Camera").is_none(), "matched a substring of another name");
        assert!(idx.get("CameraShake").is_some());
    }

    #[test]
    fn snippets_are_dedented_and_capped_per_item() {
        let body: String = (0..10)
            .map(|i| format!("    let x{i} = Widget::new();\n"))
            .collect();
        let dir = fixture("cap", &[("examples/d.rs", &format!("fn main() {{\n{body}}}\n"))]);
        let idx = harvest(&[UsageSource { path: dir.join("examples"), all_statements: true }], &names(&["Widget"]));
        let snips = &idx["Widget"];
        assert!(snips.len() <= MAX_PER_ITEM, "cap not applied: {}", snips.len());
        assert!(!snips[0].code.starts_with(' '), "snippet not dedented: {:?}", snips[0].code);
    }

    #[test]
    fn discovery_finds_sibling_examples_and_tests() {
        let dir = fixture("discover", &[
            ("src/lib.rs", "pub struct T;\n"),
            ("examples/a.rs", "fn main() {}\n"),
            ("tests/b.rs", "#[test] fn t() {}\n"),
            ("example.rs", "fn main() {}\n"),
        ]);
        let found = discover_sources(&dir.join("src"));
        let names: Vec<String> = found
            .iter()
            .map(|u| u.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"examples".to_string()), "{names:?}");
        assert!(names.contains(&"tests".to_string()), "{names:?}");
        assert!(names.contains(&"example.rs".to_string()), "{names:?}");
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join("quartz-ctx-usage-q").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, src) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        }
        dir
    }

    /// The implementation tree must contribute only its tests. Mining it wholesale
    /// offered a `Debug` impl body and a trait's default method as examples of
    /// "using" Canvas — its own internals presented as usage.
    #[test]
    fn implementation_code_is_not_offered_as_usage() {
        let dir = fixture("impl_not_usage", &[("src/lib.rs", "\
pub struct Canvas;
impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct(\"Canvas\").finish()
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn real_usage() {
        let c = Canvas::new();
    }
}
")]);
        let idx = harvest(&discover_sources(&dir.join("src")), &names(&["Canvas"]));
        let snips = idx.get("Canvas").map(|v| v.as_slice()).unwrap_or(&[]);

        assert!(
            !snips.iter().any(|s| s.code.contains("debug_struct")),
            "implementation internals offered as usage: {:?}",
            snips.iter().map(|s| &s.code).collect::<Vec<_>>()
        );
        assert!(
            snips.iter().any(|s| s.code.contains("Canvas::new()")),
            "the #[test] body should still be harvested: {:?}",
            snips.iter().map(|s| &s.code).collect::<Vec<_>>()
        );
    }

    /// A nested definition is not a call site.
    #[test]
    fn nested_item_definitions_are_skipped() {
        let dir = fixture("defs", &[("examples/d.rs", "\
fn main() {
    fn helper(c: &Canvas) {}
    let c = Canvas::new();
}
")]);
        let idx = harvest(
            &[UsageSource { path: dir.join("examples"), all_statements: true }],
            &names(&["Canvas"]),
        );
        let snips = &idx["Canvas"];
        assert!(
            !snips.iter().any(|s| s.code.starts_with("fn helper")),
            "a nested fn definition was captured as usage"
        );
    }
}
