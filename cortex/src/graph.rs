use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::model::{CodeUnit, GraphEdge, GraphNode, RelationType};

/// Mirror `code_units` into `graph_nodes`.
///
/// Upserts in place — deliberately NOT `INSERT OR REPLACE`. SQLite implements
/// OR REPLACE as DELETE-then-INSERT, and `graph_edges` references this table
/// `ON DELETE CASCADE` with `PRAGMA foreign_keys=ON`, so replacing every node
/// silently deleted every edge attached to it.
///
/// That went unnoticed for as long as the only edges were the ones
/// `infer_edges` rebuilds from scratch on the very next line. It surfaced the
/// moment a durable edge type arrived: `calls` edges were inserted for all
/// eleven indexed sources, and only the last source's survived, because each
/// subsequent `sync_nodes` cascaded the earlier ones away.
pub fn sync_nodes(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "INSERT INTO graph_nodes (id, kind, name, module_path)
         SELECT id, kind, name, module_path FROM code_units
         WHERE true
         ON CONFLICT(id) DO UPDATE SET
             kind        = excluded.kind,
             name        = excluded.name,
             module_path = excluded.module_path",
        [],
    )?;
    Ok(n)
}

pub fn infer_edges(conn: &Connection, units: &[CodeUnit]) -> Result<usize> {
    conn.execute("DELETE FROM graph_edges WHERE source = 'inferred'", [])?;

    let by_name = node_name_to_id(conn)?;
    let mut seen = HashSet::<(String, String, String)>::new();
    let mut inserted = 0usize;

    for unit in units {
        inserted += infer_impl_edges(conn, unit, &by_name, &mut seen)?;
        inserted += infer_uses_edges(conn, unit, &by_name, &mut seen)?;
        inserted += infer_derived_edges(conn, unit, &mut seen)?;
    }

    Ok(inserted)
}

/// Record extracted call sites, and add graph edges for the ones that resolve
/// to exactly one indexed unit.
///
/// Two stores, deliberately: `call_graph` keeps every call with its `file:line`
/// so a question like "where is this actually called?" can be answered exactly,
/// while `graph_edges` only gains an edge when the callee is unambiguous.
///
/// A method call carries no receiver type — `.run(..)` could belong to any
/// indexed type with a `run` method. Adding a traversal edge for those would
/// mean inventing ownership, and a dependency path built on invented edges is
/// worse than one that admits it does not know. So they are recorded and left
/// unresolved unless exactly one indexed type owns that method name.
/// `source_root` scopes resolution to the crate being indexed. Without it a
/// callee resolves to any indexed unit that happens to be the only one with
/// that name — across projects. Quartz's `Canvas` was recorded as calling
/// `ss_engine::events::Hand` and `synful::crystalline::types::PhysicsConfig`
/// purely because those were the sole indexed bearers of names Quartz uses from
/// its own dependencies. A dependency graph that invents links between unrelated
/// projects is worse than one with fewer edges.
pub fn ingest_calls(
    conn: &Connection,
    unit_id: &str,
    calls: &[crate::model::ApiGraphCall],
    scope: Option<&str>,
    source_root: &str,
) -> Result<(usize, usize)> {
    if calls.is_empty() {
        return Ok((0, 0));
    }

    // name -> unit ids, for resolving a path call's owner.
    let by_name = node_name_to_ids(conn, source_root)?;
    // method name -> unit ids owning a method of that name.
    let by_method = method_name_to_ids(conn, source_root)?;

    let mut recorded = 0usize;
    let mut edged = 0usize;
    let mut seen = HashSet::<(String, String, String)>::new();

    for call in calls {
        let (file, line) = match &call.span {
            Some(s) => (s.file.clone(), s.line as i64),
            None => (String::new(), 0),
        };

        // Resolve the callee to a unit id where that is honest.
        let resolved: Option<String> = match call.kind.as_str() {
            "path" | "Path" => {
                // `Canvas::new` -> the owning type, scoped like everything else.
                let owner = call.to.split("::").next().unwrap_or(&call.to);
                pick_unique(by_name.get(owner), scope)
            }
            _ => pick_unique(by_method.get(&call.to), scope),
        };

        // `call.from` is already qualified by its type (`Canvas::new`) or is a
        // free function name, and `unit_id` ends in that same type. Concatenating
        // the two produced `canvas::core::Canvas::Canvas::new`; take the unit's
        // module path instead so the caller reads `canvas::core::Canvas::new`.
        let module = unit_id.rsplit_once("::").map(|(m, _)| m).unwrap_or("");
        let caller = if module.is_empty() {
            call.from.clone()
        } else {
            format!("{module}::{}", call.from)
        };

        // The table is UNIQUE(caller, callee, edge_type): the same edge legitimately
        // arrives twice when a type's impls span files. Keep the first, which
        // carries a real file:line, rather than failing the whole index.
        let n = conn.execute(
            "INSERT OR IGNORE INTO call_graph
             (caller, callee, edge_type, file_path, line_number, weight, source)
             VALUES (?1, ?2, ?3, ?4, ?5, 1.0, 'extracted')",
            params![
                caller,
                resolved.clone().unwrap_or_else(|| call.to.clone()),
                call.kind,
                file,
                line
            ],
        )?;
        recorded += n;

        if let Some(to_id) = resolved {
            if to_id != unit_id {
                // source='calls', NOT 'inferred'. infer_edges opens with
                // `DELETE FROM graph_edges WHERE source='inferred'` and runs once
                // per indexed source, so tagging these 'inferred' meant every
                // source wiped the previous one's call edges — 349 extracted,
                // 14 surviving, and the count looked plausible enough to miss.
                let key = (unit_id.to_string(), to_id.clone(), "calls".to_string());
                if seen.insert(key) {
                    edged += conn.execute(
                        "INSERT OR IGNORE INTO graph_edges (from_id, to_id, relation, weight, source)
                         VALUES (?1, ?2, ?3, 1.0, 'calls')",
                        params![unit_id, to_id, RelationType::Calls.as_str()],
                    )?;
                }
            }
        }
    }

    Ok((recorded, edged))
}

/// Exactly one candidate, preferring same-scope, else None.
/// Ambiguity is reported as "unknown" rather than resolved arbitrarily.
fn pick_unique(candidates: Option<&Vec<String>>, scope: Option<&str>) -> Option<String> {
    let list = candidates?;
    if list.len() == 1 {
        return Some(list[0].clone());
    }
    // Several types share the name: accept only if the current scope has one.
    let prefix = scope.map(|s| format!("{s}::"))?;
    let mut in_scope = list.iter().filter(|id| id.starts_with(&prefix));
    let first = in_scope.next()?;
    in_scope.next().is_none().then(|| first.clone())
}

/// Restricted to units from the same source, so a callee never resolves into
/// another project that merely happens to own the only unit of that name.
fn node_name_to_ids(conn: &Connection, source_root: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT id, name FROM code_units WHERE source_root = ?1",
    )?;
    let rows = stmt.query_map(params![source_root], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        m.entry(name).or_default().push(id);
    }
    Ok(m)
}

/// Which units declare a method of each name, read from the compressed record.
fn method_name_to_ids(conn: &Connection, source_root: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT id, compressed FROM code_units WHERE source_root = ?1",
    )?;
    let rows = stmt.query_map(params![source_root], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, compressed) = row?;
        for line in compressed.lines() {
            let Some(rest) = line.strip_prefix("methods:") else { continue };
            for entry in rest.split('|') {
                // Entries are either a bare name or a full signature; the method
                // name is the leading identifier either way.
                let name: String = entry
                    .trim()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    m.entry(name).or_default().push(id.clone());
                }
            }
        }
    }
    for ids in m.values_mut() {
        ids.sort();
        ids.dedup();
    }
    Ok(m)
}

pub fn add_edge(conn: &Connection, from: &str, to: &str, relation: RelationType) -> Result<()> {
    // Allow all meaningful manual relations: pairs, conflicts, owns, uses, calls, implements
    if !matches!(relation,
        RelationType::Pairs | RelationType::Conflicts | RelationType::Owns |
        RelationType::Uses | RelationType::Calls | RelationType::Implements)
    {
        anyhow::bail!("manual edges allow: pairs, conflicts, owns, uses, calls, implements")
    }

    conn.execute(
        "INSERT INTO graph_edges (from_id, to_id, relation, weight, source)
         VALUES (?1, ?2, ?3, 1.0, 'manual')",
        params![from, to, relation.as_str()],
    )?;
    Ok(())
}

pub fn neighbors(conn: &Connection, node_id: &str) -> Result<Vec<(GraphEdge, GraphNode)>> {
    let mut stmt = conn.prepare(
        "SELECT e.from_id, e.to_id, e.relation, e.weight, e.source,
                n.id, n.kind, n.name, n.module_path
         FROM graph_edges e
         JOIN graph_nodes n ON n.id = e.to_id
         WHERE e.from_id = ?1"
    )?;

    let rows = stmt.query_map(params![node_id], |row| {
        let relation_s: String = row.get(2)?;
        let relation = RelationType::from_str(&relation_s).unwrap_or(RelationType::Uses);

        Ok((
            GraphEdge {
                from_id: row.get(0)?,
                to_id: row.get(1)?,
                relation,
                weight: row.get(3)?,
                source: row.get(4)?,
            },
            GraphNode {
                id: row.get(5)?,
                kind: row.get(6)?,
                name: row.get(7)?,
                module_path: row.get(8)?,
            },
        ))
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn used_by(conn: &Connection, node_id: &str) -> Result<Vec<GraphNode>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT n.id, n.kind, n.name, n.module_path
         FROM graph_edges e
         JOIN graph_nodes n ON n.id = e.from_id
         WHERE e.to_id = ?1 AND e.relation = 'uses'"
    )?;

    let rows = stmt.query_map(params![node_id], |row| {
        Ok(GraphNode {
            id: row.get(0)?,
            kind: row.get(1)?,
            name: row.get(2)?,
            module_path: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn subgraph(conn: &Connection, root_id: &str, depth: u8) -> Result<(Vec<GraphEdge>, Vec<GraphNode>)> {
    let mut nodes = HashMap::<String, GraphNode>::new();
    let mut edges = Vec::<GraphEdge>::new();

    if let Some(root) = get_node(conn, root_id)? {
        nodes.insert(root.id.clone(), root);
    } else {
        return Ok((edges, Vec::new()));
    }

    let mut seen = HashSet::new();
    let mut q = VecDeque::new();
    q.push_back((root_id.to_string(), 0u8));
    seen.insert(root_id.to_string());

    while let Some((node_id, d)) = q.pop_front() {
        if d >= depth {
            continue;
        }

        for (edge, node) in neighbors(conn, &node_id)? {
            edges.push(edge.clone());
            if !nodes.contains_key(&node.id) {
                nodes.insert(node.id.clone(), node.clone());
            }
            if seen.insert(node.id.clone()) {
                q.push_back((node.id.clone(), d + 1));
            }
        }
    }

    Ok((edges, nodes.into_values().collect()))
}

fn infer_impl_edges(conn: &Connection, unit: &CodeUnit, by_name: &HashMap<String, String>, seen: &mut HashSet<(String, String, String)>) -> Result<usize> {
    let mut count = 0usize;

    for line in unit.compressed.lines() {
        if let Some(raw) = line.strip_prefix("impl:") {
            for trait_name in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if let Some(to_id) = by_name.get(trait_name) {
                    count += insert_inferred_edge(conn, &unit.id, to_id, RelationType::Implements, seen)?;
                }
            }
        }
    }

    Ok(count)
}

fn infer_uses_edges(conn: &Connection, unit: &CodeUnit, by_name: &HashMap<String, String>, seen: &mut HashSet<(String, String, String)>) -> Result<usize> {
    let mut count = 0usize;

    for line in unit.compressed.lines() {
        if line.starts_with("fields:") || line.trim_start().starts_with(&format!("{}::", unit.name)) || line.starts_with("methods:") || line.starts_with("sig:") {
            for token in extract_type_tokens(line) {
                if token == unit.name {
                    continue;
                }
                if let Some(to_id) = by_name.get(&token) {
                    count += insert_inferred_edge(conn, &unit.id, to_id, RelationType::Uses, seen)?;
                }
            }
        }
    }

    Ok(count)
}

fn infer_derived_edges(conn: &Connection, unit: &CodeUnit, seen: &mut HashSet<(String, String, String)>) -> Result<usize> {
    let mut count = 0usize;

    let mut stmt = conn.prepare(
        "SELECT kind, name FROM code_members WHERE parent_id = ?1"
    )?;
    let rows = stmt.query_map(params![&unit.id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (kind, name) = row?;
        let child_id = format!("{}::{}:{}", unit.id, kind, name);

        conn.execute(
            "INSERT OR IGNORE INTO graph_nodes (id, kind, name, module_path)
             VALUES (?1, ?2, ?3, ?4)",
            params![child_id, kind, name, unit.module_path],
        )?;

        count += insert_inferred_edge(conn, &child_id, &unit.id, RelationType::DerivedFrom, seen)?;
    }

    Ok(count)
}

fn insert_inferred_edge(conn: &Connection, from_id: &str, to_id: &str, relation: RelationType, seen: &mut HashSet<(String, String, String)>) -> Result<usize> {
    let key = (from_id.to_string(), to_id.to_string(), relation.as_str().to_string());
    if !seen.insert(key) {
        return Ok(0);
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO graph_edges (from_id, to_id, relation, weight, source)
         VALUES (?1, ?2, ?3, 1.0, 'inferred')",
        params![from_id, to_id, relation.as_str()],
    )?;
    Ok(n)
}

fn node_name_to_id(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT id, name FROM graph_nodes")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut m = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        m.entry(name).or_insert(id);
    }
    Ok(m)
}

fn get_node(conn: &Connection, node_id: &str) -> Result<Option<GraphNode>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, name, module_path FROM graph_nodes WHERE id = ?1 LIMIT 1"
    )?;
    let mut rows = stmt.query_map(params![node_id], |row| {
        Ok(GraphNode {
            id: row.get(0)?,
            kind: row.get(1)?,
            name: row.get(2)?,
            module_path: row.get(3)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

fn extract_type_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();

    for c in line.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            if cur.chars().next().map(|x| x.is_ascii_uppercase()).unwrap_or(false) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }

    if !cur.is_empty() && cur.chars().next().map(|x| x.is_ascii_uppercase()).unwrap_or(false) {
        out.push(cur);
    }

    out
}

#[cfg(test)]
mod call_edge_tests {
    use super::*;
    use crate::memory::Store;
    use crate::model::{ApiGraphCall, ApiGraphSpan};

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join("cortex-call-edges");
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join(format!("{name}.db"));
        let _ = std::fs::remove_file(&db);
        Store::open(&db).unwrap()
    }

    fn node(s: &Store, id: &str, name: &str) {
        s.conn().execute(
            "INSERT OR REPLACE INTO graph_nodes (id, kind, name, module_path) VALUES (?1,'struct',?2,'m')",
            rusqlite::params![id, name],
        ).unwrap();
        // Resolution reads code_units filtered by source_root, so the unit must
        // exist there too — same as it would after a real index.
        s.conn().execute(
            "INSERT OR REPLACE INTO code_units (id,kind,name,module_path,summary,compressed,term_vector,indexed_at,source_root)
             VALUES (?1,'struct',?2,'m','','','[]','now','src')",
            rusqlite::params![id, name],
        ).unwrap();
    }

    fn call(from: &str, to: &str, kind: &str) -> ApiGraphCall {
        ApiGraphCall {
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            span: Some(ApiGraphSpan { file: "f.rs".into(), line: 7 }),
        }
    }

    /// Call edges must survive the next source's `infer_edges`, which opens with
    /// `DELETE FROM graph_edges WHERE source='inferred'`. Tagging them 'inferred'
    /// meant each indexed source silently wiped the previous one's — and the
    /// surviving count still looked plausible.
    #[test]
    fn call_edges_survive_a_later_infer_edges_pass() {
        let s = store("survive");
        node(&s, "m::Caller", "Caller");
        node(&s, "m::Callee", "Callee");

        let (_, edged) = ingest_calls(
            s.conn(), "m::Caller", &[call("Caller::go", "Callee::new", "path")], None, "src",
        ).unwrap();
        assert_eq!(edged, 1, "edge not created");

        // Simulate indexing another source.
        infer_edges(s.conn(), &[]).unwrap();

        let n: i64 = s.conn()
            .query_row("SELECT COUNT(*) FROM graph_edges WHERE relation='calls'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "call edge was deleted by a later infer_edges pass");
    }

    /// A method call names no receiver type. Resolving one arbitrarily would
    /// invent ownership and put a false edge into dependency paths.
    #[test]
    fn an_ambiguous_method_name_is_recorded_but_not_edged() {
        let s = store("ambiguous");
        node(&s, "m::Caller", "Caller");
        for id in ["m::A", "m::B"] {
            node(&s, id, id.rsplit("::").next().unwrap());
            s.conn().execute(
                "INSERT OR REPLACE INTO code_units (id,kind,name,module_path,summary,compressed,term_vector,indexed_at,source_root)
                 VALUES (?1,'struct',?2,'m','','methods: run','[]','now','src')",
                rusqlite::params![id, id.rsplit("::").next().unwrap()],
            ).unwrap();
        }

        let (recorded, edged) = ingest_calls(
            s.conn(), "m::Caller", &[call("Caller::go", "run", "method")], None, "src",
        ).unwrap();

        assert_eq!(recorded, 1, "the call should still be recorded with its file:line");
        assert_eq!(edged, 0, "an ambiguous callee must not become a graph edge");
    }

    /// The caller id must not double the type segment.
    #[test]
    fn caller_id_is_not_doubled() {
        let s = store("callerid");
        node(&s, "canvas::core::Canvas", "Canvas");
        node(&s, "m::Target", "Target");
        ingest_calls(
            s.conn(), "canvas::core::Canvas",
            &[call("Canvas::new", "Target::make", "path")], None, "src",
        ).unwrap();

        let caller: String = s.conn()
            .query_row("SELECT caller FROM call_graph LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(caller, "canvas::core::Canvas::new", "got {caller}");
    }
}

#[cfg(test)]
mod sync_nodes_tests {
    use super::*;
    use crate::memory::Store;

    /// `sync_nodes` must not destroy edges. SQLite's OR REPLACE is a
    /// DELETE + INSERT, and graph_edges cascades on graph_nodes deletion, so
    /// re-syncing wiped every durable edge. Edges that `infer_edges` rebuilds
    /// each run hid this; `calls` edges did not.
    #[test]
    fn re_syncing_nodes_preserves_existing_edges() {
        let dir = std::env::temp_dir().join("cortex-syncnodes");
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("sync.db");
        let _ = std::fs::remove_file(&db);
        let s = Store::open(&db).unwrap();

        for (id, name) in [("m::A", "A"), ("m::B", "B")] {
            s.conn().execute(
                "INSERT INTO code_units (id,kind,name,module_path,summary,compressed,term_vector,indexed_at)
                 VALUES (?1,'struct',?2,'m','','','[]','now')",
                rusqlite::params![id, name],
            ).unwrap();
        }
        sync_nodes(s.conn()).unwrap();

        s.conn().execute(
            "INSERT INTO graph_edges (from_id,to_id,relation,weight,source)
             VALUES ('m::A','m::B','calls',1.0,'calls')", [],
        ).unwrap();

        // Indexing another source re-syncs every node.
        sync_nodes(s.conn()).unwrap();

        let n: i64 = s.conn()
            .query_row("SELECT COUNT(*) FROM graph_edges WHERE relation='calls'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "sync_nodes cascaded an existing edge away");
    }

    /// It must still refresh changed metadata.
    #[test]
    fn re_syncing_updates_changed_node_fields() {
        let dir = std::env::temp_dir().join("cortex-syncnodes");
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("sync2.db");
        let _ = std::fs::remove_file(&db);
        let s = Store::open(&db).unwrap();

        s.conn().execute(
            "INSERT INTO code_units (id,kind,name,module_path,summary,compressed,term_vector,indexed_at)
             VALUES ('m::A','struct','A','m','','','[]','now')", [],
        ).unwrap();
        sync_nodes(s.conn()).unwrap();

        s.conn().execute("UPDATE code_units SET kind='enum' WHERE id='m::A'", []).unwrap();
        sync_nodes(s.conn()).unwrap();

        let kind: String = s.conn()
            .query_row("SELECT kind FROM graph_nodes WHERE id='m::A'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "enum", "node metadata went stale");
    }
}
