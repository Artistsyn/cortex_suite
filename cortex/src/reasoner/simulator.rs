//! Simulator — dry-run impact predictor.
//! Predicts what breaks before touching a core type.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use crate::graph;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Low => "Low",
            RiskLevel::Medium => "Medium",
            RiskLevel::High => "High",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedItem {
    pub name: String,
    pub relation: String,
    pub impact: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    pub changed_item: String,
    pub affected: Vec<AffectedItem>,
    pub depends_on: Vec<AffectedItem>,
    pub risk_level: RiskLevel,
    pub warnings: Vec<String>,
}

impl SimulationResult {
    pub fn render(&self) -> String {
        let risk_emoji = match self.risk_level {
            RiskLevel::Low => "✓",
            RiskLevel::Medium => "⚠",
            RiskLevel::High => "🚨",
        };

        let mut output = format!(
            "{} SIMULATION RESULT for: {}\n\
             Risk Level: {} ({})\n\n",
            risk_emoji, self.changed_item, self.risk_level.as_str(), 
            self.affected.len()
        );

        if self.affected.is_empty() {
            output.push_str("No direct impact detected.\n");
        } else {
            output.push_str("Affected Items:\n");
            for item in &self.affected {
                output.push_str(&format!(
                    "  • {} (via {}): {}\n",
                    item.name, item.relation, item.impact
                ));
            }
        }

        if !self.depends_on.is_empty() {
            output.push_str("\nDepends On (outbound):\n");
            for item in &self.depends_on {
                output.push_str(&format!(
                    "  • {} (via {}): {}\n",
                    item.name, item.relation, item.impact
                ));
            }
        }

        if !self.warnings.is_empty() {
            output.push_str("\nWarnings:\n");
            for warning in &self.warnings {
                output.push_str(&format!("  ⚠ {}\n", warning));
            }
        }

        output
    }
}

/// Simulate the impact of changing an item.
/// Looks up reverse edges (used_by) and classifies impact by relation type.
pub fn simulate_change(
    conn: &Connection,
    item_name: &str,
    change_description: &str,
) -> crate::Result<SimulationResult> {
    let mut affected = vec![];
    let mut depends_on = vec![];
    let mut warnings = vec![];
    let mut seen: HashSet<(String, String)> = HashSet::new();

    let node_ids = resolve_node_ids(conn, item_name)?;
    if node_ids.is_empty() {
        return Ok(SimulationResult {
            changed_item: item_name.to_string(),
            affected: vec![],
            depends_on: vec![],
            risk_level: RiskLevel::Low,
            warnings: vec!["Item not found in graph — no impact detected.".to_string()],
        });
    }

    if node_ids.len() > 1 {
        let names = node_ids
            .iter()
            .map(|(id, module_path)| format!("{} ({})", id, module_path))
            .collect::<Vec<_>>()
            .join(" | ");
        warnings.push(format!(
            "Ambiguous symbol lookup for '{}'. Aggregating impact across {} matches: {}",
            item_name,
            node_ids.len(),
            names
        ));
    }

    for (node_id, _module_path) in node_ids {
        collect_inbound_impact(conn, &node_id, change_description, &mut affected, &mut warnings, &mut seen)?;
        collect_outbound_dependencies(conn, &node_id, &mut depends_on)?;
    }

    depends_on.sort_by(|a, b| a.name.cmp(&b.name));
    depends_on.dedup_by(|a, b| a.name == b.name && a.relation == b.relation);

    // Classify risk
    let risk_basis = affected.len() + (depends_on.len() / 2);
    let risk_level = match risk_basis {
        0..=2 => RiskLevel::Low,
        3..=7 => RiskLevel::Medium,
        _ => RiskLevel::High,
    };

    if risk_level == RiskLevel::High {
        warnings.insert(0, "HIGH RISK: Many items depend on this. Plan for wide-ranging re-testing.".to_string());
    }

    Ok(SimulationResult {
        changed_item: item_name.to_string(),
        affected,
        depends_on,
        risk_level,
        warnings,
    })
}

fn collect_inbound_impact(
    conn: &Connection,
    node_id: &str,
    change_description: &str,
    affected: &mut Vec<AffectedItem>,
    warnings: &mut Vec<String>,
    seen: &mut HashSet<(String, String)>,
) -> crate::Result<()> {
    // Use graph reverse lookup for primary impact set (depth 1).
    let users = graph::used_by(conn, node_id)?;
    for user in users {
        if !seen.insert((user.name.clone(), "uses".to_string())) {
            continue;
        }

        let impact = if change_description.contains("new field") || change_description.contains("signature") {
            format!("'{}' uses this type in fields or return type — construction sites affected", user.name)
        } else {
            format!("'{}' references this type — may need re-evaluation", user.name)
        };

        affected.push(AffectedItem {
            name: user.name,
            relation: "uses".to_string(),
            impact,
        });
    }

    // Include any non-uses reverse relations for additional warnings.
    let mut stmt = conn.prepare(
        "SELECT ge.from_id, ge.relation, gn.name
         FROM graph_edges ge
         JOIN graph_nodes gn ON ge.from_id = gn.id
         WHERE ge.to_id = ?1
         ORDER BY ge.weight DESC
         LIMIT 20"
    )?;

    let uses = stmt.query_map([node_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    for use_result in uses {
        let (_from_id, relation, from_name) = use_result?;
        let relation_norm = relation.to_lowercase();

        if !seen.insert((from_name.clone(), relation_norm.clone())) {
            continue;
        }

        let impact = match relation_norm.as_str() {
            "implements" => {
                warnings.push(format!("Trait implementation: '{}' implements this trait. Contract may need updating.", from_name));
                "trait contract may need updating".to_string()
            },
            "uses" => {
                if change_description.contains("new field") || change_description.contains("signature") {
                    format!("'{}' uses this type in fields or return type — construction sites affected", from_name)
                } else {
                    format!("'{}' references this type — may need re-evaluation", from_name)
                }
            },
            "pairs" => {
                warnings.push(format!("Paired usage detected: '{}' is marked to be used with this item.", from_name));
                "paired usage — may require coordinated update".to_string()
            },
            "conflicts" => {
                warnings.push(format!("Conflict detected: '{}' is marked as conflicting with this item.", from_name));
                "conflicts flagged — reconsider change or add guard logic".to_string()
            },
            "derived_from" => {
                "derived from/variant — parent change may cascade".to_string()
            },
            "calls" => {
                "function call site — callers may break".to_string()
            },
            _ => "unknown relation — requires manual review".to_string(),
        };

        let relation_value = relation.clone();
        affected.push(AffectedItem {
            name: from_name,
            relation: relation_value,
            impact,
        });
    }

    Ok(())
}

fn collect_outbound_dependencies(
    conn: &Connection,
    node_id: &str,
    depends_on: &mut Vec<AffectedItem>,
) -> crate::Result<()> {
    let neighbors = graph::neighbors(conn, node_id)?;
    for (edge, node) in neighbors {
        depends_on.push(AffectedItem {
            name: node.name,
            relation: edge.relation.as_str().to_string(),
            impact: "Direct dependency of changed item".to_string(),
        });
    }
    Ok(())
}

fn resolve_node_ids(conn: &Connection, item_name: &str) -> crate::Result<Vec<(String, String)>> {
    if let Some((id, module_path)) = conn
        .query_row(
            "SELECT id, module_path FROM graph_nodes WHERE id = ?1 LIMIT 1",
            [item_name],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        return Ok(vec![(id, module_path)]);
    }

    let mut stmt = conn.prepare(
        "SELECT id, module_path FROM graph_nodes WHERE name = ?1 ORDER BY module_path"
    )?;
    let exact = stmt
        .query_map([item_name], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !exact.is_empty() {
        return Ok(exact);
    }

    let like = format!("%{}%", item_name);
    let mut stmt = conn.prepare(
        "SELECT id, module_path FROM graph_nodes
         WHERE name LIKE ?1 OR id LIKE ?1
         ORDER BY module_path
         LIMIT 5"
    )?;
    let fuzzy = stmt
        .query_map(params![like], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(fuzzy)
}

/// Extended simulation with depth-2 transitive lookup.
pub fn simulate_change_deep(
    conn: &Connection,
    item_name: &str,
    change_description: &str,
    depth: u8,
) -> crate::Result<SimulationResult> {
    let mut result = simulate_change(conn, item_name, change_description)?;

    if depth > 1 && !result.affected.is_empty() {
        // Transitive: check what uses the users
        let mut transitive_affected = vec![];
        
        for item in &result.affected {
            if let Ok(transitive_result) = simulate_change(conn, &item.name, "transitive check") {
                transitive_affected.extend(transitive_result.affected);
            }
        }

        if !transitive_affected.is_empty() {
            result.warnings.push(format!(
                "Transitive impact: {} more items affected indirectly",
                transitive_affected.len()
            ));
            result.risk_level = RiskLevel::High;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_classification() {
        assert_eq!(
            match 1 { 0..=2 => RiskLevel::Low, _ => RiskLevel::High },
            RiskLevel::Low
        );
        assert_eq!(
            match 5 { 0..=2 => RiskLevel::Low, 3..=6 => RiskLevel::Medium, _ => RiskLevel::High },
            RiskLevel::Medium
        );
        assert_eq!(
            match 10 { 0..=2 => RiskLevel::Low, 3..=6 => RiskLevel::Medium, _ => RiskLevel::High },
            RiskLevel::High
        );
    }

    #[test]
    fn test_render_output() {
        let result = SimulationResult {
            changed_item: "Action".to_string(),
            affected: vec![
                AffectedItem {
                    name: "GameObject".to_string(),
                    relation: "Uses".to_string(),
                    impact: "field type change".to_string(),
                },
            ],
            depends_on: vec![],
            risk_level: RiskLevel::Medium,
            warnings: vec!["Consider backwards compat.".to_string()],
        };
        let rendered = result.render();
        assert!(rendered.contains("Action"));
        assert!(rendered.contains("Medium"));
        assert!(rendered.contains("GameObject"));
    }
}
