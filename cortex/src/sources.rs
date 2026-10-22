//! New code nobody listed: finding it, and getting it into the manifest.
//!
//! Both servers read `.cortex/index-sources.json` before every answer, and an
//! unstamped root is indexed on first use - so a target appended here is served
//! by the next call, with no reindex or restart. What used to be missing was the
//! appending: a crate created after the manifest was written was never listed,
//! so cortex and quartz-ctx ignored it in silence. On FlowMake that was 46 of
//! 77 crates by 2026-10-21, among them the baker being worked on that week.
//!
//! The rule, measured against that workspace before it was written:
//! - a Rust crate inside a git repository that already has an indexed root is
//!   almost certainly the same project's code. It is added automatically;
//! - anything else - a repository nothing is indexed from yet (a vendored fork),
//!   or a non-Rust project (on FlowMake these were Android Gradle wrappers and a
//!   vendored SDK's samples) - is a judgment call. The agent is told once per
//!   session to ask, and `add` / `ignore` record the answer;
//! - demos, examples, benches, tests and xtask crates are neither.
//!
//! `"auto_index": "ask"` in the manifest turns the automatic part into a
//! question too, `"off"` turns the whole thing off, and `"ignore": [...]` lists
//! path prefixes the user declined.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

const MANIFEST: &str = ".cortex/index-sources.json";

/// Deep enough for FlowMake's deepest real crate (`spacesoup/space_soup_app/
/// space_soup_hands/Cargo.toml` is depth 4); the walk prunes build output.
const MAX_DEPTH: usize = 5;

/// Files that make a directory a project in a language quartz-ctx reads.
const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
];

/// Path components that mark sample or tooling code rather than the project.
const SAMPLE_DIRS: &[&str] =
    &["demo", "demos", "example", "examples", "bench", "benches", "tests", "test", "xtask", "fuzz", "samples"];

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The project directory, relative to the workspace, forward slashes.
    pub dir: String,
    /// What a manifest target would list: `<dir>/src` for a crate, else `dir`.
    pub source: String,
    pub name: String,
    /// None for a Rust crate; the marker file otherwise.
    pub marker: Option<String>,
    /// The git repository holding it, relative ("" = the workspace itself).
    pub repo: Option<String>,
    /// A crate with a main.rs and no lib.rs: index its private items too.
    pub binary: bool,
}

/// Projects that need a person's answer, under the path that answers for them.
#[derive(Debug, Default)]
pub struct Group {
    pub prefix: String,
    pub why: String,
    pub members: Vec<Found>,
}

#[derive(Debug, Default)]
pub struct Sweep {
    /// Appended to the manifest by this sweep.
    pub added: Vec<Found>,
    pub ask: Vec<Group>,
    /// A listed root that is gone, and every same-named project found elsewhere.
    pub moved: Vec<(String, Vec<String>)>,
    pub samples_skipped: usize,
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

fn norm(p: &str) -> String {
    p.replace('\\', "/").trim_start_matches("./").trim_end_matches('/').to_string()
}

/// `a` is `b` or sits inside it. "" (the workspace) contains everything.
fn under(a: &str, b: &str) -> bool {
    b.is_empty() || a == b || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
}

fn is_sample(dir: &str) -> bool {
    dir.split('/').any(|c| SAMPLE_DIRS.contains(&c.to_ascii_lowercase().as_str()))
}

/// The nearest enclosing git repository, stopping at the workspace root.
fn repo_of(root: &Path, dir: &Path) -> Option<String> {
    let mut d = dir;
    loop {
        if d.join(".git").exists() {
            return Some(rel(root, d));
        }
        if d == root {
            return None;
        }
        d = d.parent()?;
        if !d.starts_with(root) {
            return None;
        }
    }
}

fn package_name(cargo_toml: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cargo_toml).ok()?;
    let v: toml::Value = toml::from_str(&raw).ok()?;
    v.get("package")?.get("name")?.as_str().map(str::to_string)
}

/// Every project under `root`, in path order. Build output, dependency caches
/// and hidden directories are pruned by quartz-ctx's own rules.
pub fn discover(root: &Path) -> Vec<Found> {
    let mut by_dir: BTreeMap<String, Found> = BTreeMap::new();
    let walk = walkdir::WalkDir::new(root).max_depth(MAX_DEPTH).into_iter().filter_entry(|e| {
        !quartz_ctx::parser::is_excluded_walk_entry(e)
            && !(e.depth() > 0 && e.file_type().is_dir() && e.file_name().to_string_lossy().starts_with('.'))
    });
    for entry in walk.filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let Some(dir) = entry.path().parent() else { continue };
        if dir == root {
            continue;
        }
        let file = entry.file_name().to_string_lossy();
        let dir_rel = rel(root, dir);
        let dir_name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if file == "Cargo.toml" {
            if !dir.join("src").is_dir() {
                continue; // a virtual workspace: its members are found on their own
            }
            by_dir.insert(
                dir_rel.clone(),
                Found {
                    source: format!("{dir_rel}/src"),
                    name: package_name(entry.path()).unwrap_or(dir_name),
                    marker: None,
                    repo: repo_of(root, dir),
                    binary: dir.join("src/main.rs").is_file() && !dir.join("src/lib.rs").is_file(),
                    dir: dir_rel,
                },
            );
        } else if PROJECT_MARKERS.contains(&file.as_ref()) || file.ends_with(".csproj") {
            // A crate in the same directory wins: it is what the marker wraps.
            by_dir.entry(dir_rel.clone()).or_insert_with(|| Found {
                source: dir_rel.clone(),
                name: dir_name,
                marker: Some(file.to_string()),
                repo: repo_of(root, dir),
                binary: false,
                dir: dir_rel,
            });
        }
    }
    by_dir.into_values().collect()
}

// ── the manifest ────────────────────────────────────────────────────────────

struct Manifest {
    raw: String,
    targets: Vec<(String, Option<String>)>,
    ignore: Vec<String>,
    auto_index: String,
}

fn read_manifest(root: &Path) -> Option<Manifest> {
    let raw = std::fs::read_to_string(root.join(MANIFEST)).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let targets = v
        .get("targets")?
        .as_array()?
        .iter()
        .filter_map(|t| {
            Some((norm(t.get("source")?.as_str()?), t.get("scope").and_then(Value::as_str).map(str::to_string)))
        })
        .collect();
    let ignore = v
        .get("ignore")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(norm).collect())
        .unwrap_or_default();
    let auto_index = v.get("auto_index").and_then(Value::as_str).unwrap_or("same_repo").to_string();
    Some(Manifest { raw, targets, ignore, auto_index })
}

#[derive(Serialize)]
struct NewTarget<'a> {
    source: &'a str,
    name: &'a str,
    scope: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    include_private: bool,
}

/// The `[` and `]` of a top-level key's array, found by scanning the text so
/// the rest of the file is left byte for byte as its owner wrote it.
fn top_level_array(raw: &str, key: &str) -> Option<(usize, usize)> {
    let b = raw.as_bytes();
    let (mut depth, mut i) = (0i32, 0usize);
    let (mut last_string, mut after_key, mut open) = (None::<(usize, usize)>, false, None::<usize>);
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                after_key = false;
                if depth == 1 && open.is_none() {
                    last_string = Some((start, i.min(b.len())));
                }
            }
            b':' if depth == 1 && open.is_none() => {
                after_key = last_string.is_some_and(|(s, e)| &raw[s..e] == key);
            }
            b'{' | b'[' => {
                depth += 1;
                if after_key && depth == 2 && b[i] == b'[' {
                    open = Some(i);
                }
                after_key = false;
            }
            b'}' | b']' => {
                depth -= 1;
                if let Some(o) = open {
                    if depth == 1 && b[i] == b']' {
                        return Some((o, i));
                    }
                }
            }
            b' ' | b'\n' | b'\r' | b'\t' => {}
            _ => after_key = false,
        }
        i += 1;
    }
    None
}

/// The whitespace that starts the line holding byte `at`.
fn line_indent(raw: &str, at: usize) -> &str {
    let start = raw[..at].rfind('\n').map_or(0, |n| n + 1);
    let line = &raw[start..];
    &line[..line.len() - line.trim_start().len()]
}

fn indent_lines(text: &str, indent: &str) -> String {
    text.lines().map(|l| format!("{indent}{l}")).collect::<Vec<_>>().join("\n")
}

/// Append JSON values to a top-level array, creating the key when it is
/// absent. Returns None if the result would not parse.
fn append_to_array(raw: &str, key: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        return Some(raw.to_string());
    }
    let out = if let Some((open, close)) = top_level_array(raw, key) {
        let empty = raw[open + 1..close].trim().is_empty();
        let elem_indent = if empty {
            format!("{}  ", line_indent(raw, open))
        } else {
            let first = open + 1 + (raw[open + 1..].len() - raw[open + 1..].trim_start().len());
            line_indent(raw, first).to_string()
        };
        let close_indent = if empty { line_indent(raw, open).to_string() } else { line_indent(raw, close).to_string() };
        let body: Vec<String> = items.iter().map(|i| indent_lines(i, &elem_indent)).collect();
        format!(
            "{}{}\n{}\n{}{}",
            raw[..close].trim_end(),
            if empty { "" } else { "," },
            body.join(",\n"),
            close_indent,
            &raw[close..]
        )
    } else {
        let end = raw.rfind('}')?;
        let key_indent = raw.find('"').map(|q| line_indent(raw, q).to_string()).unwrap_or_else(|| "  ".into());
        let before = raw[..end].trim_end();
        let body: Vec<String> = items.iter().map(|i| indent_lines(i, &format!("{key_indent}  "))).collect();
        format!(
            "{before}{}\n{key_indent}\"{key}\": [\n{}\n{key_indent}]\n{}",
            if before.ends_with('{') { "" } else { "," },
            body.join(",\n"),
            &raw[end..]
        )
    };
    serde_json::from_str::<Value>(&out).ok().map(|_| out)
}

/// Replace the manifest atomically, so a server reading it mid-write sees the
/// old file or the new one, never half of either.
fn write_manifest(root: &Path, text: &str) -> Result<()> {
    let path = root.join(MANIFEST);
    let tmp: PathBuf = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))
}

fn scope_for(name: &str, taken: &mut HashSet<String>) -> String {
    let base: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let base = if base.is_empty() { "src".to_string() } else { base };
    let mut scope = base.clone();
    let mut n = 2;
    while !taken.insert(scope.clone()) {
        scope = format!("{base}_{n}");
        n += 1;
    }
    scope
}

/// Append these projects as targets. Returns the new manifest text.
fn with_targets(m: &Manifest, found: &[Found]) -> Result<String> {
    let mut taken: HashSet<String> = m.targets.iter().filter_map(|(_, s)| s.clone()).collect();
    let items: Vec<String> = found
        .iter()
        .map(|f| {
            let t = NewTarget {
                source: &f.source,
                name: &f.name,
                scope: scope_for(&f.name, &mut taken),
                // Applications publish almost nothing; libraries keep their API view.
                include_private: f.binary || f.marker.is_some(),
            };
            serde_json::to_string_pretty(&t).expect("a target serialises")
        })
        .collect();
    append_to_array(&m.raw, "targets", &items).context("the manifest would not parse after adding targets")
}

// ── the sweep ───────────────────────────────────────────────────────────────

fn covered(f: &Found, m: &Manifest) -> bool {
    m.targets.iter().any(|(t, _)| under(&f.source, t) || under(&f.dir, t) || under(t, &f.dir))
}

/// Compare the workspace with its manifest, and (with `apply`) append the
/// crates the rule adds by itself. A workspace without a readable manifest is
/// left alone: there is nothing to keep in step.
pub fn sweep(root: &Path, apply: bool) -> Result<Sweep> {
    let mut out = Sweep::default();
    let Some(m) = read_manifest(root) else { return Ok(out) };
    if m.auto_index == "off" {
        return Ok(out);
    }
    let found = discover(root);
    let indexed_repos: HashSet<Option<String>> = m
        .targets
        .iter()
        .filter(|(t, _)| root.join(t).exists())
        .map(|(t, _)| repo_of(root, &root.join(t)))
        .collect();
    let others: Vec<&str> = found.iter().filter(|f| f.marker.is_some()).map(|f| f.dir.as_str()).collect();

    let mut auto = Vec::new();
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for f in &found {
        if covered(f, &m) || m.ignore.iter().any(|ig| under(&f.dir, ig)) {
            continue;
        }
        if is_sample(&f.dir) {
            out.samples_skipped += 1;
            continue;
        }
        // A non-Rust project inside another one is part of it.
        if f.marker.is_some() && others.iter().any(|o| *o != f.dir && under(&f.dir, o)) {
            continue;
        }
        let repo_indexed = indexed_repos.contains(&f.repo);
        if f.marker.is_none() && repo_indexed && m.auto_index != "ask" {
            auto.push(f.clone());
            continue;
        }
        // Answered by the repository when nothing in it is indexed yet, so one
        // reply covers a whole fork; by the directory otherwise, so declining
        // one project never declines an indexed repository.
        let (prefix, why) = match (&f.repo, repo_indexed) {
            (Some(r), false) if !r.is_empty() => (r.clone(), "a git repository nothing is indexed from yet".to_string()),
            _ if f.marker.is_some() => {
                (f.dir.clone(), format!("a {} project", f.marker.as_deref().unwrap_or("non-Rust")))
            }
            _ => (f.dir.clone(), "a crate outside every indexed repository".to_string()),
        };
        let g = groups.entry(prefix.clone()).or_insert_with(|| Group { prefix, why, members: Vec::new() });
        g.members.push(f.clone());
    }
    out.ask = groups.into_values().collect();

    // A listed root that is gone, where a crate of the same name now sits.
    let last_named = |p: &str| p.trim_end_matches("/src").rsplit('/').next().unwrap_or("").to_string();
    // Every match is named: two crates called `bake` mean the move is a
    // question, not a fact.
    for (t, _) in m.targets.iter().filter(|(t, _)| !root.join(t).exists()) {
        let name = last_named(t);
        let candidates = auto.iter().chain(out.ask.iter().flat_map(|g| g.members.iter()));
        let same: Vec<String> = candidates.filter(|f| last_named(&f.dir) == name).map(|f| f.dir.clone()).collect();
        if !same.is_empty() {
            out.moved.push((t.clone(), same));
        }
    }

    if apply && !auto.is_empty() {
        write_manifest(root, &with_targets(&m, &auto)?)?;
    }
    out.added = auto;
    Ok(out)
}

/// `add`: index every project under these paths, samples included only when
/// named exactly. Returns what was added, and the paths that matched nothing.
pub fn add(root: &Path, paths: &[String]) -> Result<(Vec<Found>, Vec<String>)> {
    let Some(m) = read_manifest(root) else { bail!("no readable {MANIFEST} in {}", root.display()) };
    let found = discover(root);
    let (mut picked, mut unmatched) = (Vec::<Found>::new(), Vec::new());
    for p in paths.iter().map(|p| norm(p)) {
        let hits: Vec<&Found> = found
            .iter()
            .filter(|f| under(&f.dir, &p) && !covered(f, &m) && (f.dir == p || !is_sample(&f.dir)))
            .collect();
        if hits.is_empty() {
            unmatched.push(p);
        }
        for f in hits {
            if !picked.iter().any(|x| x.dir == f.dir) {
                picked.push(f.clone());
            }
        }
    }
    if !picked.is_empty() {
        write_manifest(root, &with_targets(&m, &picked)?)?;
    }
    Ok((picked, unmatched))
}

/// `ignore`: stop asking about everything under these paths.
pub fn ignore(root: &Path, paths: &[String]) -> Result<Vec<String>> {
    let Some(m) = read_manifest(root) else { bail!("no readable {MANIFEST} in {}", root.display()) };
    let mut new: Vec<String> = Vec::new();
    for p in paths.iter().map(|p| norm(p)).filter(|p| !p.is_empty()) {
        if !m.ignore.contains(&p) && !new.contains(&p) {
            new.push(p);
        }
    }
    let items: Vec<String> = new.iter().map(|p| Value::String(p.clone()).to_string()).collect();
    let text = append_to_array(&m.raw, "ignore", &items).context("the manifest would not parse after adding to ignore")?;
    if !new.is_empty() {
        write_manifest(root, &text)?;
    }
    Ok(new)
}

// ── what the agent is told ──────────────────────────────────────────────────

fn names(fs: &[Found], max: usize) -> String {
    let mut s: Vec<String> = fs.iter().take(max).map(|f| f.name.clone()).collect();
    if fs.len() > max {
        s.push(format!("+{} more", fs.len() - max));
    }
    s.join(", ")
}

/// Said once by construction: a crate is added by exactly one sweep.
fn notice_added(s: &Sweep) -> String {
    if s.added.is_empty() {
        return String::new();
    }
    let list: Vec<&str> = s.added.iter().map(|f| f.source.as_str()).collect();
    format!(
        "[cortex] Now indexing {} new crate(s) found in repositories already indexed: {}. \
         The next lookup serves them.",
        s.added.len(),
        list.join(", ")
    )
}

/// What stays true until someone answers, so it is deduped per session.
fn notice_questions(s: &Sweep) -> String {
    let mut out = Vec::new();
    if !s.ask.is_empty() {
        let mut lines = vec![
            "[cortex] Code that cortex and quartz-ctx do not index yet. ASK THE USER whether to index each \
             of these, then call index_sources with add=[paths] for a yes or ignore=[paths] for a no; \
             until answered this is raised once per session:"
                .to_string(),
        ];
        for g in &s.ask {
            let what = if g.members.len() == 1 && g.members[0].dir == g.prefix {
                g.why.clone()
            } else {
                format!("{}, {} project(s): {}", g.why, g.members.len(), names(&g.members, 5))
            };
            lines.push(format!("  - {} ({what})", g.prefix));
        }
        out.push(lines.join("\n"));
    }
    if !s.moved.is_empty() {
        let mut lines = vec![
            "[cortex] Manifest roots that are gone, with same-named code elsewhere - perhaps where they went \
             (tell the user; a gone root is only skipped, so nothing breaks meanwhile):"
                .to_string(),
        ];
        for (t, same) in &s.moved {
            lines.push(format!("  - {t}: {}", same.join(", ")));
        }
        out.push(lines.join("\n"));
    }
    out.join("\n\n")
}

/// The sweep the prompt hook runs: apply what is automatic, and say what is
/// new to this session. Silent when nothing changed since the session last
/// heard, and on any failure - a prompt must never wait on this.
pub fn prompt_notice(store: &crate::memory::Store, session_id: &str, root: &Path) -> String {
    let Ok(s) = sweep(root, true) else { return String::new() };
    let mut questions = notice_questions(&s);
    if !questions.is_empty() {
        use sha2::{Digest, Sha256};
        let fp = hex::encode(Sha256::digest(questions.as_bytes()));
        let key = format!("sources_notice:{session_id}");
        if store.get_meta(&key).ok().flatten().as_deref() == Some(fp.as_str()) {
            questions.clear();
        } else {
            let _ = store.set_meta(&key, &fp);
        }
    }
    [notice_added(&s), questions].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// A plain report for the CLI and for index_sources called with no change.
pub fn report(s: &Sweep) -> String {
    let mut lines = Vec::new();
    if s.added.is_empty() && s.ask.is_empty() {
        lines.push("every project found is indexed, ignored, or a sample".to_string());
    }
    for f in &s.added {
        lines.push(format!("auto     {}  ({})", f.source, f.name));
    }
    for g in &s.ask {
        lines.push(format!("ask      {}  - {}", g.prefix, g.why));
        for f in &g.members {
            lines.push(format!("           {}  ({})", f.source, f.name));
        }
    }
    for (t, same) in &s.moved {
        lines.push(format!("moved?   {t} is missing; same name at {}", same.join(", ")));
    }
    if s.samples_skipped > 0 {
        lines.push(format!("skipped  {} demo/example/bench/test crate(s)", s.samples_skipped));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ws(PathBuf);
    impl Drop for Ws {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn crate_at(root: &Path, dir: &str, name: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    }

    /// Two repos: `engine` (indexed) and `fork` (not), plus a workspace-level
    /// stray, a sample and a gradle wrapper inside the indexed repo.
    fn ws(tag: &str) -> Ws {
        let root = std::env::temp_dir().join(format!("cortex_sources_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for repo in ["engine", "fork"] {
            std::fs::create_dir_all(root.join(repo).join(".git")).unwrap();
        }
        crate_at(&root, "engine/core", "engine-core");
        crate_at(&root, "engine/baker", "baker");
        crate_at(&root, "engine/demo", "demo");
        crate_at(&root, "fork/a", "fork_a");
        crate_at(&root, "fork/b", "fork_b");
        crate_at(&root, "fork/target/debug/build/x", "x"); // build output: never a crate
        std::fs::create_dir_all(root.join("engine/android")).unwrap();
        std::fs::write(root.join("engine/android/build.gradle"), "").unwrap();
        std::fs::create_dir_all(root.join(".cortex")).unwrap();
        std::fs::write(
            root.join(MANIFEST),
            "{\n  \"targets\": [\n    {\n      \"source\": \"engine/core/src\",\n      \"name\": \"Core\",\n      \"scope\": \"core\"\n    },\n    {\n      \"source\": \"engine/old_baker/src\",\n      \"name\": \"Gone\",\n      \"scope\": \"gone\"\n    }\n  ]\n}\n",
        )
        .unwrap();
        Ws(root)
    }

    #[test]
    fn a_crate_in_an_indexed_repo_is_added_and_a_new_repo_is_asked_about() {
        let w = ws("rule");
        let s = sweep(&w.0, true).unwrap();
        assert_eq!(s.added.iter().map(|f| f.source.as_str()).collect::<Vec<_>>(), ["engine/baker/src"]);
        let asks: Vec<&str> = s.ask.iter().map(|g| g.prefix.as_str()).collect();
        assert_eq!(asks, ["engine/android", "fork"], "{}", report(&s));
        assert_eq!(s.ask[1].members.len(), 2, "one answer covers the whole fork");
        assert_eq!(s.samples_skipped, 1);
        // Written, and served from: the target is in the file.
        let raw = std::fs::read_to_string(w.0.join(MANIFEST)).unwrap();
        assert!(raw.contains("\"source\": \"engine/baker/src\"") && raw.contains("\"scope\": \"baker\""), "{raw}");
        // Idempotent: the next sweep has nothing to add.
        assert!(sweep(&w.0, true).unwrap().added.is_empty());
    }

    #[test]
    fn the_owners_text_is_kept_byte_for_byte_and_new_entries_match_its_indent() {
        let w = ws("bytes");
        let before = std::fs::read_to_string(w.0.join(MANIFEST)).unwrap();
        sweep(&w.0, true).unwrap();
        let after = std::fs::read_to_string(w.0.join(MANIFEST)).unwrap();
        let kept = before.trim_end().trim_end_matches('}').trim_end().trim_end_matches(']').trim_end();
        assert!(after.starts_with(kept), "existing text changed:\n{after}");
        assert!(after.contains("\n    {\n      \"source\": \"engine/baker/src\""), "{after}");
    }

    #[test]
    fn answers_are_remembered() {
        let w = ws("answers");
        assert_eq!(ignore(&w.0, &["fork".into()]).unwrap(), ["fork"]);
        assert_eq!(ignore(&w.0, &["fork/".into()]).unwrap(), Vec::<String>::new(), "no duplicate");
        let (added, unmatched) = add(&w.0, &["engine/android".into(), "nowhere".into()]).unwrap();
        assert_eq!(added.len(), 1);
        assert!(added[0].marker.is_some());
        assert_eq!(unmatched, ["nowhere"]);
        let s = sweep(&w.0, true).unwrap();
        assert!(s.ask.is_empty(), "{}", report(&s));
        // A sample is added when named exactly, never through its parent.
        assert!(add(&w.0, &["engine".into()]).unwrap().0.iter().all(|f| f.dir != "engine/demo"));
        assert_eq!(add(&w.0, &["engine/demo".into()]).unwrap().0.len(), 1);
    }

    #[test]
    fn a_missing_root_points_at_the_crate_that_replaced_it() {
        let w = ws("moved");
        std::fs::rename(w.0.join("engine/baker"), w.0.join("fork/old_baker")).unwrap();
        let s = sweep(&w.0, false).unwrap();
        assert_eq!(s.moved, [("engine/old_baker/src".to_string(), vec!["fork/old_baker".to_string()])]);
        let n = notice_questions(&s);
        assert!(n.contains("perhaps where they went") && n.contains("fork/old_baker"), "{n}");
    }

    #[test]
    fn off_means_off_and_ask_asks_for_everything() {
        let w = ws("policy");
        let raw = std::fs::read_to_string(w.0.join(MANIFEST)).unwrap();
        std::fs::write(w.0.join(MANIFEST), raw.replacen("{\n", "{\n  \"auto_index\": \"ask\",\n", 1)).unwrap();
        let s = sweep(&w.0, true).unwrap();
        assert!(s.added.is_empty());
        assert!(s.ask.iter().any(|g| g.prefix == "engine/baker"), "{}", report(&s));
        std::fs::write(w.0.join(MANIFEST), raw.replacen("{\n", "{\n  \"auto_index\": \"off\",\n", 1)).unwrap();
        let s = sweep(&w.0, true).unwrap();
        assert!(s.added.is_empty() && s.ask.is_empty());
    }

    #[test]
    fn the_prompt_hook_adds_and_asks_once_per_session() {
        let w = ws("hook");
        let store = crate::test_support::TempStore::new("sources_hook").unwrap();
        let first = prompt_notice(&store, "s1", &w.0);
        assert!(first.contains("Now indexing 1 new crate") && first.contains("engine/baker/src"), "{first}");
        assert!(first.contains("ASK THE USER") && first.contains("- fork ("), "{first}");
        // Already heard: the next prompt of the session stays quiet...
        assert_eq!(prompt_notice(&store, "s1", &w.0), "");
        // ...a new session is asked again, and is not told twice about the add.
        let next = prompt_notice(&store, "s2", &w.0);
        assert!(next.contains("ASK THE USER") && !next.contains("Now indexing"), "{next}");
        // Answered: silence everywhere.
        ignore(&w.0, &["fork".into(), "engine/android".into()]).unwrap();
        assert_eq!(prompt_notice(&store, "s3", &w.0), "");
    }

    #[test]
    fn no_manifest_no_change() {
        let w = ws("bare");
        std::fs::remove_file(w.0.join(MANIFEST)).unwrap();
        let s = sweep(&w.0, true).unwrap();
        assert!(s.added.is_empty() && s.ask.is_empty());
        assert!(!w.0.join(MANIFEST).exists());
    }

    #[test]
    fn the_array_scanner_is_not_fooled_by_strings_or_nesting() {
        let raw = r#"{"note": "\"targets\": [x]", "nested": {"targets": [1]}, "targets": [{"source": "a]"}]}"#;
        let (o, c) = top_level_array(raw, "targets").unwrap();
        assert_eq!(&raw[o..=c], r#"[{"source": "a]"}]"#);
        assert!(top_level_array(raw, "ignore").is_none());
        let added = append_to_array(raw, "ignore", &["\"x\"".into()]).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&added).unwrap()["ignore"][0], "x");
        let empty = append_to_array("{\n  \"targets\": []\n}\n", "targets", &["1".into()]).unwrap();
        assert_eq!(empty, "{\n  \"targets\": [\n    1\n  ]\n}\n");
    }

    #[test]
    fn this_workspace_rule_matches_the_measurement() {
        // FlowMake as measured on 2026-10-21: skipped where it is not present.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !root.join(MANIFEST).is_file() || !root.join("spacesoup/ssstudio/bake/src").is_dir() {
            eprintln!("skipped: not the FlowMake workspace");
            return;
        }
        let s = sweep(&root.canonicalize().unwrap(), false).unwrap();
        eprintln!("{}", report(&s));
        // True before the first sweep and after it: the baker is automatic
        // (or already listed), the wgpu fork never is.
        let auto: Vec<&str> = s.added.iter().map(|f| f.dir.as_str()).collect();
        assert!(s.ask.iter().flat_map(|g| &g.members).all(|f| f.dir != "spacesoup/ssstudio/bake"), "{}", report(&s));
        assert!(auto.iter().all(|d| !d.starts_with("spacesoup/wgpu")), "a fork is never automatic");
        let fork_answered = std::fs::read_to_string(root.join(MANIFEST)).unwrap().contains("spacesoup/wgpu");
        assert!(fork_answered || s.ask.iter().any(|g| g.prefix == "spacesoup/wgpu"), "{}", report(&s));
    }
}
