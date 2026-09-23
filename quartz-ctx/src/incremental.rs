//! A parsed workspace that stays current by re-reading only what changed.
//!
//! The server used to re-parse every root whenever a throttled fingerprint over
//! the whole tree moved, and the fingerprint had three holes that each served a
//! stale answer with full confidence:
//!
//! * it was checked at most every 5 s, so the query an agent makes right after
//!   an edit - the one that most needs the edit - got the previous parse;
//! * it hashed mtimes in whole seconds, so a second edit landing in the same
//!   second as a reload was never seen at all, until some other file changed;
//! * it walked `node_modules`, `venv` and `target`, which the parser prunes,
//!   spending ~100 ms per check on 27k files to watch 501.
//!
//! This keeps one `FileParse` per source file, stamped with its size and
//! nanosecond mtime. `refresh` re-stats the pruned tree (the same walk the
//! parser does), re-parses only files whose stamp moved, and re-runs the
//! cross-file resolution for the roots that changed. It is cheap enough to run
//! before EVERY answer, which is the point: freshness becomes a property checked
//! at the moment of answering instead of a side effect of a timer.
//!
//! A stamp can still lie on a filesystem with coarse timestamps: two writes of
//! the same length inside one tick look identical. Borrowing git's answer to the
//! same problem, a file modified within `RACY_WINDOW` of when it was read is
//! "racily clean" and is re-hashed on the next refresh even if its stamp has not
//! moved. That costs a read of files edited in the last couple of seconds and
//! nothing else.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::model::ApiItem;
use crate::parser::{self, FileParse, ParseOptions};

/// A file modified this close to when it was read may have been written again
/// without its stamp moving. Generous against every filesystem in use (APFS and
/// ext4 are nanosecond, FAT is 2 s).
const RACY_WINDOW: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    mtime_ns: u128,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Stamp { len: meta.len(), mtime_ns }
    }

    fn racy(&self, read_at: SystemTime) -> bool {
        let read_ns = read_at.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        self.mtime_ns + RACY_WINDOW.as_nanos() > read_ns
    }
}

struct FileState {
    stamp: Stamp,
    hash: u64,
    read_at: SystemTime,
    parse: FileParse,
}

fn content_hash(content: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

/// One configured source root and everything parsed from it.
pub struct Root {
    pub dir: PathBuf,
    pub tag: String,
    pub include_private: bool,
    files: BTreeMap<PathBuf, FileState>,
    items: Vec<ApiItem>,
}

impl Root {
    fn opts(&self) -> ParseOptions {
        ParseOptions { include_private: self.include_private }
    }

    fn load(dir: PathBuf, tag: String, include_private: bool) -> Self {
        let mut root = Root { dir, tag, include_private, files: BTreeMap::new(), items: Vec::new() };
        root.sync(&mut RefreshReport::default(), true);
        root
    }

    /// Bring this root in line with the disk. Returns whether anything changed.
    fn sync(&mut self, report: &mut RefreshReport, initial: bool) -> bool {
        let opts = self.opts();
        let on_disk = parser::source_files(&self.dir);
        report.scanned += on_disk.len();
        let mut changed = false;

        let mut seen: std::collections::HashSet<&Path> = std::collections::HashSet::new();
        for path in &on_disk {
            seen.insert(path.as_path());
            let Ok(meta) = std::fs::metadata(path) else { continue };
            let stamp = Stamp::of(&meta);
            let existed = self.files.contains_key(path);
            let must_read = match self.files.get(path) {
                None => true,
                Some(f) => f.stamp != stamp || f.stamp.racy(f.read_at),
            };
            if !must_read {
                continue;
            }
            let read_at = SystemTime::now();
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warn: could not read {}: {}", path.display(), e);
                    continue;
                }
            };
            let hash = content_hash(&content);
            if let Some(f) = self.files.get_mut(path) {
                if f.hash == hash {
                    // Touched, or racily clean and in fact unchanged.
                    f.stamp = stamp;
                    f.read_at = read_at;
                    continue;
                }
            }
            let parse = parser::parse_source_file(&self.dir, path, &content, opts);
            if let Some(err) = &parse.error {
                eprintln!("warn: could not parse {}: {err}", self.dir.display());
            }
            if !initial {
                if existed {
                    report.files_changed.push(path.clone());
                } else {
                    report.files_added.push(path.clone());
                }
            }
            self.files.insert(path.clone(), FileState { stamp, hash, read_at, parse });
            changed = true;
        }

        let gone: Vec<PathBuf> =
            self.files.keys().filter(|p| !seen.contains(p.as_path())).cloned().collect();
        for p in gone {
            self.files.remove(&p);
            report.files_removed.push(p);
            changed = true;
        }

        if changed || (self.items.is_empty() && !self.files.is_empty()) {
            let mut items = parser::assemble(self.files.values().map(|f| &f.parse));
            for item in &mut items {
                item.origin = self.tag.clone();
            }
            if !initial {
                report.api_changes.extend(diff_items(&self.items, &items));
            }
            self.items = items;
        }
        changed
    }
}

/// What one `refresh` found.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RefreshReport {
    pub files_changed: Vec<PathBuf>,
    pub files_added: Vec<PathBuf>,
    pub files_removed: Vec<PathBuf>,
    pub api_changes: Vec<ApiChange>,
    /// Source files stat'ed.
    pub scanned: usize,
    #[serde(skip)]
    pub elapsed: Duration,
}

impl RefreshReport {
    pub fn touched_files(&self) -> usize {
        self.files_changed.len() + self.files_added.len() + self.files_removed.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Changed => "changed",
        }
    }
}

/// One API-level difference between two parses of a root.
#[derive(Debug, Clone, Serialize)]
pub struct ApiChange {
    pub change: ChangeKind,
    pub origin: String,
    pub kind: String,
    /// `module::path::Name`, or the bare name at the root module.
    pub path: String,
    pub name: String,
    /// `file:line` of the item as it now stands (or last stood, if removed).
    pub location: Option<String>,
    /// What moved, for a `Changed`: e.g. `signature`, `+method foo`, `-field bar`.
    pub detail: Vec<String>,
}

fn item_key(i: &ApiItem) -> (String, String, String) {
    (i.kind.label().to_string(), i.module_path.join("::"), i.name.clone())
}

/// The API-relevant shape of an item, as a list of `facet` lines.
///
/// Docs and spans are deliberately absent: moving a function down a file or
/// rewording its comment is not an API change, and reporting it as one would
/// bury the changes that are.
fn facets(i: &ApiItem) -> Vec<String> {
    let mut out = vec![
        format!("signature {}", i.signature),
        format!("generics {}", i.generics),
        format!("visibility {:?}", i.visibility),
    ];
    for m in &i.methods {
        out.push(format!("method {} = {}", m.name, m.signature));
    }
    for f in &i.fields {
        out.push(format!("field {}: {}", f.name, f.ty));
    }
    for v in &i.variants {
        out.push(format!("variant {}{}", v.name, v.fields_inline()));
    }
    for t in &i.traits_impl {
        out.push(format!("impl {t}"));
    }
    out
}

/// `+name`, `-name`, `~name` for each facet added, removed or reshaped. Facets are
/// `category rest` lines; singleton categories (signature, generics, visibility)
/// are keyed by category alone. Shared with cortex so both servers describe a
/// change in the same words.
pub fn describe_facet_delta(before: &[String], after: &[String]) -> Vec<String> {
    // "method foo = fn foo(..)" -> "method foo"; "field x: T" -> "field x";
    // "signature ..." -> "signature". Singleton facets are keyed by category
    // alone, so a reshaped signature reads as one `~signature`, not a removal
    // plus an addition of two differently-truncated strings.
    let name_of = |f: &str| -> String {
        let (cat, rest) = f.split_once(' ').unwrap_or((f, ""));
        match cat {
            "signature" | "generics" | "visibility" => cat.to_string(),
            _ => {
                let ident: String =
                    rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                format!("{cat} {ident}")
            }
        }
    };
    let b: HashMap<String, &String> = before.iter().map(|f| (name_of(f), f)).collect();
    let a: HashMap<String, &String> = after.iter().map(|f| (name_of(f), f)).collect();
    let mut out = Vec::new();
    for (k, v) in &a {
        match b.get(k) {
            None => out.push(format!("+{k}")),
            Some(old) if old != v => out.push(format!("~{k}")),
            _ => {}
        }
    }
    for k in b.keys() {
        if !a.contains_key(k) {
            out.push(format!("-{k}"));
        }
    }
    out.sort();
    out
}

fn location(i: &ApiItem) -> Option<String> {
    i.span.as_ref().map(|s| format!("{}:{}", s.file, s.line))
}

/// Items added, removed or reshaped between two parses of the same root.
pub fn diff_items(before: &[ApiItem], after: &[ApiItem]) -> Vec<ApiChange> {
    let index = |items: &[ApiItem]| -> HashMap<(String, String, String), usize> {
        let mut m = HashMap::new();
        for (n, i) in items.iter().enumerate() {
            m.entry(item_key(i)).or_insert(n);
        }
        m
    };
    let b = index(before);
    let a = index(after);
    let change = |c: ChangeKind, i: &ApiItem, detail: Vec<String>| ApiChange {
        change: c,
        origin: i.origin.clone(),
        kind: i.kind.label().to_string(),
        path: if i.module_path.is_empty() {
            i.name.clone()
        } else {
            format!("{}::{}", i.module_path.join("::"), i.name)
        },
        name: i.name.clone(),
        location: location(i),
        detail,
    };

    let mut out = Vec::new();
    for (key, &ai) in &a {
        let item = &after[ai];
        match b.get(key) {
            None => out.push(change(ChangeKind::Added, item, Vec::new())),
            Some(&bi) => {
                let fb = facets(&before[bi]);
                let fa = facets(item);
                if fb != fa {
                    out.push(change(ChangeKind::Changed, item, describe_facet_delta(&fb, &fa)));
                }
            }
        }
    }
    for (key, &bi) in &b {
        if !a.contains_key(key) {
            out.push(change(ChangeKind::Removed, &before[bi], Vec::new()));
        }
    }
    out.sort_by(|x, y| (x.origin.as_str(), x.path.as_str()).cmp(&(y.origin.as_str(), y.path.as_str())));
    out
}

/// Every configured root, parsed and kept current.
pub struct Workspace {
    roots: Vec<Root>,
    cached: Vec<ApiItem>,
}

impl Workspace {
    /// Parse every root cold. Same result as `parser::load_sources_with`.
    pub fn load(sources: &[(PathBuf, String, bool)]) -> Self {
        let roots: Vec<Root> = sources
            .iter()
            .map(|(p, t, ip)| Root::load(p.clone(), t.clone(), *ip))
            .collect();
        let mut ws = Workspace { roots, cached: Vec::new() };
        ws.rebuild_cache();
        for r in &ws.roots {
            parser::report_parse_outcomes(r.files.values().map(|f| &f.parse));
        }
        ws
    }

    fn rebuild_cache(&mut self) {
        self.cached = self.roots.iter().flat_map(|r| r.items.iter().cloned()).collect();
    }

    /// Re-stat every root and re-parse whatever moved.
    pub fn refresh(&mut self) -> RefreshReport {
        let t = Instant::now();
        let mut report = RefreshReport::default();
        let mut any = false;
        for root in &mut self.roots {
            any |= root.sync(&mut report, false);
        }
        if any {
            self.rebuild_cache();
        }
        report.elapsed = t.elapsed();
        report
    }

    /// Items of every root, in root order (the first root is primary).
    pub fn items(&self) -> &[ApiItem] {
        &self.cached
    }

    /// Items of one root.
    pub fn root_items(&self, dir: &Path) -> Option<&[ApiItem]> {
        self.roots.iter().find(|r| r.dir == dir).map(|r| r.items.as_slice())
    }

    pub fn roots(&self) -> impl Iterator<Item = &Root> {
        self.roots.iter()
    }

    /// Files that currently fail to parse, as `root/file:line:col: message`.
    /// Their items are served from nothing, so every answer that could involve
    /// them should say so.
    pub fn parse_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.roots {
            for f in r.files.values() {
                if let Some(e) = &f.parse.error {
                    out.push(format!("{}/{}", r.dir.display(), e));
                }
            }
        }
        out
    }

    pub fn file_count(&self) -> usize {
        self.roots.iter().map(|r| r.files.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qctx_incr_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(ws: &Workspace) -> Vec<String> {
        let mut v: Vec<String> = ws.items().iter().map(|i| i.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_rename_is_visible_on_the_very_next_refresh() {
        let d = scratch("rename");
        std::fs::write(d.join("lib.rs"), "pub struct Alpha { pub x: i32 }\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        assert_eq!(names(&ws), ["Alpha"]);
        std::fs::write(d.join("lib.rs"), "pub struct Gamma { pub x: i32 }\n").unwrap();
        let r = ws.refresh();
        assert_eq!(names(&ws), ["Gamma"]);
        let kinds: Vec<_> = r.api_changes.iter().map(|c| (c.change, c.name.as_str())).collect();
        assert!(kinds.contains(&(ChangeKind::Added, "Gamma")));
        assert!(kinds.contains(&(ChangeKind::Removed, "Alpha")));
    }

    /// The old fingerprint hashed whole seconds, so an edit in the same second
    /// as the previous reload was lost for good.
    #[test]
    fn two_edits_inside_one_second_are_both_seen() {
        let d = scratch("samesec");
        let f = d.join("lib.rs");
        std::fs::write(&f, "pub struct Delta;\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        std::fs::write(&f, "pub struct Epsln;\n").unwrap();
        ws.refresh();
        std::fs::write(&f, "pub struct Zetaa;\n").unwrap(); // same length, same second
        ws.refresh();
        assert_eq!(names(&ws), ["Zetaa"]);
    }

    /// A write that leaves size AND mtime unchanged (coarse filesystems, or a
    /// tool that restores mtime) is still caught while the file is racy.
    #[test]
    fn an_unchanged_stamp_on_a_racy_file_is_rehashed() {
        let d = scratch("racy");
        let f = d.join("lib.rs");
        std::fs::write(&f, "pub struct Aaaa;\n").unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        std::fs::write(&f, "pub struct Bbbb;\n").unwrap();
        let file = std::fs::File::options().write(true).open(&f).unwrap();
        file.set_modified(mtime).unwrap();
        ws.refresh();
        assert_eq!(names(&ws), ["Bbbb"]);
    }

    #[test]
    fn a_deleted_file_takes_its_items_with_it() {
        let d = scratch("delete");
        std::fs::write(d.join("a.rs"), "pub struct A;\n").unwrap();
        std::fs::write(d.join("b.rs"), "pub struct B;\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        std::fs::remove_file(d.join("b.rs")).unwrap();
        let r = ws.refresh();
        assert_eq!(names(&ws), ["A"]);
        assert_eq!(r.files_removed.len(), 1);
    }

    #[test]
    fn an_impl_in_another_file_still_attaches_after_its_owner_is_reparsed() {
        let d = scratch("impl");
        std::fs::write(d.join("a.rs"), "pub struct Canvas;\n").unwrap();
        std::fs::write(d.join("b.rs"), "impl Canvas { pub fn draw(&self) {} }\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        std::fs::write(d.join("a.rs"), "pub struct Canvas { pub w: u32 }\n").unwrap();
        let r = ws.refresh();
        let c = ws.items().iter().find(|i| i.name == "Canvas").unwrap();
        assert_eq!(c.methods.len(), 1, "cross-file impl lost on incremental rebuild");
        let ch = r.api_changes.iter().find(|c| c.name == "Canvas").unwrap();
        assert_eq!(ch.change, ChangeKind::Changed);
        assert_eq!(ch.detail, ["+field w", "~signature"]);
    }

    #[test]
    fn a_touch_without_a_content_change_reparses_nothing() {
        let d = scratch("touch");
        let f = d.join("lib.rs");
        std::fs::write(&f, "pub struct A;\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        let file = std::fs::File::options().write(true).open(&f).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
        let r = ws.refresh();
        assert_eq!(r.touched_files(), 0);
        assert!(r.api_changes.is_empty());
    }

    #[test]
    fn a_parse_error_is_reported_and_clears_when_fixed() {
        let d = scratch("err");
        let f = d.join("lib.rs");
        std::fs::write(&f, "pub struct A { x: }\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        assert_eq!(ws.parse_errors().len(), 1);
        assert!(ws.parse_errors()[0].contains("lib.rs:1:"), "{:?}", ws.parse_errors());
        std::fs::write(&f, "pub struct A { pub x: u8 }\n").unwrap();
        ws.refresh();
        assert!(ws.parse_errors().is_empty());
        assert_eq!(names(&ws), ["A"]);
    }

    #[test]
    fn build_output_is_never_walked() {
        let d = scratch("prune");
        std::fs::write(d.join("lib.rs"), "pub struct A;\n").unwrap();
        std::fs::create_dir_all(d.join("node_modules/x")).unwrap();
        std::fs::write(d.join("node_modules/x/index.js"), "export function f() {}\n").unwrap();
        let mut ws = Workspace::load(&[(d.clone(), "t".into(), false)]);
        std::fs::write(d.join("node_modules/x/index.js"), "export function g() {}\n").unwrap();
        let r = ws.refresh();
        assert_eq!(r.scanned, 1);
        assert_eq!(r.touched_files(), 0);
    }
}
