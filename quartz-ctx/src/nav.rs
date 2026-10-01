//! Code navigation that replaces the grep -> sed pair.
//!
//! Measured on this workspace (14 days to 2026-09-30): agents ran 4,889 `sed`/`cat`
//! reads of Rust files and 3,088 Rust greps, and 2,492 of those reads came straight
//! after a grep that only located the code. Each of those pairs is two requests,
//! and every request re-reads the whole conversation. The API index answered
//! neither need well: it serves signatures, not bodies, and it leaves out private
//! items, which is most of the code an agent edits.
//!
//! So this module answers from the files on disk at call time - as current as
//! `sed`, and blind to nothing the compiler can see:
//!
//! * [`Nav::get_source`]: a definition's exact source with line numbers, found by
//!   name (`Type::method` too, several names as `a|b`), in one call.
//! * [`Nav::find_references`]: every use of a name, grouped by file and by the
//!   function it sits in. Comments and strings are left out unless asked for,
//!   which removes grep's substring and comment noise; asking for them keeps the
//!   one job grep does better (a rename must touch comments and strings).
//! * [`Nav::outline`]: a file's items with line ranges, instead of reading it.
//!
//! The output is organised by file, then by enclosing item, because flat snippet
//! lists localise worse than file-centred ones (RepoNav, EMNLP 2026), and it is
//! budgeted, because a symbol tool that costs more tokens than grep is used *in
//! addition to* grep rather than instead of it ("Does a Language Server Save
//! Tokens for Coding Agents?", 2026).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use syn::spanned::Spanned;
use tree_sitter::{Node, Parser as TsParser};

use crate::lang::Language;

/// Default number of lines shown for one definition before it is cut.
pub const DEFAULT_MAX_LINES: usize = 150;
/// Total lines one `get_source` answer may carry across all its definitions.
const TOTAL_LINES: usize = 400;
/// Default number of uses one `find_references` answer lists.
pub const DEFAULT_REF_LIMIT: usize = 80;
/// A single source line longer than this is cut (minified files).
const LINE_CAP: usize = 400;
/// Text shown for one use in a reference list.
const USE_TEXT_CAP: usize = 100;

/// A definition found in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    /// The bare name: `draw`.
    pub name: String,
    /// `Canvas::draw` for a method, the bare name otherwise. An impl block is
    /// `impl Canvas` or `impl Draw for Canvas`.
    pub qual: String,
    pub kind: &'static str,
    /// First line, including doc comments and attributes.
    pub start: usize,
    /// The line carrying the name.
    pub name_line: usize,
    pub end: usize,
    /// Where it sits, when that is not obvious: the Rust item holding a
    /// shader, or the file a `mod x;` loads.
    pub ctx: String,
}

/// One source file under a root.
#[derive(Debug, Clone)]
struct SrcFile {
    path: PathBuf,
    origin: String,
}

/// identifier -> lines where it is used as code.
type IdentMap = HashMap<String, Vec<u32>>;

#[derive(Default)]
pub struct Nav {
    /// path -> (len, mtime, text). A file is re-read only when either moves.
    cache: HashMap<PathBuf, (u64, Option<SystemTime>, Arc<String>)>,
    /// path -> definitions, valid for the text it was computed from.
    defs: HashMap<PathBuf, (Arc<String>, Arc<Result<Vec<Def>, String>>)>,
    /// path -> identifier uses, valid for the text it was computed from; `None`
    /// when the file does not tokenize. Each file version is tokenized once:
    /// re-tokenizing per call grew proc-macro2's thread-local source map past
    /// its 32-bit offsets in a few hundred calls and panicked the server.
    idents: HashMap<PathBuf, (Arc<String>, Arc<Option<IdentMap>>)>,
}

/// The directories a navigation call walks: every configured root and, for a
/// Rust crate's `src`, the rest of the crate's own code - `examples`, `tests`,
/// `benches`, `build.rs` - which agents read as often as `src` and which the API
/// index leaves out. Not the whole crate directory, which can hold vendored
/// SDKs.
fn crate_roots(roots: &[(PathBuf, String, bool)]) -> Vec<(PathBuf, String, bool)> {
    let mut out: Vec<(PathBuf, String, bool)> = Vec::new();
    for (root, origin, private) in roots {
        out.push((root.clone(), origin.clone(), *private));
        let Some(parent) = root.parent() else { continue };
        if root.file_name().is_some_and(|n| n == "src") && parent.join("Cargo.toml").is_file() {
            for extra in ["examples", "tests", "benches", "build.rs"] {
                let p = parent.join(extra);
                if p.exists() {
                    out.push((p, origin.clone(), *private));
                }
            }
        }
    }
    out
}

/// Bundled or minified code: long lines on average.
fn looks_minified(text: &str) -> bool {
    let lines = text.lines().count().max(1);
    text.len() > 20_000 && text.len() / lines > 300
}

fn is_source(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("rs" | "wgsl")) || Language::from_path(path).is_some()
}

impl Nav {
    pub fn new() -> Self {
        Self::default()
    }

    fn files(&self, roots: &[(PathBuf, String, bool)]) -> Vec<SrcFile> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (root, origin, _) in &crate_roots(roots) {
            let walker = ignore::WalkBuilder::new(root)
                .filter_entry(|e| {
                    let n = e.file_name().to_string_lossy();
                    !matches!(
                        n.as_ref(),
                        "target" | "node_modules" | ".git" | "dist" | "build" | "venv" | ".venv" | "__pycache__"
                    )
                })
                .build();
            for entry in walker.flatten() {
                let p = entry.path();
                if entry.file_type().is_some_and(|t| t.is_file()) && is_source(p) {
                    let abs = absolute(p);
                    if seen.insert(abs.clone()) {
                        out.push(SrcFile { path: abs, origin: origin.clone() });
                    }
                }
            }
        }
        out
    }

    fn read(&mut self, path: &Path) -> Option<Arc<String>> {
        let md = std::fs::metadata(path).ok()?;
        let key = (md.len(), md.modified().ok());
        if let Some((len, mtime, text)) = self.cache.get(path) {
            if (*len, *mtime) == key {
                return Some(text.clone());
            }
        }
        let raw = std::fs::read_to_string(path).ok()?;
        // A minified bundle is one enormous line: unreadable as source, and
        // every name search would match inside it.
        let text = Arc::new(if looks_minified(&raw) { String::new() } else { raw });
        self.cache.insert(path.to_path_buf(), (key.0, key.1, text.clone()));
        Some(text)
    }

    fn defs_of(&mut self, path: &Path, text: &Arc<String>) -> Arc<Result<Vec<Def>, String>> {
        if let Some((t, d)) = self.defs.get(path) {
            if Arc::ptr_eq(t, text) {
                return d.clone();
            }
        }
        let d = Arc::new(defs_in(path, text));
        self.defs.insert(path.to_path_buf(), (text.clone(), d.clone()));
        d
    }

    fn idents_of(&mut self, path: &Path, text: &Arc<String>) -> Arc<Option<IdentMap>> {
        if let Some((t, m)) = self.idents.get(path) {
            if Arc::ptr_eq(t, text) {
                return m.clone();
            }
        }
        let m = Arc::new(ident_map(path, text));
        self.idents.insert(path.to_path_buf(), (text.clone(), m.clone()));
        m
    }

    /// Files whose text contains `needle`, with their text.
    fn candidates(&mut self, roots: &[(PathBuf, String, bool)], needle: &str, file: Option<&str>) -> Vec<(SrcFile, Arc<String>)> {
        let mut out = Vec::new();
        for f in self.files(roots) {
            if let Some(want) = file {
                if !f.path.to_string_lossy().contains(want) {
                    continue;
                }
            }
            if let Some(text) = self.read(&f.path) {
                if needle.is_empty() || text.contains(needle) {
                    out.push((f, text));
                }
            }
        }
        out
    }

    // ── get_source ───────────────────────────────────────────────────────────

    /// Show the definitions of one or more names (`a|b|c`) with numbered lines.
    pub fn get_source(
        &mut self,
        roots: &[(PathBuf, String, bool)],
        query: &str,
        file: Option<&str>,
        lines: Option<(usize, usize)>,
        max_lines: usize,
    ) -> String {
        let names = split_names(query);
        if names.is_empty() {
            return "Give a name: `get_source(name=\"draw\")`, `Canvas::draw`, or several as `a|b`.".into();
        }
        let mut out = String::new();
        let mut budget = TOTAL_LINES.max(max_lines);
        for want in &names {
            let (owner, bare) = match want.rsplit_once("::") {
                Some((o, b)) => (Some(o.rsplit("::").next().unwrap_or(o)), b),
                None => (None, want.as_str()),
            };
            let mut found: Vec<(SrcFile, Arc<String>, Def)> = Vec::new();
            let mut broken: Vec<String> = Vec::new();
            for (f, text) in self.candidates(roots, bare, file) {
                match &*self.defs_of(&f.path, &text) {
                    Ok(defs) => {
                        for d in defs {
                            let hit = match owner {
                                Some(o) => d.name == bare && d.qual == format!("{o}::{bare}"),
                                None => d.name == bare && d.kind != "impl",
                            };
                            if hit {
                                found.push((f.clone(), text.clone(), d.clone()));
                            }
                        }
                    }
                    Err(e) => broken.push(format!("{} ({e})", f.path.display())),
                }
            }
            if found.is_empty() {
                out.push_str(&not_found(want, &broken));
                continue;
            }
            found.sort_by(|a, b| (a.0.path.cmp(&b.0.path), a.2.start).cmp(&(b.0.path.cmp(&a.0.path), b.2.start)));
            let total: usize = found.iter().map(|(_, _, d)| d.end + 1 - d.start).sum();
            if found.len() > 6 && total > budget && lines.is_none() {
                out.push_str(&format!(
                    "`{want}`: {} definitions; narrow with `Type::{bare}` or `file`:\n",
                    found.len()
                ));
                for (f, _, d) in &found {
                    out.push_str(&format!("  {} {}  {}:{}-{}\n", d.kind, d.qual, f.path.display(), d.start, d.end));
                }
                continue;
            }
            for (f, text, d) in &found {
                let (from, to) = match lines {
                    Some((a, b)) => (a.max(1), b),
                    None => (d.start, d.end),
                };
                out.push_str(&format!(
                    "{} {} — {}:{}-{} ({}{})\n",
                    d.kind,
                    d.qual,
                    f.path.display(),
                    d.start,
                    d.end,
                    origin_label(f),
                    if d.ctx.is_empty() { String::new() } else { format!("; {}", d.ctx) }
                ));
                let cap = max_lines.min(budget.max(20));
                let shown = render_lines(text, from, to, cap, &mut out);
                budget = budget.saturating_sub(shown);
                if matches!(d.kind, "struct" | "enum" | "trait" | "union" | "class" | "interface") && lines.is_none() {
                    let methods = self.methods_of(roots, &d.name);
                    if !methods.is_empty() {
                        out.push_str(&format!("  methods of {} ({}):\n", d.name, methods.len()));
                        for (path, m) in methods.iter().take(40) {
                            out.push_str(&format!("    {}  {}:{}-{}\n", m.qual, path.display(), m.start, m.end));
                        }
                        if methods.len() > 40 {
                            out.push_str(&format!("    ... {} more\n", methods.len() - 40));
                        }
                    }
                }
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    }

    /// Every method defined in an impl or class block for `owner`, across the roots.
    fn methods_of(&mut self, roots: &[(PathBuf, String, bool)], owner: &str) -> Vec<(PathBuf, Def)> {
        let mut out = Vec::new();
        let prefix = format!("{owner}::");
        for (f, text) in self.candidates(roots, owner, None) {
            if let Ok(defs) = &*self.defs_of(&f.path, &text) {
                for d in defs {
                    if matches!(d.kind, "fn" | "const" | "type") && d.qual.starts_with(&prefix) && d.qual[prefix.len()..] == d.name {
                        out.push((f.path.clone(), d.clone()));
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.0, a.1.start).cmp(&(&b.0, b.1.start)));
        out
    }

    // ── find_references ──────────────────────────────────────────────────────

    /// Every use of one or more names, grouped by file and enclosing item.
    pub fn find_references(
        &mut self,
        roots: &[(PathBuf, String, bool)],
        query: &str,
        file: Option<&str>,
        include_comments: bool,
        limit: usize,
    ) -> String {
        let names = split_names(query);
        if names.is_empty() {
            return "Give a name: `find_references(name=\"flush_batch\")`, or several as `a|b`.".into();
        }
        let mut out = String::new();
        for want in &names {
            let bare = want.rsplit("::").next().unwrap_or(want).to_string();
            // file -> item label -> [(line, text, is_code)]
            let mut groups: BTreeMap<PathBuf, BTreeMap<(usize, String), Vec<(usize, String, bool)>>> = BTreeMap::new();
            let mut defs_at: HashSet<(PathBuf, usize)> = HashSet::new();
            let mut total = 0usize;
            let mut files_hit = 0usize;
            let mut unparsed = Vec::new();
            // Whole-word matches in comments and strings, counted when they are
            // not listed: a name that lives in SQL or a doc comment must not
            // look absent.
            let mut elsewhere = 0usize;
            let mut elsewhere_files = 0usize;
            for (f, text) in self.candidates(roots, &bare, file) {
                let code_lines: Vec<usize> = match &*self.idents_of(&f.path, &text) {
                    Some(map) => map.get(&bare).map(|v| v.iter().map(|&l| l as usize).collect()).unwrap_or_default(),
                    None => {
                        unparsed.push(f.path.display().to_string());
                        word_lines(&text, &bare)
                    }
                };
                let code: HashSet<usize> = code_lines.iter().copied().collect();
                let mut hits: Vec<(usize, bool)> = code.iter().map(|&l| (l, true)).collect();
                let other: Vec<usize> = word_lines(&text, &bare).into_iter().filter(|l| !code.contains(l)).collect();
                if include_comments {
                    hits.extend(other.iter().map(|&l| (l, false)));
                } else if !other.is_empty() {
                    elsewhere += other.len();
                    elsewhere_files += 1;
                }
                if hits.is_empty() {
                    continue;
                }
                hits.sort();
                files_hit += 1;
                let defs = self.defs_of(&f.path, &text);
                let defs: &[Def] = match &*defs {
                    Ok(d) => d,
                    Err(_) => &[],
                };
                let all: Vec<&str> = text.lines().collect();
                for (line, is_code) in hits {
                    total += 1;
                    let enclosing = innermost(defs, line);
                    let label = match enclosing {
                        Some(d) => {
                            if d.name == bare && d.name_line == line {
                                defs_at.insert((f.path.clone(), line));
                            }
                            (d.start, format!("{} {} {}-{}", d.kind, d.qual, d.start, d.end))
                        }
                        None => (0, "(top level)".to_string()),
                    };
                    let t = all.get(line - 1).map(|s| cut(s.trim(), USE_TEXT_CAP)).unwrap_or_default();
                    groups.entry(f.path.clone()).or_default().entry(label).or_default().push((line, t, is_code));
                }
            }
            let skipped = if elsewhere > 0 {
                format!("; {elsewhere} more in comments/strings in {elsewhere_files} file(s), listed with include_comments=true")
            } else {
                String::new()
            };
            if total == 0 {
                out.push_str(&format!("`{want}`: no uses in code in the indexed roots{skipped}.\n\n"));
                continue;
            }
            out.push_str(&format!(
                "`{bare}`: {total} use(s) in {files_hit} file(s){}\n",
                if include_comments { ", comments and strings marked ~".to_string() } else { format!(", code only{skipped}") }
            ));
            let mut shown = 0usize;
            let mut hidden: Vec<String> = Vec::new();
            for (path, items) in &groups {
                let n: usize = items.values().map(Vec::len).sum();
                if shown >= limit {
                    hidden.push(format!("{} ({n})", path.display()));
                    continue;
                }
                out.push_str(&format!("{}\n", path.display()));
                for ((_, label), uses) in items {
                    if shown >= limit {
                        break;
                    }
                    let is_def = uses.iter().any(|(l, _, _)| defs_at.contains(&(path.clone(), *l)));
                    out.push_str(&format!("  {label}{}\n", if is_def { " (definition)" } else { "" }));
                    for (l, t, code) in uses.iter().take(limit.saturating_sub(shown)) {
                        out.push_str(&format!("{l:>7}{} {t}\n", if *code { " " } else { "~" }));
                        shown += 1;
                    }
                }
            }
            if !hidden.is_empty() {
                out.push_str(&format!(
                    "... {} more use(s) in: {} (raise `limit` or narrow with `file`)\n",
                    total - shown,
                    hidden.join(", ")
                ));
            }
            if !unparsed.is_empty() {
                out.push_str(&format!(
                    "[note] {} file(s) did not tokenize; whole-word matches used there: {}\n",
                    unparsed.len(),
                    unparsed.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    // ── outline ──────────────────────────────────────────────────────────────

    /// A file's items with line ranges, or a directory's files with their
    /// top-level items.
    pub fn outline(&mut self, roots: &[(PathBuf, String, bool)], path_query: &str) -> String {
        let q = path_query.trim().trim_end_matches('/');
        if q.is_empty() {
            return "Give a file or directory: `get_outline(path=\"src/canvas/core.rs\")`.".into();
        }
        let files = self.files(roots);
        let qa = absolute(Path::new(q));
        let exact: Vec<&SrcFile> = files.iter().filter(|f| f.path == qa).collect();
        let matching: Vec<&SrcFile> = if !exact.is_empty() {
            exact
        } else {
            files.iter().filter(|f| f.path.to_string_lossy().ends_with(q) || f.path.to_string_lossy().contains(&format!("/{q}"))).collect()
        };
        let in_dir: Vec<&SrcFile> = files
            .iter()
            .filter(|f| f.path.starts_with(&qa) || f.path.to_string_lossy().contains(&format!("/{q}/")))
            .collect();
        if matching.len() == 1 {
            let f = matching[0].clone();
            return self.outline_file(&f);
        }
        if matching.len() > 1 {
            let mut out = format!("`{q}` matches {} files; pass one of:\n", matching.len());
            for f in matching.iter().take(30) {
                out.push_str(&format!("  {}\n", f.path.display()));
            }
            return out.trim_end().to_string();
        }
        if in_dir.is_empty() {
            return format!("No source file or directory matching `{q}` under the indexed roots.");
        }
        let mut out = format!("{q}/ — {} source file(s)\n", in_dir.len());
        for f in in_dir.iter().take(80) {
            let Some(text) = self.read(&f.path) else { continue };
            let n = text.lines().count();
            let tops: Vec<String> = match &*self.defs_of(&f.path, &text) {
                Ok(defs) => defs
                    .iter()
                    .filter(|d| !d.qual.contains("::") && d.kind != "impl" && !d.kind.starts_with("wgsl"))
                    .map(|d| d.name.clone())
                    .collect(),
                Err(_) => vec!["(does not parse)".into()],
            };
            let shown: Vec<String> = tops.iter().take(12).cloned().collect();
            let more = tops.len().saturating_sub(shown.len());
            out.push_str(&format!(
                "  {} ({n} lines): {}{}\n",
                f.path.strip_prefix(&qa).unwrap_or(&f.path).display(),
                shown.join(", "),
                if more > 0 { format!(", +{more}") } else { String::new() }
            ));
        }
        if in_dir.len() > 80 {
            out.push_str(&format!("  ... {} more files\n", in_dir.len() - 80));
        }
        out.trim_end().to_string()
    }

    fn outline_file(&mut self, f: &SrcFile) -> String {
        let Some(text) = self.read(&f.path) else {
            return format!("{} cannot be read.", f.path.display());
        };
        let lines: Vec<&str> = text.lines().collect();
        let defs = self.defs_of(&f.path, &text);
        let defs = match &*defs {
            Ok(d) => d.clone(),
            Err(e) => return format!("{} does not parse ({e}); read it directly.", f.path.display()),
        };
        let mut out = format!("{} ({}, {} lines)\n", f.path.display(), origin_label(f), lines.len());
        let defs: Vec<Def> = defs.into_iter().filter(|d| !matches!(d.kind, "field" | "variant")).collect();
        for d in &defs {
            let depth = defs
                .iter()
                .filter(|o| !std::ptr::eq(*o, d) && o.start <= d.start && o.end >= d.end && (o.start, o.end) != (d.start, d.end))
                .count();
            let raw = lines
                .get(d.name_line.saturating_sub(1))
                .map(|s| signature_text(s))
                .unwrap_or_default();
            let sig = match d.kind.strip_prefix("wgsl ") {
                Some(kw) if raw.starts_with(kw) || raw.starts_with('@') => format!("[wgsl] {raw}"),
                Some(kw) => format!("[wgsl] {kw} {}", d.name),
                None => raw,
            };
            out.push_str(&format!("{}{}  {}-{}\n", "  ".repeat(depth + 1), sig, d.start, d.end));
        }
        out.trim_end().to_string()
    }
}

// ── Definitions ───────────────────────────────────────────────────────────────

fn ext_of(path: &Path) -> &str {
    path.extension().and_then(|e| e.to_str()).unwrap_or("")
}

fn defs_in(path: &Path, text: &str) -> Result<Vec<Def>, String> {
    let mut defs = match ext_of(path) {
        "rs" => {
            let mut d = rust_defs(text)?;
            for (lit, line) in wgsl_literals(text) {
                wgsl_defs(&lit, line, &mut d);
            }
            d
        }
        "wgsl" => {
            let mut d = Vec::new();
            wgsl_defs(text, 1, &mut d);
            d
        }
        _ => match Language::from_path(path) {
            Some(lang) => ts_defs(text, lang),
            None => Vec::new(),
        },
    };
    defs.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    // Say which Rust item a shader sits in, and which file a `mod x;` loads.
    let rust: Vec<(usize, usize, String)> = defs
        .iter()
        .filter(|d| !d.kind.starts_with("wgsl") && !matches!(d.kind, "field" | "variant"))
        .map(|d| (d.start, d.end, format!("{} {}", d.kind, d.qual)))
        .collect();
    for d in defs.iter_mut() {
        if d.kind.starts_with("wgsl") && ext_of(path) == "rs" {
            if let Some((_, _, label)) = rust.iter().filter(|(s, e, _)| *s <= d.start && d.end <= *e).min_by_key(|(s, e, _)| e - s) {
                d.ctx = format!("in {label}");
            }
        } else if d.kind == "mod" && d.start == d.end {
            if let Some(p) = mod_file(path, &d.name) {
                d.ctx = format!("file {}", p.display());
            }
        }
    }
    Ok(defs)
}

/// The file a `mod name;` declared in `decl_file` loads (ignoring `#[path]`).
fn mod_file(decl_file: &Path, name: &str) -> Option<PathBuf> {
    let dir = decl_file.parent()?;
    let stem = decl_file.file_stem()?.to_str()?;
    let base = if matches!(stem, "lib" | "main" | "mod") { dir.to_path_buf() } else { dir.join(stem) };
    [base.join(format!("{name}.rs")), base.join(name).join("mod.rs")].into_iter().find(|p| p.is_file())
}

pub fn rust_defs(text: &str) -> Result<Vec<Def>, String> {
    let file = syn::parse_file(text).map_err(|e| {
        let l = e.span().start().line;
        format!("line {l}: {e}")
    })?;
    let mut out = Vec::new();
    for item in &file.items {
        rust_item(item, "", &mut out);
    }
    Ok(out)
}

fn push(out: &mut Vec<Def>, name: String, qual: String, kind: &'static str, whole: proc_macro2::Span, ident: proc_macro2::Span) {
    let start = whole.start().line;
    let end = whole.end().line.max(start);
    if start == 0 {
        return;
    }
    out.push(Def { name, qual, kind, start, name_line: ident.start().line.max(start), end, ctx: String::new() });
}

/// One item and everything declared inside it. `outer` is the function an
/// item is nested in (`outer::NAME`), empty at module level.
fn rust_item(item: &syn::Item, outer: &str, out: &mut Vec<Def>) {
    let q = |n: &str| if outer.is_empty() { n.to_string() } else { format!("{outer}::{n}") };
    match item {
        syn::Item::Fn(f) => {
            let n = f.sig.ident.to_string();
            push(out, n.clone(), q(&n), "fn", item.span(), f.sig.ident.span());
            nested(&f.block, &q(&n), out);
        }
        syn::Item::Struct(s) => {
            let n = s.ident.to_string();
            push(out, n.clone(), q(&n), "struct", item.span(), s.ident.span());
            fields(&s.fields, &n, out);
        }
        syn::Item::Enum(e) => {
            let n = e.ident.to_string();
            push(out, n.clone(), q(&n), "enum", item.span(), e.ident.span());
            for v in &e.variants {
                let vn = v.ident.to_string();
                push(out, vn.clone(), format!("{n}::{vn}"), "variant", v.span(), v.ident.span());
            }
        }
        syn::Item::Union(u) => push(out, u.ident.to_string(), q(&u.ident.to_string()), "union", item.span(), u.ident.span()),
        syn::Item::Type(t) => push(out, t.ident.to_string(), q(&t.ident.to_string()), "type", item.span(), t.ident.span()),
        syn::Item::Const(c) => push(out, c.ident.to_string(), q(&c.ident.to_string()), "const", item.span(), c.ident.span()),
        syn::Item::Static(s) => push(out, s.ident.to_string(), q(&s.ident.to_string()), "static", item.span(), s.ident.span()),
        syn::Item::Macro(m) => {
            if let Some(id) = &m.ident {
                push(out, id.to_string(), q(&id.to_string()), "macro", item.span(), id.span());
            }
        }
        syn::Item::Trait(t) => {
            let owner = t.ident.to_string();
            push(out, owner.clone(), q(&owner), "trait", item.span(), t.ident.span());
            for ti in &t.items {
                match ti {
                    syn::TraitItem::Fn(m) => {
                        let n = m.sig.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "fn", ti.span(), m.sig.ident.span());
                        if let Some(b) = &m.default {
                            nested(b, &format!("{owner}::{n}"), out);
                        }
                    }
                    syn::TraitItem::Const(c) => {
                        let n = c.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "const", ti.span(), c.ident.span());
                    }
                    syn::TraitItem::Type(t) => {
                        let n = t.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "type", ti.span(), t.ident.span());
                    }
                    _ => {}
                }
            }
        }
        syn::Item::Mod(m) => {
            let n = m.ident.to_string();
            push(out, n.clone(), q(&n), "mod", item.span(), m.ident.span());
            if let Some((_, inner)) = &m.content {
                for i in inner {
                    rust_item(i, outer, out);
                }
            }
        }
        syn::Item::Impl(i) => {
            let owner = type_name(&i.self_ty);
            if owner.is_empty() {
                return;
            }
            let label = match &i.trait_ {
                Some((_, p, _)) => format!(
                    "impl {} for {owner}",
                    p.segments.last().map(|s| s.ident.to_string()).unwrap_or_default()
                ),
                None => format!("impl {owner}"),
            };
            push(out, owner.clone(), label, "impl", item.span(), i.self_ty.span());
            for ii in &i.items {
                match ii {
                    syn::ImplItem::Fn(m) => {
                        let n = m.sig.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "fn", ii.span(), m.sig.ident.span());
                        nested(&m.block, &format!("{owner}::{n}"), out);
                    }
                    syn::ImplItem::Const(c) => {
                        let n = c.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "const", ii.span(), c.ident.span());
                    }
                    syn::ImplItem::Type(t) => {
                        let n = t.ident.to_string();
                        push(out, n.clone(), format!("{owner}::{n}"), "type", ii.span(), t.ident.span());
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn fields(f: &syn::Fields, owner: &str, out: &mut Vec<Def>) {
    if let syn::Fields::Named(named) = f {
        for fl in &named.named {
            if let Some(id) = &fl.ident {
                let n = id.to_string();
                push(out, n.clone(), format!("{owner}::{n}"), "field", fl.span(), id.span());
            }
        }
    }
}

/// Items declared inside a function body - a `const` beside the code that
/// uses it, a helper `fn` - which a walk over module items alone misses.
fn nested(block: &syn::Block, outer: &str, out: &mut Vec<Def>) {
    struct V<'a> {
        outer: &'a str,
        out: &'a mut Vec<Def>,
    }
    impl<'ast> syn::visit::Visit<'ast> for V<'_> {
        fn visit_item(&mut self, i: &'ast syn::Item) {
            rust_item(i, self.outer, self.out);
        }
    }
    syn::visit::Visit::visit_block(&mut V { outer, out }, block);
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default(),
        syn::Type::Reference(r) => type_name(&r.elem),
        syn::Type::Group(g) => type_name(&g.elem),
        syn::Type::Paren(p) => type_name(&p.elem),
        _ => String::new(),
    }
}

// ── WGSL, in .wgsl files and inside Rust string literals ──────────────────────

/// String literals in Rust source that hold WGSL, each with the line it starts
/// on. Shaders built in Rust live in `r#"..."#` and `format!` strings, where a
/// Rust parser sees one opaque token and grep sees everything.
fn wgsl_literals(text: &str) -> Vec<(String, usize)> {
    let Ok(ts) = text.parse::<proc_macro2::TokenStream>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_literals(ts, &mut out);
    out.retain(|(s, _)| looks_like_wgsl(s));
    // In a normal string a shader's lines are joined by `\n` escapes, which
    // would glue `n` onto the next word. Same-length blanks keep the offsets.
    for (s, _) in out.iter_mut() {
        if s.starts_with('"') {
            *s = s.replace("\\n", "  ").replace("\\t", "  ").replace("\\r", "  ");
        }
    }
    out
}

fn collect_literals(ts: proc_macro2::TokenStream, out: &mut Vec<(String, usize)>) {
    for tt in ts {
        match tt {
            proc_macro2::TokenTree::Literal(l) => {
                let s = l.to_string();
                if s.starts_with('"') || s.starts_with("r\"") || s.starts_with("r#") {
                    out.push((s, l.span().start().line));
                }
            }
            proc_macro2::TokenTree::Group(g) => collect_literals(g.stream(), out),
            _ => {}
        }
    }
}

fn looks_like_wgsl(s: &str) -> bool {
    const STRONG: &[&str] = &[
        "@vertex", "@fragment", "@compute", "@group(", "@binding(", "@location(", "@builtin(",
        "@workgroup_size", "var<uniform>", "var<storage", "var<private>", "var<workgroup>",
    ];
    const WEAK: &[&str] = &[
        "vec2<f32>", "vec3<f32>", "vec4<f32>", "mat4x4<f32>", "mat3x3<f32>", "vec2f", "vec3f", "vec4f",
        "-> f32", "-> vec", "-> u32", ": f32", ": u32", "textureSample", "array<", "select(", "clamp(",
    ];
    STRONG.iter().any(|m| s.contains(m)) || WEAK.iter().filter(|m| s.contains(*m)).count() >= 2
}

/// `src` with WGSL comments blanked to spaces, newlines kept, so offsets and
/// line numbers still match.
fn blank_wgsl_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let (mut i, mut depth) = (0usize, 0usize);
    while i < b.len() {
        if depth == 0 && b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            continue;
        }
        if depth > 0 && b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            continue;
        }
        if depth > 0 && b[i] != b'\n' {
            out[i] = b' ';
        }
        i += 1;
    }
    // Only ASCII bytes were replaced, so this stays valid UTF-8 wherever the
    // input was; fall back to the input if a comment held multi-byte text.
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

fn line_at(starts: &[usize], base: usize, offset: usize) -> usize {
    base + starts.partition_point(|&s| s <= offset) - 1
}

/// Module-scope WGSL declarations in `src`, whose first line is source line
/// `base`.
fn wgsl_defs(src: &str, base: usize, out: &mut Vec<Def>) {
    static DECL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = DECL.get_or_init(|| {
        regex::Regex::new(
            r"((?:@[A-Za-z_]\w*(?:\([^()]*\))?\s*)*)\b(fn|struct|const|override|alias|var(?:<[^<>]*>)?)\s+([A-Za-z_]\w*)",
        )
        .unwrap()
    });
    let clean = blank_wgsl_comments(src);
    let bytes = clean.as_bytes();
    let starts = line_starts(&clean);
    let (mut pos, mut depth) = (0usize, 0i64);
    for c in re.captures_iter(&clean) {
        let kw = c.get(2).unwrap();
        while pos < kw.start() {
            match bytes[pos] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            pos += 1;
        }
        if depth != 0 {
            continue;
        }
        let name = c.get(3).unwrap();
        let kind = match kw.as_str() {
            "fn" => "wgsl fn",
            "struct" => "wgsl struct",
            "const" => "wgsl const",
            "override" => "wgsl override",
            "alias" => "wgsl alias",
            _ => "wgsl var",
        };
        let first = c.get(1).filter(|a| !a.as_str().is_empty()).map_or(kw.start(), |a| a.start());
        let end = if matches!(kind, "wgsl fn" | "wgsl struct") {
            let mut d = 0i64;
            let mut end = clean.len().saturating_sub(1);
            let mut opened = false;
            for (i, &ch) in bytes.iter().enumerate().skip(name.end()) {
                match ch {
                    b'{' => {
                        d += 1;
                        opened = true;
                    }
                    b'}' => {
                        d -= 1;
                        if opened && d <= 0 {
                            end = i;
                            break;
                        }
                    }
                    b';' if !opened => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            end
        } else {
            clean[name.end()..].find(';').map_or(clean.len().saturating_sub(1), |i| name.end() + i)
        };
        out.push(Def {
            name: name.as_str().to_string(),
            qual: name.as_str().to_string(),
            kind,
            start: line_at(&starts, base, first),
            name_line: line_at(&starts, base, name.start()),
            end: line_at(&starts, base, end),
            ctx: String::new(),
        });
    }
}

const TS_DEF_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_definition",
    "function_signature",
    "method_definition",
    "method_declaration",
    "method_signature",
    "abstract_method_signature",
    "constructor_declaration",
    "class_declaration",
    "abstract_class_declaration",
    "class_definition",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
    "struct_declaration",
    "record_declaration",
    "trait_declaration",
    "namespace_declaration",
    "type_spec",
    "class",
    "module",
    "method",
    "singleton_method",
];

const TS_OWNER_KINDS: &[&str] = &[
    "class_declaration",
    "abstract_class_declaration",
    "class_definition",
    "interface_declaration",
    "struct_declaration",
    "record_declaration",
    "trait_declaration",
    "namespace_declaration",
    "class",
    "module",
];

fn ts_kind(kind: &str) -> &'static str {
    if kind.contains("class") || kind == "module" || kind.contains("struct") || kind.contains("record") {
        "class"
    } else if kind.contains("interface") || kind.contains("trait") {
        "interface"
    } else if kind.contains("enum") {
        "enum"
    } else if kind.contains("type") {
        "type"
    } else if kind.contains("namespace") {
        "mod"
    } else {
        "fn"
    }
}

pub fn ts_defs(text: &str, lang: Language) -> Vec<Def> {
    let mut parser = TsParser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    ts_walk(tree.root_node(), text, None, &mut out);
    out.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    out
}

fn node_text<'a>(n: Node, src: &'a str) -> &'a str {
    src.get(n.start_byte()..n.end_byte()).unwrap_or("")
}

fn ts_walk(node: Node, src: &str, owner: Option<&str>, out: &mut Vec<Def>) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        let kind = child.kind();
        // Python decorators and JS `export` wrap the definition: cite from the wrapper.
        let outer = match node.kind() {
            "decorated_definition" | "export_statement" => node,
            _ => child,
        };
        let mut next_owner = owner.map(str::to_string);
        if TS_DEF_KINDS.contains(&kind) {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, src).to_string();
                if !name.is_empty() {
                    let k = ts_kind(kind);
                    let qual = match owner {
                        Some(o) if k == "fn" => format!("{o}::{name}"),
                        _ => name.clone(),
                    };
                    out.push(Def {
                        name: name.clone(),
                        qual,
                        kind: k,
                        start: outer.start_position().row + 1,
                        name_line: name_node.start_position().row + 1,
                        end: child.end_position().row + 1,
                        ctx: String::new(),
                    });
                    if TS_OWNER_KINDS.contains(&kind) {
                        next_owner = Some(name);
                    }
                }
            }
        } else if kind == "variable_declarator" {
            // const draw = (..) => {..}  /  const draw = function () {..}
            if let (Some(n), Some(v)) = (child.child_by_field_name("name"), child.child_by_field_name("value")) {
                if matches!(v.kind(), "arrow_function" | "function" | "function_expression" | "generator_function") {
                    let name = node_text(n, src).to_string();
                    let decl = node; // lexical_declaration
                    let start_node = match decl.parent() {
                        Some(p) if p.kind() == "export_statement" => p,
                        _ => decl,
                    };
                    out.push(Def {
                        name: name.clone(),
                        qual: name,
                        kind: "fn",
                        start: start_node.start_position().row + 1,
                        name_line: n.start_position().row + 1,
                        end: child.end_position().row + 1,
                        ctx: String::new(),
                    });
                }
            }
        }
        ts_walk(child, src, next_owner.as_deref(), out);
    }
}

// ── References ────────────────────────────────────────────────────────────────

/// Every identifier used as code in a file, with its lines: comments and
/// string literals excluded, except string literals holding WGSL, which are
/// shader code. `None` when the file does not tokenize.
fn ident_map(path: &Path, text: &str) -> Option<IdentMap> {
    let mut map: IdentMap = HashMap::new();
    match ext_of(path) {
        "rs" => {
            let ts: proc_macro2::TokenStream = text.parse().ok()?;
            rust_idents(ts, &mut map);
            for (lit, line) in wgsl_literals(text) {
                wgsl_words(&lit, line, &mut map);
            }
        }
        "wgsl" => wgsl_words(text, 1, &mut map),
        _ => {
            let lang = Language::from_path(path)?;
            let mut parser = TsParser::new();
            parser.set_language(&lang.ts_language()).ok()?;
            let tree = parser.parse(text, None)?;
            ts_idents(tree.root_node(), text, &mut map);
        }
    }
    for lines in map.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }
    Some(map)
}

fn rust_idents(ts: proc_macro2::TokenStream, map: &mut IdentMap) {
    for tt in ts {
        match tt {
            proc_macro2::TokenTree::Ident(i) => {
                let s = i.to_string();
                let line = i.span().start().line as u32;
                let key = s.strip_prefix("r#").map(str::to_string).unwrap_or(s);
                map.entry(key).or_default().push(line);
            }
            proc_macro2::TokenTree::Group(g) => rust_idents(g.stream(), map),
            _ => {}
        }
    }
}

fn ts_idents(node: Node, src: &str, map: &mut IdentMap) {
    if node.child_count() == 0 {
        let k = node.kind();
        if k.contains("identifier") || k == "constant" || k == "name" {
            let t = node_text(node, src);
            if !t.is_empty() {
                map.entry(t.to_string()).or_default().push(node.start_position().row as u32 + 1);
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        ts_idents(child, src, map);
    }
}

/// Every word of WGSL code in `src` (comments excluded), whose first line is
/// source line `base`.
fn wgsl_words(src: &str, base: usize, map: &mut IdentMap) {
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = WORD.get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
    let clean = blank_wgsl_comments(src);
    let starts = line_starts(&clean);
    for m in re.find_iter(&clean) {
        map.entry(m.as_str().to_string()).or_default().push(line_at(&starts, base, m.start()) as u32);
    }
}

/// Lines where `name` occurs as a whole word anywhere, comments and strings
/// included.
fn word_lines(text: &str, name: &str) -> Vec<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(pos) = line[from..].find(name) {
            let a = from + pos;
            let b = a + name.len();
            let before = line[..a].chars().next_back();
            let after = line[b..].chars().next();
            if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
                out.push(i + 1);
                break;
            }
            from = b;
        }
    }
    out
}

/// The innermost definition containing `line`, preferring functions over the
/// impl or class around them.
fn innermost(defs: &[Def], line: usize) -> Option<&Def> {
    defs.iter()
        .filter(|d| d.start <= line && line <= d.end)
        .min_by_key(|d| (d.end - d.start, if d.kind == "impl" { 1 } else { 0 }))
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn split_names(q: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    q.split(['|', ',', ' '])
        .map(|s| s.trim().trim_matches('`').trim_end_matches("()").to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

fn render_lines(text: &str, from: usize, to: usize, cap: usize, out: &mut String) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let to = to.min(lines.len());
    if from > to {
        out.push_str("  (no lines in that range)\n");
        return 0;
    }
    let last = (from + cap).saturating_sub(1).min(to);
    for n in from..=last {
        out.push_str(&format!("{n:>5}\t{}\n", cut(lines[n - 1], LINE_CAP)));
    }
    if last < to {
        out.push_str(&format!(
            "  ... {} more lines ({}-{}); ask again with lines=\"{}-{}\"\n",
            to - last,
            last + 1,
            to,
            last + 1,
            to
        ));
    }
    last + 1 - from
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

fn signature_text(line: &str) -> String {
    let t = line.trim();
    let t = t.split(" {").next().unwrap_or(t).trim_end_matches('{').trim();
    cut(t, 110)
}

fn origin_label(f: &SrcFile) -> String {
    let lang = if ext_of(&f.path) == "rs" {
        "rust"
    } else if ext_of(&f.path) == "wgsl" {
        "wgsl"
    } else {
        Language::from_path(&f.path).map(|l| l.label()).unwrap_or("text")
    };
    if f.origin.is_empty() {
        lang.to_string()
    } else {
        format!("{lang} · {}", f.origin)
    }
}

fn not_found(want: &str, broken: &[String]) -> String {
    let mut s = format!("`{want}`: no definition found in the indexed roots.");
    if !broken.is_empty() {
        s.push_str(&format!(
            " {} file(s) containing the name do not parse, so a definition may be there: {}",
            broken.len(),
            broken.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        ));
    }
    s.push_str(" Try find_references to see where it is used, or grep for text outside the indexed roots.\n\n");
    s
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    }
}

/// `"120-160"` -> (120, 160); `"120"` -> (120, 120).
pub fn parse_range(s: &str) -> Option<(usize, usize)> {
    let s = s.trim();
    match s.split_once('-') {
        Some((a, b)) => {
            let a: usize = a.trim().parse().ok()?;
            let b: usize = b.trim().parse().ok()?;
            (a <= b).then_some((a, b))
        }
        None => s.parse().ok().map(|a| (a, a)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(files: &[(&str, &str)]) -> (tempdir_lite::Dir, Vec<(PathBuf, String, bool)>) {
        let d = tempdir_lite::Dir::new();
        for (name, body) in files {
            let p = d.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let roots = vec![(d.path().to_path_buf(), "t".to_string(), true)];
        (d, roots)
    }

    const CANVAS: &str = "pub struct Canvas { w: u32 }\n\nimpl Canvas {\n    /// Draws it.\n    #[inline]\n    pub fn draw(&mut self) {\n        self.flush_batch();\n        // flush_batch in a comment\n        let s = \"flush_batch\";\n    }\n\n    fn flush_batch(&mut self) {}\n}\n\nfn draw_rect() {}\n";

    #[test]
    fn source_of_a_method_includes_its_docs_and_ends_at_its_brace() {
        let (_d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let out = Nav::new().get_source(&roots, "Canvas::draw", None, None, DEFAULT_MAX_LINES);
        assert!(out.starts_with("fn Canvas::draw — "), "{out}");
        assert!(out.contains(":4-10 "), "{out}");
        assert!(out.contains("    4\t    /// Draws it."), "{out}");
        assert!(out.contains("   10\t    }"), "{out}");
        assert!(!out.contains("flush_batch(&mut self) {}"), "{out}");
    }

    #[test]
    fn a_bare_name_does_not_match_a_longer_one() {
        let (_d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let out = Nav::new().get_source(&roots, "draw", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("fn Canvas::draw"), "{out}");
        assert!(!out.contains("draw_rect"), "{out}");
    }

    #[test]
    fn a_type_lists_its_methods_with_ranges() {
        let (_d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let out = Nav::new().get_source(&roots, "Canvas", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("struct Canvas"), "{out}");
        assert!(out.contains("Canvas::draw"), "{out}");
        assert!(out.contains("Canvas::flush_batch"), "{out}");
    }

    #[test]
    fn references_skip_comments_and_strings_unless_asked() {
        let (_d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let mut nav = Nav::new();
        let code = nav.find_references(&roots, "flush_batch", None, false, DEFAULT_REF_LIMIT);
        assert!(code.contains("2 use(s) in 1 file(s), code only; 2 more in comments/strings in 1 file(s)"), "{code}");
        assert!(code.contains("  fn Canvas::draw 4-10\n      7  self.flush_batch();\n"), "{code}");
        assert!(code.contains("  fn Canvas::flush_batch 12-12 (definition)\n     12  fn flush_batch"), "{code}");
        assert!(!code.contains("8~"), "{code}");
        let all = nav.find_references(&roots, "flush_batch", None, true, DEFAULT_REF_LIMIT);
        assert!(all.contains("      8~ // flush_batch in a comment"), "{all}");
        assert!(all.contains("      9~ let s"), "{all}");
    }

    #[test]
    fn several_names_in_one_call() {
        let (_d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let out = Nav::new().get_source(&roots, "flush_batch|draw_rect", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("fn Canvas::flush_batch"), "{out}");
        assert!(out.contains("fn draw_rect"), "{out}");
    }

    #[test]
    fn a_long_definition_is_cut_with_a_way_to_the_rest() {
        let body: String = (0..300).map(|i| format!("    let v{i} = {i};\n")).collect();
        let src = format!("fn big() {{\n{body}}}\n");
        let (_d, roots) = tmp_root(&[("b.rs", &src)]);
        let mut nav = Nav::new();
        let out = nav.get_source(&roots, "big", None, None, 50);
        assert!(out.contains("more lines (51-302); ask again with lines=\"51-302\""), "{out}");
        let rest = nav.get_source(&roots, "big", None, Some((290, 302)), 50);
        assert!(rest.contains("  302\t}"), "{rest}");
    }

    #[test]
    fn outline_nests_methods_under_their_impl() {
        let (_d, roots) = tmp_root(&[("src/a.rs", CANVAS)]);
        let out = Nav::new().outline(&roots, "src/a.rs");
        assert!(out.contains("  impl Canvas  3-13"), "{out}");
        assert!(out.contains("    pub fn draw(&mut self)  4-10"), "{out}");
    }

    #[test]
    fn javascript_functions_and_methods_are_found() {
        let js = "export function load(a) {\n  return a;\n}\n\nclass Scene {\n  render() {\n    load(1);\n  }\n}\n\nconst tick = () => {\n  load(2);\n};\n";
        let (_d, roots) = tmp_root(&[("s.jsx", js)]);
        let mut nav = Nav::new();
        let out = nav.get_source(&roots, "Scene::render|tick", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("fn Scene::render"), "{out}");
        assert!(out.contains("fn tick"), "{out}");
        let refs = nav.find_references(&roots, "load", None, false, DEFAULT_REF_LIMIT);
        assert!(refs.contains("3 use(s)"), "{refs}");
        assert!(refs.contains("fn Scene::render"), "{refs}");
    }

    #[test]
    fn a_crate_src_root_also_covers_its_examples_and_tests() {
        let (d, _) = tmp_root(&[
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("src/lib.rs", "pub fn lib_fn() {}\n"),
            ("examples/demo.rs", "fn demo_main() { x::lib_fn(); }\n"),
            ("vendor/sdk.c", "int vendored(void) { return 0; }\n"),
        ]);
        let roots = vec![(d.path().join("src"), "t".to_string(), false)];
        let mut nav = Nav::new();
        let out = nav.get_source(&roots, "demo_main", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("fn demo_main"), "{out}");
        let refs = nav.find_references(&roots, "lib_fn", None, false, DEFAULT_REF_LIMIT);
        assert!(refs.contains("2 use(s) in 2 file(s)"), "{refs}");
        let vendored = nav.get_source(&roots, "vendored", None, None, DEFAULT_MAX_LINES);
        assert!(vendored.contains("no definition found"), "{vendored}");
    }

    const SHADER_RS: &str = r##"pub const OUTPUT_WGSL: &str = r#"
struct VOut {
    @builtin(position) pos: vec4<f32>,
};

// fn not_a_def() is commented out
@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let c = shade(in.pos.xyz);
    return vec4<f32>(c, 1.0);
}
"#;

fn build(cap: f32) -> String {
    format!("const MAX_REFL: f32 = {cap:?};\nfn shade(p: vec3<f32>) -> vec3<f32> {{ return p * MAX_REFL; }}")
}

fn tick(s: &mut u32) {
    const FADE_TICKS: u32 = 300;
    *s = (*s).min(FADE_TICKS);
}

pub enum Action {
    /// Jump up.
    Jump { height: f32 },
    Run,
}
"##;

    #[test]
    fn shader_code_in_rust_strings_is_found_and_placed() {
        let (_d, roots) = tmp_root(&[("src/shader.rs", SHADER_RS)]);
        let mut nav = Nav::new();
        let out = nav.get_source(&roots, "fs_main", None, None, DEFAULT_MAX_LINES);
        assert!(out.contains("wgsl fn fs_main — "), "{out}");
        assert!(out.contains(":7-11 (rust · t; in const OUTPUT_WGSL)"), "{out}");
        assert!(out.contains("   11\t}"), "{out}");
        let vout = nav.get_source(&roots, "VOut", None, None, DEFAULT_MAX_LINES);
        assert!(vout.contains("wgsl struct VOut") && vout.contains(":2-4 "), "{vout}");
        // A one-line format! string: `{{`/`}}` still balance, and the const is found.
        let refl = nav.get_source(&roots, "MAX_REFL|shade", None, None, DEFAULT_MAX_LINES);
        assert!(refl.contains("wgsl const MAX_REFL") && refl.contains("in fn build"), "{refl}");
        assert!(refl.contains("wgsl fn shade"), "{refl}");
        assert!(!nav.get_source(&roots, "not_a_def", None, None, 50).contains("wgsl fn"));
        // Uses inside shader strings are code uses, not comment matches.
        let refs = nav.find_references(&roots, "shade", None, false, DEFAULT_REF_LIMIT);
        assert!(refs.contains("      9  let c = shade(in.pos.xyz);"), "{refs}");
        assert!(refs.contains("2 use(s)"), "{refs}");
    }

    #[test]
    fn nested_consts_variants_and_mod_files_are_found() {
        let (_d, roots) = tmp_root(&[
            ("src/shader.rs", SHADER_RS),
            ("src/lib.rs", "mod convert;\npub mod shader;\n"),
            ("src/convert.rs", "pub fn to_beam() {}\n"),
        ]);
        let mut nav = Nav::new();
        let fade = nav.get_source(&roots, "FADE_TICKS", None, None, DEFAULT_MAX_LINES);
        assert!(fade.contains("const tick::FADE_TICKS — "), "{fade}");
        let jump = nav.get_source(&roots, "Action::Jump", None, None, DEFAULT_MAX_LINES);
        assert!(jump.contains("variant Action::Jump") && jump.contains("/// Jump up."), "{jump}");
        let m = nav.get_source(&roots, "convert", None, None, DEFAULT_MAX_LINES);
        assert!(m.contains("mod convert") && m.contains("; file ") && m.contains("convert.rs"), "{m}");
        let outline = nav.outline(&roots, "src/shader.rs");
        assert!(outline.contains("    [wgsl] fn fs_main(in: VOut) -> @location(0) vec4<f32>  7-11"), "{outline}");
        assert!(outline.contains("    [wgsl] const MAX_REFL  15-15"), "{outline}");
        assert!(!outline.contains("height"), "fields stay out of outlines: {outline}");
    }

    #[test]
    fn a_file_is_tokenized_once_per_version() {
        let (d, roots) = tmp_root(&[("a.rs", CANVAS)]);
        let mut nav = Nav::new();
        nav.find_references(&roots, "flush_batch", None, false, DEFAULT_REF_LIMIT);
        let first = nav.idents.values().next().map(|(_, m)| Arc::as_ptr(m)).unwrap();
        nav.find_references(&roots, "draw", None, false, DEFAULT_REF_LIMIT);
        assert_eq!(nav.idents.values().next().map(|(_, m)| Arc::as_ptr(m)).unwrap(), first);
        // An edit (new size) is a new version and is tokenized again.
        std::fs::write(d.path().join("a.rs"), format!("{CANVAS}\nfn extra() {{ draw_rect(); }}\n")).unwrap();
        let out = nav.find_references(&roots, "draw_rect", None, false, DEFAULT_REF_LIMIT);
        assert!(out.contains("2 use(s)"), "{out}");
        assert_ne!(nav.idents.values().next().map(|(_, m)| Arc::as_ptr(m)).unwrap(), first);
    }

    #[test]
    fn ranges_parse() {
        assert_eq!(parse_range("120-160"), Some((120, 160)));
        assert_eq!(parse_range("7"), Some((7, 7)));
        assert_eq!(parse_range("9-3"), None);
    }
}

/// A minimal temporary directory for tests, removed on drop.
#[cfg(test)]
mod tempdir_lite {
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "quartz-ctx-nav-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
