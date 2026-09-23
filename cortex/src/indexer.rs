//! Indexing one source root into the store - as a REPLACEMENT of what that root
//! contributed before, not an accretion on top of it.
//!
//! `cortex index` used to upsert and never delete, which left three kinds of
//! residue that every code-backed tool then served as fact:
//!
//! * a renamed or deleted item stayed in the store forever, surviving reindex
//!   and restart alike;
//! * `code_members` had no natural key, so every run appended another copy of
//!   every field and method (9,772 rows, 5,517 distinct, on the live store);
//! * the graph passes ran over ALL units on every run, one autocommitted INSERT
//!   at a time, so re-indexing a 10-item crate took a second - far too slow to do
//!   inside a request, which is where freshness has to be decided.
//!
//! Here a root is indexed inside one transaction: its previous units are read,
//! its new units written, anything it no longer declares deleted with its
//! members, graph nodes and edges, and what changed at the API level is written
//! to `api_changes` - the journal `get_delta` reads, which needs no git.
//!
//! Extraction is quartz-ctx's parser, linked in-process, so cortex and quartz-ctx
//! read the code through one front end and cannot disagree about what a type is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::memory::Store;
use crate::model::{self, ApiGraphItem};
use crate::{cache, compressor, graph};

/// How the graph tables are brought up to date after a root is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphPass {
    /// Re-infer every edge in the store. What `cortex index` has always done.
    Global,
    /// Re-infer only edges leaving this root's units. Edges INTO a newly added
    /// unit from other roots appear at the next global pass - a thinner graph
    /// for a while, never a wrong one.
    Root,
}

/// One root to index.
#[derive(Clone, Debug)]
pub struct RootSpec {
    /// Where to read the source (absolute, or relative to the cwd).
    pub source: PathBuf,
    /// Provenance recorded on each unit: the manifest's own relative path, with
    /// forward slashes, so rows line up however the root was reached.
    pub source_root: String,
    /// Key the source fingerprint is stamped under (`source_fp:<key>`).
    pub fp_key: String,
    pub scope: Option<String>,
    pub include_private: bool,
    /// Write API changes to the journal. Only when the previous state was
    /// actually observed (a per-file snapshot exists): a catch-up over days of
    /// unobserved edits would stamp all of them "now", misdating `get_delta`
    /// and flagging knowledge written in between as outdated.
    pub journal: bool,
}

#[derive(Debug, Default)]
pub struct IndexOutcome {
    pub units: usize,
    pub members: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub graph_nodes: usize,
    pub graph_edges: usize,
    pub calls_recorded: usize,
    pub calls_edged: usize,
    pub api_graph_items: usize,
    pub api_graph_replaced: usize,
    /// Units whose text changed and were rewritten.
    pub units_rewritten: usize,
    /// Units in OTHER roots whose edges were re-inferred because they name
    /// something this root gained.
    pub cross_root_relinked: usize,
    pub elapsed: Duration,
}

/// Parse `source` with quartz-ctx and shape it as the api-graph cortex ingests.
///
/// Same parser and the same serde shape `quartz-ctx generate` writes to
/// `api-graph.json`, so the in-process path and the launcher's file path hand
/// cortex identical input.
pub fn extract_api_graph(source: &Path, include_private: bool) -> Result<Vec<ApiGraphItem>> {
    let items = quartz_ctx::parser::parse_dir_with(
        source,
        quartz_ctx::parser::ParseOptions { include_private },
    )?;
    let value = serde_json::to_value(&items)?;
    Ok(serde_json::from_value(value)?)
}

/// Create the tables this module owns. Cheap and idempotent.
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_changes (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            at          TEXT    NOT NULL,
            source_root TEXT    NOT NULL,
            unit_id     TEXT    NOT NULL,
            name        TEXT    NOT NULL,
            kind        TEXT    NOT NULL,
            change      TEXT    NOT NULL,
            detail      TEXT    NOT NULL DEFAULT '',
            location    TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_api_changes_at ON api_changes(at);
         CREATE INDEX IF NOT EXISTS idx_api_changes_name ON api_changes(name);
         CREATE TABLE IF NOT EXISTS source_file_stamps (
            source_root TEXT    NOT NULL,
            rel         TEXT    NOT NULL,
            len         INTEGER NOT NULL,
            mtime_ns    INTEGER NOT NULL,
            hash        TEXT    NOT NULL,
            seen_ns     INTEGER NOT NULL,
            PRIMARY KEY (source_root, rel)
         );",
    )?;
    Ok(())
}

/// Bounded so the journal cannot grow the store without limit.
const API_CHANGES_KEEP: i64 = 20_000;

/// Index one root. Runs in a single IMMEDIATE transaction, so a second server
/// indexing the same root waits for this one and then finds nothing to do.
///
/// `snapshot` must be taken BEFORE the source is extracted: the stamps it
/// records are what the index is claimed to reflect, so a file written during
/// extraction is left looking changed - and is picked up next time - rather than
/// stamped as current with its new content never indexed.
pub fn index_root(
    store: &Store,
    spec: &RootSpec,
    graph_items: Option<Vec<ApiGraphItem>>,
    pass: GraphPass,
    snapshot: &Snapshot,
) -> Result<IndexOutcome> {
    let conn = store.conn();
    ensure_tables(conn)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match index_root_in_tx(store, spec, graph_items, pass, snapshot) {
        Ok(out) => {
            conn.execute_batch("COMMIT")?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn index_root_in_tx(
    store: &Store,
    spec: &RootSpec,
    graph_items: Option<Vec<ApiGraphItem>>,
    pass: GraphPass,
    snapshot: &Snapshot,
) -> Result<IndexOutcome> {
    let t = Instant::now();
    let conn = store.conn();
    let scope = spec.scope.as_deref();
    let mut out = IndexOutcome::default();

    let (mut units, mut members) = compressor::compress_dir(&spec.source, scope)?;

    let mut graph_calls: Vec<(String, Vec<model::ApiGraphCall>)> = Vec::new();
    if let Some(graph_items) = graph_items {
        for gi in &graph_items {
            if gi.calls.is_empty() {
                continue;
            }
            let raw_module = gi.module_path.join("::");
            let module_path = match scope {
                Some(sc) if raw_module.is_empty() => sc.to_string(),
                Some(sc) => format!("{}::{}", sc, raw_module),
                None => raw_module,
            };
            let id = if module_path.is_empty() {
                gi.name.clone()
            } else {
                format!("{}::{}", module_path, gi.name)
            };
            graph_calls.push((id, gi.calls.clone()));
        }
        // Members too, not only units: without them every api-graph-only type
        // (every type of a non-Rust root) arrives with no fields and no variants.
        let graph_members = compressor::api_graph_members(&graph_items, scope);
        let graph_units = compressor::compress_api_graph(&graph_items, scope);
        // api-graph items take precedence: they carry full method signatures
        // with types, per-method docs and field docs.
        let graph_ids: HashSet<String> = graph_units.iter().map(|u| u.id.clone()).collect();
        out.api_graph_items = graph_ids.len();
        out.api_graph_replaced = units.iter().filter(|u| graph_ids.contains(&u.id)).count();
        units.retain(|u| !graph_ids.contains(&u.id));
        units.extend(graph_units);
        members.retain(|m| !graph_ids.contains(&m.parent_id));
        members.extend(graph_members);
    }

    // Two items can resolve to one id (cfg-gated duplicates). The last write
    // wins in the table, so let the last one win here too, consistently.
    let mut dedup: HashMap<String, usize> = HashMap::new();
    for (n, u) in units.iter().enumerate() {
        dedup.insert(u.id.clone(), n);
    }
    let mut keep: Vec<usize> = dedup.into_values().collect();
    keep.sort_unstable();
    let units: Vec<model::CodeUnit> = keep.into_iter().map(|n| units[n].clone()).collect();

    // What this root contributed before.
    let before: HashMap<String, (String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, compressed FROM code_units WHERE source_root = ?1",
        )?;
        let rows = stmt.query_map(params![spec.source_root], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let first_index = before.is_empty();

    // Which units to write. A global pass rewrites every unit of the root. A
    // root pass - the one run inside a request - writes only units whose text
    // actually changed, so an edit to one file costs that file's units' FTS
    // rows and catalog entries, not the whole crate's. (Members, edges and
    // calls are cheap and are rebuilt for the whole root either way.)
    let dirty: HashSet<&str> = units
        .iter()
        .filter(|u| {
            pass == GraphPass::Global
                || before.get(&u.id).map_or(true, |(_, _, old)| *old != u.compressed)
        })
        .map(|u| u.id.as_str())
        .collect();
    out.units_rewritten = dirty.len();

    // Files changed but not one unit's text did (a comment edit, a reformat):
    // nothing downstream can have moved, so stop here. Only once the root has
    // been through a full rewrite under this version, which is what clears the
    // duplicated members older versions left behind.
    let converged_key = format!("converged_v2:{}", spec.fp_key);
    let gone_any = before.keys().any(|id| !units.iter().any(|u| &u.id == id));
    if pass == GraphPass::Root
        && dirty.is_empty()
        && !gone_any
        && store.get_meta(&converged_key)?.is_some()
    {
        out.units = units.len();
        save_snapshot(conn, &spec.fp_key, snapshot)?;
        store.set_meta(&format!("extractor:{}", spec.fp_key), &cache::build_stamp())?;
        out.elapsed = t.elapsed();
        return Ok(out);
    }

    // Replace, not accrete: the root's members are rewritten wholesale on every
    // pass. They carry no FTS triggers, so this is cheap, and it converges a
    // store that older versions filled with duplicated members.
    conn.execute(
        "DELETE FROM code_members WHERE parent_id IN
            (SELECT id FROM code_units WHERE source_root = ?1)",
        params![spec.source_root],
    )?;

    for unit in units.iter().filter(|u| dirty.contains(u.id.as_str())) {
        store.upsert_unit_from(unit, Some(&spec.source_root))?;
        store.upsert_symbol_catalog_from_unit(unit)?;
        store.add_symbol_example_if_missing(
            &unit.id,
            &unit.module_path,
            None,
            &unit.compressed,
            "index_unit",
        )?;
    }
    let new_ids: HashSet<&str> = units.iter().map(|u| u.id.as_str()).collect();
    // A member whose parent id is not among this root's units would dangle.
    let mut member_seen: HashSet<(String, String, String, String)> = HashSet::new();
    for m in &members {
        if !new_ids.contains(m.parent_id.as_str()) {
            continue;
        }
        let key = (m.parent_id.clone(), m.kind.clone(), m.name.clone(), m.type_sig.clone());
        if member_seen.insert(key) {
            store.upsert_member(m)?;
        }
    }
    out.units = units.len();
    out.members = member_seen.len();

    // Delete what the root no longer declares.
    let gone: Vec<&String> = before.keys().filter(|id| !new_ids.contains(id.as_str())).collect();
    for id in &gone {
        delete_unit(conn, id)?;
    }

    // Journal the API-level changes. A root's first index is not a change to
    // anything, and journaling its thousand "added" rows would bury real ones.
    let now = chrono::Utc::now().to_rfc3339();
    if !first_index && spec.journal {
        let mut ins = conn.prepare(
            "INSERT INTO api_changes (at, source_root, unit_id, name, kind, change, detail, location)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for u in &units {
            let location = unit_location(&u.compressed);
            match before.get(&u.id) {
                None => {
                    out.added += 1;
                    ins.execute(params![now, spec.source_root, u.id, u.name, u.kind, "added", "", location])?;
                }
                Some((_, _, old)) => {
                    let (fb, fa) = (unit_facets(&u.kind, old), unit_facets(&u.kind, &u.compressed));
                    if fb != fa {
                        out.changed += 1;
                        let detail = quartz_ctx::incremental::describe_facet_delta(&fb, &fa).join(", ");
                        ins.execute(params![now, spec.source_root, u.id, u.name, u.kind, "changed", detail, location])?;
                    }
                }
            }
        }
        for id in &gone {
            let (name, kind, old) = &before[*id];
            out.removed += 1;
            ins.execute(params![now, spec.source_root, id, name, kind, "removed", "", unit_location(old)])?;
        }
        conn.execute(
            "DELETE FROM api_changes WHERE id <= (SELECT MAX(id) FROM api_changes) - ?1",
            params![API_CHANGES_KEEP],
        )?;
    }

    // Graph.
    match pass {
        GraphPass::Global => {
            out.graph_nodes = graph::sync_nodes(conn)?;
            let all_units = store.all_units()?;
            out.graph_edges = graph::infer_edges(conn, &all_units)?;
        }
        GraphPass::Root => {
            out.graph_nodes = graph::sync_nodes_for_root(conn, &spec.source_root)?;
            // Every unit of the root, not only the changed ones: an unchanged
            // unit's edges can still point at a name that moved.
            out.graph_edges = graph::infer_edges_for(conn, &units)?;
            // And edges INTO what this root just gained, from other roots that
            // already named it. Without this, `used_by` / `simulate_change`
            // answered "nothing depends on it" for a new item until the next
            // full reindex. (Edges into REMOVED items go with delete_unit.)
            let added: HashSet<&str> = units
                .iter()
                .filter(|u| !before.contains_key(&u.id))
                .map(|u| u.name.as_str())
                .collect();
            let dependents = graph::units_mentioning(conn, &added, &spec.source_root)?;
            out.cross_root_relinked = dependents.len();
            out.graph_edges += graph::infer_edges_for(conn, &dependents)?;
        }
    }

    // Call edges. This root's previous call rows are cleared first so a reindex
    // replaces rather than accumulates. Keyed on the unit ids being re-ingested,
    // because an empty scope prefix in a LIKE would match - and delete -
    // everything.
    if !graph_calls.is_empty() {
        // Every row a unit's calls produce has `caller` equal to the unit id or
        // under it (`m::Type::new` for unit `m::Type`), and every call edge
        // leaves the unit itself - so a unit's old rows are addressable exactly,
        // by key range, without touching its neighbours'.
        let mut del_calls = conn.prepare(
            "DELETE FROM call_graph WHERE source = 'extracted'
               AND (caller = ?1 OR (caller >= ?1 || '::' AND caller < ?1 || ':;'))",
        )?;
        let mut del_edges =
            conn.prepare("DELETE FROM graph_edges WHERE source = 'calls' AND from_id = ?1")?;
        let resolver = graph::CallResolver::for_root(conn, &spec.source_root)?;
        for (unit_id, calls) in &graph_calls {
            if !new_ids.contains(unit_id.as_str()) {
                continue;
            }
            del_calls.execute(params![unit_id])?;
            del_edges.execute(params![unit_id])?;
            let (r, e) = graph::ingest_calls_with(conn, &resolver, unit_id, calls, scope)?;
            out.calls_recorded += r;
            out.calls_edged += e;
        }
    }

    // Record what the root looked like at ingest, so staleness is measurable,
    // and bump the generation so every running server reloads its units.
    save_snapshot(conn, &spec.fp_key, snapshot)?;
    // Which build extracted this root. The parser is compiled into this binary,
    // so an extractor change is a rebuild - and a rebuild changes nothing on
    // disk that the ladder would notice. Without this, improved extraction
    // never reached a store whose source had not moved.
    store.set_meta(&format!("extractor:{}", spec.fp_key), &cache::build_stamp())?;
    store.set_meta(&converged_key, "1")?;
    bump_generation(conn)?;

    out.elapsed = t.elapsed();
    Ok(out)
}

/// Remove one unit and everything hanging off it.
pub fn delete_unit(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM code_members WHERE parent_id = ?1", params![id])?;
    // Member child nodes (`<unit>::<kind>:<name>`) cascade their own edges.
    // A key range, not LIKE: `_` in an id is a LIKE wildcard, and a range can
    // use the primary key where LIKE and substr() scan the table.
    conn.execute(
        "DELETE FROM graph_nodes
          WHERE id >= ?1 || '::' AND id < ?1 || ':;'
            AND instr(substr(id, length(?1) + 3), '::') = 0
            AND instr(substr(id, length(?1) + 3), ':') > 0",
        params![id],
    )?;
    conn.execute("DELETE FROM graph_edges WHERE from_id = ?1 OR to_id = ?1", params![id])?;
    conn.execute(
        "DELETE FROM call_graph WHERE source = 'extracted'
           AND (caller = ?1 OR (caller >= ?1 || '::' AND caller < ?1 || ':;'))",
        params![id],
    )?;
    conn.execute("DELETE FROM graph_nodes WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM symbol_catalog WHERE symbol_name = ?1", params![id])?;
    conn.execute(
        "DELETE FROM symbol_examples WHERE symbol_name = ?1 AND source_tier = 'index_unit'",
        params![id],
    )?;
    conn.execute("DELETE FROM code_units WHERE id = ?1", params![id])?;
    Ok(())
}

/// A counter every index run increments. Servers compare it per request and
/// reload their in-memory units when it moves - including when ANOTHER process
/// (the launcher, or a second session's server) did the indexing.
pub fn bump_generation(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('index_generation', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        [],
    )?;
    Ok(())
}

pub fn generation(conn: &Connection) -> i64 {
    conn.query_row("SELECT value FROM meta WHERE key = 'index_generation'", [], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

fn unit_location(compressed: &str) -> Option<String> {
    compressed
        .lines()
        .find_map(|l| l.strip_prefix("at: "))
        .map(|s| s.trim().to_string())
}

/// The API-relevant facets of a unit's compressed text.
///
/// `at: file:line` and `//` doc lines are dropped: an edit ABOVE an item moves
/// its line number, and treating that as a change would flag every item below
/// every edit. Facet names match quartz-ctx's (`signature`, `field x`,
/// `method m`, `variant V`, `impl T`) so both servers describe a change alike.
pub fn unit_facets(kind: &str, compressed: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut sig: Vec<&str> = Vec::new();
    let mut in_sig = false;
    let mut in_variants = false;
    for line in compressed.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && out.is_empty() && sig.is_empty() {
            continue; // header
        }
        if trimmed.starts_with("//") || line.starts_with("at: ") {
            continue;
        }
        let label = |p: &str| line.starts_with(p);
        if label("sig: ") {
            in_sig = true;
            in_variants = false;
            sig.push(line["sig: ".len()..].trim());
            continue;
        }
        if label("fields:") || label("methods:") || label("variants:") || label("impl:") {
            in_sig = false;
            in_variants = label("variants:");
        }
        if in_sig {
            sig.push(trimmed);
            continue;
        }
        if let Some(rest) = line.strip_prefix("fields:") {
            let rest = rest.split(" // ").next().unwrap_or(rest);
            for f in split_top_level(rest, ',') {
                out.push(format!("field {}", f.trim()));
            }
        } else if let Some(rest) = line.strip_prefix("methods:") {
            for m in rest.split('|').map(str::trim).filter(|m| !m.is_empty()) {
                let name: String =
                    m.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                out.push(format!("method {name} = {m}"));
            }
        } else if let Some(rest) = line.strip_prefix("impl:") {
            for t in rest.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                out.push(format!("impl {t}"));
            }
        } else if in_variants && line.starts_with("  ") {
            let v = trimmed.rsplit("::").next().unwrap_or(trimmed);
            out.push(format!("variant {v}"));
        }
    }
    // A struct's or enum's `sig` restates its fields/variants; comparing it too
    // would report every field change twice.
    if !matches!(kind, "struct" | "enum") && !sig.is_empty() {
        out.push(format!("signature {}", sig.join(" ")));
    }
    out.sort();
    out
}

fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth <= 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

// ── Keeping the store current from a running server ─────────────────────────

/// One manifest target, as the server sees it.
#[derive(Clone, Debug)]
pub struct Target {
    pub source: String,
    pub scope: Option<String>,
    pub include_private: bool,
}

/// Read `.cortex/index-sources.json`. Missing or malformed yields nothing.
pub fn manifest_targets(repo_root: &Path) -> Vec<Target> {
    let path = repo_root.join(".cortex").join("index-sources.json");
    let Ok(raw) = std::fs::read(&path) else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else { return Vec::new() };
    let Some(targets) = v.get("targets").and_then(|t| t.as_array()) else { return Vec::new() };
    targets
        .iter()
        .filter_map(|t| {
            Some(Target {
                source: t.get("source")?.as_str()?.to_string(),
                scope: t.get("scope").and_then(|s| s.as_str()).map(str::to_string),
                include_private: t.get("include_private").and_then(|b| b.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

// ── The check ladder ─────────────────────────────────────────────────────────
//
// "Has this root changed?" is answered by the cheapest test that can settle
// it, escalating only for what that test could not clear:
//
//   1. metadata  - every file's path, size and nanosecond mtime, hashed into
//                  one fingerprint. The usual answer, a stat walk (~ms).
//   2. content   - for files whose size/mtime moved, or that were "racily
//                  clean" when last read, the bytes are hashed and compared.
//                  Settles a touch, a no-op save, a formatter that changed
//                  nothing - which used to cost a full re-parse.
//   3. parse     - some file's content changed: the root is re-extracted and
//                  each unit's text compared. Settles a comment or layout edit.
//   4. api       - units changed: their API facets are compared, and only real
//                  API differences reach the change journal.
//
// Each rung runs only on what the rung below could not clear, so the ladder is
// never slower than jumping straight to the rung that decides, and usually
// much faster.

/// Nanoseconds a file must have been stable before a matching size+mtime is
/// trusted. Inside it, a second write in the same timestamp tick could leave
/// both unchanged, so the content is checked instead (git's "racy clean").
const RACY_WINDOW_NS: i128 = 3_000_000_000;

#[derive(Debug, Clone)]
pub struct FileStamp {
    pub rel: String,
    pub len: u64,
    pub mtime_ns: i64,
    pub hash: String,
    pub seen_ns: i64,
}

impl FileStamp {
    fn racy(&self) -> bool {
        self.mtime_ns as i128 + RACY_WINDOW_NS > self.seen_ns as i128
    }
}

/// What a root looked like at one moment, file by file.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub stamps: Vec<FileStamp>,
    pub fp: String,
}

impl Snapshot {
    pub fn racy(&self) -> bool {
        self.stamps.iter().any(FileStamp::racy)
    }
}

/// How far up the ladder one root's check had to go.
#[derive(Debug, Default, Clone)]
pub struct RootCheck {
    pub source: String,
    pub files: usize,
    /// Cleared by size + mtime alone.
    pub by_stamp: usize,
    /// Read and hashed.
    pub rehashed: usize,
    /// Whose content actually differed (including added and removed files).
    pub content_changed: usize,
    pub reparsed: bool,
    pub units_rewritten: usize,
    pub api_changes: usize,
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn load_stamps(conn: &Connection, key: &str) -> Result<HashMap<String, FileStamp>> {
    ensure_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT rel, len, mtime_ns, hash, seen_ns FROM source_file_stamps WHERE source_root = ?1",
    )?;
    let rows = stmt.query_map(params![key], |r| {
        Ok(FileStamp {
            rel: r.get(0)?,
            len: r.get::<_, i64>(1)? as u64,
            mtime_ns: r.get(2)?,
            hash: r.get(3)?,
            seen_ns: r.get(4)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.rel.clone(), row);
    }
    Ok(out)
}

/// Rung 2: walk `dir`, trusting `prior` where size+mtime match and the file is
/// not racy, hashing everything else. Returns the snapshot and how many files'
/// content differs from `prior` (added and removed files count).
pub fn take_snapshot(
    dir: &Path,
    prior: &HashMap<String, FileStamp>,
    check: &mut RootCheck,
) -> (Snapshot, usize) {
    use sha2::{Digest, Sha256};
    let live = cache::walk_source(dir);
    let now = now_ns();
    let mut stamps = Vec::with_capacity(live.len());
    let mut changed = 0usize;
    for (rel, len, mtime) in &live {
        let mtime_ns = *mtime as i64;
        if let Some(p) = prior.get(rel) {
            if p.len == *len && p.mtime_ns == mtime_ns && !p.racy() {
                check.by_stamp += 1;
                stamps.push(p.clone());
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(dir.join(rel)) else { continue };
        let hash = hex::encode(Sha256::digest(&bytes));
        check.rehashed += 1;
        if prior.get(rel).map(|p| p.hash.as_str()) != Some(hash.as_str()) {
            changed += 1;
        }
        stamps.push(FileStamp { rel: rel.clone(), len: *len, mtime_ns, hash, seen_ns: now });
    }
    let live_set: HashSet<&str> = live.iter().map(|(r, _, _)| r.as_str()).collect();
    changed += prior.keys().filter(|k| !live_set.contains(k.as_str())).count();
    check.files = live.len();
    check.content_changed = changed;
    (Snapshot { stamps, fp: cache::fingerprint_of(&live) }, changed)
}

/// Record a snapshot as what the index now reflects.
fn save_snapshot(conn: &Connection, key: &str, snap: &Snapshot) -> Result<()> {
    ensure_tables(conn)?;
    conn.execute("DELETE FROM source_file_stamps WHERE source_root = ?1", params![key])?;
    {
        let mut ins = conn.prepare(
            "INSERT INTO source_file_stamps (source_root, rel, len, mtime_ns, hash, seen_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for f in &snap.stamps {
            ins.execute(params![key, f.rel, f.len as i64, f.mtime_ns, f.hash, f.seen_ns])?;
        }
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![format!("source_fp:{key}"), snap.fp],
    )?;
    if snap.racy() {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![format!("source_racy:{key}")],
        )?;
    } else {
        conn.execute("DELETE FROM meta WHERE key = ?1", params![format!("source_racy:{key}")])?;
    }
    Ok(())
}

/// One line saying how deep a check went - the evidence behind "nothing changed".
pub fn describe_checks(checks: &[RootCheck]) -> String {
    let files: usize = checks.iter().map(|c| c.files).sum();
    let by_stamp: usize = checks.iter().map(|c| c.by_stamp).sum();
    let rehashed: usize = checks.iter().map(|c| c.rehashed).sum();
    let differed: usize = checks.iter().map(|c| c.content_changed).sum();
    let reparsed = checks.iter().filter(|c| c.reparsed).count();
    let units: usize = checks.iter().map(|c| c.units_rewritten).sum();
    let api: usize = checks.iter().map(|c| c.api_changes).sum();
    format!(
        "verified {files} files in {} roots: {by_stamp} unchanged by size+mtime, {rehashed} \
         re-hashed ({differed} differed), {reparsed} root(s) re-parsed ({units} units rewritten), \
         {api} API change(s)",
        checks.len()
    )
}

static LAST_CHECK: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// `describe_checks` of the most recent `refresh_stale` in this process.
pub fn last_check() -> Option<String> {
    LAST_CHECK.lock().ok().and_then(|g| g.clone())
}

/// What `refresh_stale` did.
#[derive(Debug, Default)]
pub struct RefreshSummary {
    pub refreshed: Vec<(String, IndexOutcome)>,
    pub failed: Vec<(String, String)>,
    pub pruned_orphans: usize,
    pub checks: Vec<RootCheck>,
}

impl RefreshSummary {
    pub fn changed_anything(&self) -> bool {
        !self.refreshed.is_empty() || self.pruned_orphans > 0
    }
}

/// Bring every manifest root up to date, climbing the check ladder per root.
///
/// Called before a code-backed answer, so an edit is reflected in the very
/// next lookup - the lookup an agent makes to check the edit. Each re-index
/// runs in its own IMMEDIATE transaction and re-checks under that lock, so a
/// second server that raced to the same root waits, then finds the stamp
/// current and skips it rather than doing the work twice.
///
/// Also prunes units whose root has left the manifest, once per manifest edit.
pub fn refresh_stale(store: &Store, repo_root: &Path) -> RefreshSummary {
    let mut summary = RefreshSummary::default();
    let targets = manifest_targets(repo_root);
    if targets.is_empty() {
        return summary;
    }
    let conn = store.conn();
    if let Err(e) = ensure_tables(conn) {
        summary.failed.push(("*".into(), format!("{e:#}")));
        return summary;
    }

    for t in &targets {
        let dir = repo_root.join(&t.source);
        if !dir.is_dir() {
            continue;
        }
        let key = t.source.replace('\\', "/");
        let mut check = RootCheck { source: key.clone(), ..RootCheck::default() };

        // Rung 0: was this root extracted by the parser this binary carries?
        // If not, the source has not changed but what we would read from it
        // has, so the cheap rungs cannot settle it.
        // No stamp counts as changed: a root indexed before this existed was
        // read by some older extractor, which is exactly the case to catch.
        let extractor_changed = store
            .get_meta(&format!("extractor:{key}"))
            .ok()
            .flatten()
            .as_deref()
            != Some(cache::build_stamp().as_str());

        // Rung 1: metadata.
        let live = cache::walk_source(&dir);
        let fp = cache::fingerprint_of(&live);
        let stamped = store.get_meta(&format!("source_fp:{key}")).ok().flatten();
        let racy = store.get_meta(&format!("source_racy:{key}")).ok().flatten();
        if !extractor_changed && stamped.as_deref() == Some(fp.as_str()) && racy.is_none() {
            check.files = live.len();
            check.by_stamp = live.len();
            summary.checks.push(check);
            continue;
        }

        // Rung 2: content.
        let prior = load_stamps(conn, &key).unwrap_or_default();
        let (snap, changed) = take_snapshot(&dir, &prior, &mut check);
        if changed == 0 && !prior.is_empty() && !extractor_changed {
            let saved = conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(anyhow::Error::from)
                .and_then(|_| save_snapshot(conn, &key, &snap));
            match saved {
                Ok(()) => {
                    let _ = conn.execute_batch("COMMIT");
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    summary.failed.push((key.clone(), format!("{e:#}")));
                }
            }
            summary.checks.push(check);
            continue;
        }

        // Rungs 3 and 4: parse, then compare API facets (inside index_root).
        let spec = RootSpec {
            source: dir.clone(),
            source_root: key.clone(),
            fp_key: key.clone(),
            scope: t.scope.clone(),
            // The launcher extracts every root with --include-private; match it
            // so the in-process refresh and `reindex` produce the same rows.
            include_private: true,
            // Not after an extractor change: what moved is how the code is
            // read, not the code, and journaling it would report extraction
            // improvements as API changes and flag knowledge as outdated.
            journal: !prior.is_empty() && !extractor_changed,
        };
        // Extract before taking the write lock, so the lock is held only for
        // the database work.
        let graph = match extract_api_graph(&dir, spec.include_private) {
            Ok(g) => Some(g),
            Err(e) => {
                summary.failed.push((key.clone(), format!("extraction: {e:#}")));
                None
            }
        };
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            summary.failed.push((key.clone(), format!("{e}")));
            summary.checks.push(check);
            continue;
        }
        // Re-check under the write lock: another server may have just stamped
        // exactly this state.
        let now_fp = store.get_meta(&format!("source_fp:{key}")).ok().flatten();
        let now_racy = store.get_meta(&format!("source_racy:{key}")).ok().flatten();
        if !extractor_changed
            && now_fp.as_deref() == Some(snap.fp.as_str())
            && now_racy.is_none()
            && now_fp != stamped
        {
            let _ = conn.execute_batch("COMMIT");
            summary.checks.push(check);
            continue;
        }
        check.reparsed = true;
        match index_root_in_tx(store, &spec, graph, GraphPass::Root, &snap) {
            Ok(out) => {
                if let Err(e) = conn.execute_batch("COMMIT") {
                    summary.failed.push((key, format!("commit: {e}")));
                } else {
                    check.units_rewritten = out.units_rewritten;
                    check.api_changes = out.added + out.removed + out.changed;
                    if out.units_rewritten > 0 || out.added + out.removed > 0 {
                        summary.refreshed.push((key, out));
                    }
                }
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                summary.failed.push((key, format!("{e:#}")));
            }
        }
        summary.checks.push(check);
    }
    if let Ok(mut g) = LAST_CHECK.lock() {
        *g = Some(describe_checks(&summary.checks));
    }

    // Orphans: roots that left the manifest. Their rows otherwise stay served
    // with no signal (279 units of scene_editor_web/frontend/src did, for a
    // month). Checked only when the manifest's bytes change.
    if let Ok(raw) = std::fs::read(repo_root.join(".cortex").join("index-sources.json")) {
        use sha2::{Digest, Sha256};
        let fp = hex::encode(Sha256::digest(&raw));
        if store.get_meta("manifest_fp").ok().flatten().as_deref() != Some(fp.as_str()) {
            let keep: Vec<String> = targets.iter().map(|t| t.source.replace('\\', "/")).collect();
            if let Ok(n) = store.prune_orphan_units(&keep) {
                summary.pruned_orphans = n;
                if n > 0 {
                    let _ = bump_generation(store.conn());
                }
            }
            let _ = store.set_meta("manifest_fp", &fp);
        }
    }
    summary
}

// ── The change journal, read back ───────────────────────────────────────────

static SESSION_START: std::sync::OnceLock<chrono::DateTime<chrono::Utc>> = std::sync::OnceLock::new();

/// Record when this server process began serving. A host starts one server per
/// session, so "since the session started" is "since this".
pub fn mark_session_start() {
    let _ = SESSION_START.set(chrono::Utc::now());
}

pub fn session_start() -> chrono::DateTime<chrono::Utc> {
    *SESSION_START.get_or_init(chrono::Utc::now)
}

/// One item's NET change over a window: added then changed is `added`, added
/// then removed is nothing, changed twice is one `changed` with both details.
#[derive(Debug, Clone)]
pub struct NetChange {
    pub unit_id: String,
    pub name: String,
    pub kind: String,
    pub change: &'static str,
    pub detail: Vec<String>,
    pub location: Option<String>,
    pub source_root: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Parse a `since` argument: `session`, an RFC 3339 timestamp, or a relative
/// `<N>m` / `<N>h` / `<N>d`. None when it is none of those (a git ref, say).
pub fn parse_since(since: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = since.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("session") {
        return Some(session_start());
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().ok()?;
    let d = match unit {
        "m" => chrono::Duration::minutes(n),
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        _ => return None,
    };
    Some(chrono::Utc::now() - d)
}

/// Net API changes journaled at or after `since`, newest item first.
pub fn net_changes_since(
    conn: &Connection,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<NetChange>> {
    ensure_tables(conn)?;
    // Stored `at` is RFC 3339 text whose fractional width varies, so the SQL
    // bound is a coarse prefilter a second early and the exact cut is made on
    // parsed times - comparing the text directly misorders within a second.
    let coarse = (since - chrono::Duration::seconds(1)).format("%Y-%m-%dT%H:%M:%S").to_string();
    let mut stmt = conn.prepare(
        "SELECT at, source_root, unit_id, name, kind, change, detail, location
           FROM api_changes WHERE at >= ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![coarse], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
        ))
    })?;

    // unit_id -> (first change, net)
    let mut order: Vec<String> = Vec::new();
    let mut net: HashMap<String, (String, NetChange)> = HashMap::new();
    for row in rows {
        let (at, root, id, name, kind, change, detail, location) = row?;
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&at) else { continue };
        let at = at.with_timezone(&chrono::Utc);
        if at < since {
            continue;
        }
        let detail: Vec<String> =
            detail.split(", ").filter(|d| !d.is_empty()).map(str::to_string).collect();
        match net.get_mut(&id) {
            None => {
                order.push(id.clone());
                let c: &'static str = match change.as_str() {
                    "added" => "added",
                    "removed" => "removed",
                    _ => "changed",
                };
                net.insert(
                    id.clone(),
                    (change, NetChange {
                        unit_id: id, name, kind, change: c, detail, location,
                        source_root: root, at,
                    }),
                );
            }
            Some((first, n)) => {
                n.at = at;
                if location.is_some() {
                    n.location = location;
                }
                n.kind = kind;
                n.change = match (first.as_str(), change.as_str()) {
                    ("added", "removed") => "",       // transient: never existed
                    ("added", _) => "added",
                    ("removed", "added") => "changed", // deleted and restored
                    (_, "removed") => "removed",
                    _ => "changed",
                };
                for d in detail {
                    if !n.detail.contains(&d) {
                        n.detail.push(d);
                    }
                }
            }
        }
    }
    let mut out: Vec<NetChange> = order
        .into_iter()
        .filter_map(|id| net.remove(&id).map(|(_, n)| n))
        .filter(|n| !n.change.is_empty())
        .collect();
    out.sort_by(|a, b| b.at.cmp(&a.at));
    Ok(out)
}

/// Render net changes compactly: one line each, grouped by root, capped.
pub fn render_changes(changes: &[NetChange], cap: usize) -> String {
    let mut by_root: Vec<(&str, Vec<&NetChange>)> = Vec::new();
    for c in changes.iter().take(cap) {
        match by_root.iter_mut().find(|(r, _)| *r == c.source_root) {
            Some((_, v)) => v.push(c),
            None => by_root.push((c.source_root.as_str(), vec![c])),
        }
    }
    let mut s = String::new();
    for (root, items) in by_root {
        s.push_str(&format!("{root}\n"));
        for c in items {
            let sigil = match c.change {
                "added" => '+',
                "removed" => '-',
                _ => '~',
            };
            s.push_str(&format!("  {sigil} {} ({})", c.unit_id, c.kind));
            if !c.detail.is_empty() {
                s.push_str(&format!(" {}", c.detail.join(", ")));
            }
            if let Some(loc) = &c.location {
                if c.change != "removed" {
                    s.push_str(&format!(" @ {loc}"));
                }
            }
            s.push('\n');
        }
    }
    if changes.len() > cap {
        s.push_str(&format!("... and {} more (raise `max_changes`)\n", changes.len() - cap));
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn net_changes_collapse_a_window_to_what_it_left_behind() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(5);
        let at = |m: i64| (t0 + chrono::Duration::minutes(m)).to_rfc3339();
        let rows = [
            (at(1), "a::Tmp", "added", ""),
            (at(2), "a::Tmp", "removed", ""),
            (at(1), "a::New", "added", ""),
            (at(3), "a::New", "changed", "+field x"),
            (at(1), "a::Old", "changed", "~signature"),
            (at(2), "a::Old", "changed", "-method f"),
            (at(2), "a::Gone", "removed", ""),
        ];
        for (at, id, change, detail) in rows {
            conn.execute(
                "INSERT INTO api_changes (at, source_root, unit_id, name, kind, change, detail)
                 VALUES (?1, 'src', ?2, ?2, 'struct', ?3, ?4)",
                params![at, id, change, detail],
            )
            .unwrap();
        }
        let net = net_changes_since(&conn, t0).unwrap();
        let got: HashMap<&str, (&str, Vec<String>)> =
            net.iter().map(|n| (n.unit_id.as_str(), (n.change, n.detail.clone()))).collect();
        assert!(!got.contains_key("a::Tmp"), "added then removed is no change");
        assert_eq!(got["a::New"].0, "added");
        assert_eq!(got["a::Old"], ("changed", vec!["~signature".into(), "-method f".into()]));
        assert_eq!(got["a::Gone"].0, "removed");
        // A window starting after everything sees nothing.
        assert!(net_changes_since(&conn, chrono::Utc::now()).unwrap().is_empty());
    }

    #[test]
    fn since_accepts_session_timestamps_and_relative_spans() {
        assert!(parse_since("session").is_some());
        assert!(parse_since("2026-09-23T04:00:00Z").is_some());
        let h = parse_since("2h").unwrap();
        assert!((chrono::Utc::now() - h).num_minutes() >= 119);
        assert!(parse_since("HEAD~3").is_none(), "a git ref is not a time");
    }

    use super::*;

    fn ladder_fixture(tag: &str) -> (crate::test_support::TempStore, crate::test_support::TempDir) {
        let store = crate::test_support::TempStore::new(tag).unwrap();
        let ws = crate::test_support::TempDir::new(tag).unwrap();
        std::fs::create_dir_all(ws.join(".cortex")).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(
            ws.join(".cortex/index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":"t"}]}"#,
        )
        .unwrap();
        std::fs::write(ws.join("src/lib.rs"), "pub fn alpha(x: u8) -> u8 { x }\n").unwrap();
        (store, ws)
    }

    fn only(s: &RefreshSummary) -> RootCheck {
        assert_eq!(s.checks.len(), 1, "{s:?}");
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        s.checks[0].clone()
    }

    /// Age a file past the racy window without touching its content.
    fn settle(path: &std::path::Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago)).unwrap();
    }

    #[test]
    fn the_ladder_stops_at_the_first_rung_that_can_settle_it() {
        let (store, ws) = ladder_fixture("ladder");
        let lib = ws.join("src/lib.rs");
        settle(&lib, 60);

        // First sight: nothing stamped, so it parses.
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(c.reparsed);

        // Rung 1: nothing moved - settled by metadata alone, nothing read.
        let c = only(&refresh_stale(&store, ws.path()));
        assert_eq!((c.by_stamp, c.rehashed, c.reparsed), (1, 0, false), "{c:?}");

        // Rung 2: touched, same bytes - settled by a hash, no parse.
        settle(&lib, 30);
        let c = only(&refresh_stale(&store, ws.path()));
        assert_eq!((c.rehashed, c.content_changed, c.reparsed), (1, 0, false), "{c:?}");

        // Rung 3: content changed, API did not (a body edit) - parsed, but
        // not one unit rewritten and nothing journaled.
        std::fs::write(&lib, "pub fn alpha(x: u8) -> u8 { x + 0 }\n").unwrap();
        settle(&lib, 20);
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(c.reparsed, "{c:?}");
        assert_eq!(c.api_changes, 0, "{c:?}");

        // Rung 4: the API changed - journaled.
        std::fs::write(&lib, "pub fn alpha(x: u16) -> u8 { x as u8 }\n").unwrap();
        settle(&lib, 10);
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(c.reparsed && c.api_changes == 1, "{c:?}");
        let net = net_changes_since(store.conn(), chrono::Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(net.len(), 1);
        assert_eq!(net[0].change, "changed");
    }

    /// A write that leaves size and mtime exactly as they were is invisible to
    /// rung 1. It is caught only because the file was racy when last read.
    #[test]
    fn a_write_hidden_from_metadata_is_caught_while_racy() {
        let (store, ws) = ladder_fixture("racy");
        let lib = ws.join("src/lib.rs");
        refresh_stale(&store, ws.path()); // read just after writing: racy
        let mtime = std::fs::metadata(&lib).unwrap().modified().unwrap();
        std::fs::write(&lib, "pub fn gamma(x: u8) -> u8 { x }\n").unwrap(); // same length
        std::fs::File::options().write(true).open(&lib).unwrap().set_modified(mtime).unwrap();
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(c.reparsed && c.api_changes == 2, "alpha removed + gamma added: {c:?}");
    }

    /// A new item that ANOTHER root already mentions must gain its incoming
    /// edges at once: `simulate_change` and `used_by` read exactly those, and
    /// "nothing depends on this" is the wrong answer to act on.
    #[test]
    fn a_new_item_gains_edges_from_other_roots_already_naming_it() {
        let store = crate::test_support::TempStore::new("xroot").unwrap();
        let ws = crate::test_support::TempDir::new("xroot").unwrap();
        std::fs::create_dir_all(ws.join(".cortex")).unwrap();
        std::fs::create_dir_all(ws.join("a")).unwrap();
        std::fs::create_dir_all(ws.join("b")).unwrap();
        std::fs::write(
            ws.join(".cortex/index-sources.json"),
            r#"{"targets":[{"source":"a","name":"a","scope":"a"},{"source":"b","name":"b","scope":"b"}]}"#,
        )
        .unwrap();
        std::fs::write(ws.join("a/lib.rs"), "pub struct Bar;\n").unwrap();
        std::fs::write(ws.join("b/lib.rs"), "pub struct User { pub f: Foo }\n").unwrap();
        refresh_stale(&store, ws.path());

        std::fs::write(ws.join("a/lib.rs"), "pub struct Bar;\npub struct Foo;\n").unwrap();
        let s = refresh_stale(&store, ws.path());
        assert!(s.failed.is_empty(), "{:?}", s.failed);

        let users: Vec<String> = crate::graph::used_by(store.conn(), "a::Foo")
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(users, ["b::User"], "b::User names Foo but no edge reached the new a::Foo");
    }

    /// A new extractor must re-read unchanged source, and must not journal
    /// what it reads differently as a change to the code.
    #[test]
    fn an_extractor_change_reparses_without_journaling() {
        let (store, ws) = ladder_fixture("extractor");
        let lib = ws.join("src/lib.rs");
        settle(&lib, 60);
        refresh_stale(&store, ws.path());
        store.conn().execute("DELETE FROM meta WHERE key = 'extractor:src'", []).unwrap();
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(c.reparsed, "an unchanged tree must still be re-read by a new extractor: {c:?}");
        assert_eq!(store.get_meta("extractor:src").unwrap().unwrap(), cache::build_stamp());
        let c = only(&refresh_stale(&store, ws.path()));
        assert!(!c.reparsed, "and only once: {c:?}");
        let n: i64 = store.conn().query_row("SELECT COUNT(*) FROM api_changes", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_line_move_is_not_an_api_change() {
        let a = "[fn: f (m)]\nat: a.rs:10\nsig: fn f (x : u8) -> u8";
        let b = "[fn: f (m)]\n// now documented\nat: a.rs:42\nsig: fn f (x : u8) -> u8";
        assert_eq!(unit_facets("fn", a), unit_facets("fn", b));
    }

    #[test]
    fn struct_facets_name_fields_and_methods_not_the_sig_body() {
        let c = "[struct: A (m)]\nat: a.rs:1\nsig: pub struct A {\n    pub x: Vec <u8>,\n}\nfields: x: Vec <u8>, y: HashMap <K, V> // doc\nmethods: len (& self) -> usize | push (& mut self , v : u8)\nimpl: Default, Clone";
        let f = unit_facets("struct", c);
        assert!(f.contains(&"field x: Vec <u8>".to_string()), "{f:?}");
        assert!(f.contains(&"field y: HashMap <K, V>".to_string()), "{f:?}");
        assert!(f.contains(&"method len = len (& self) -> usize".to_string()), "{f:?}");
        assert!(f.contains(&"impl Clone".to_string()), "{f:?}");
        assert!(!f.iter().any(|x| x.starts_with("signature")), "{f:?}");
    }

    #[test]
    fn enum_variants_are_facets() {
        let c = "[enum: E (m)]\nat: e.rs:1\nsig: pub enum E\nvariants:\n  E::A\n  E::B\nimpl: Default";
        let f = unit_facets("enum", c);
        assert!(f.contains(&"variant A".to_string()) && f.contains(&"variant B".to_string()), "{f:?}");
    }
}
