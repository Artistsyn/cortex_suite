use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::model::{CodeUnit, GraphEdge, GraphNode, RelationType};

pub fn sync_nodes(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "INSERT OR REPLACE INTO graph_nodes (id, kind, name, module_path)
         SELECT id, kind, name, module_path FROM code_units",
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
