/// Phase 3: Graphify drift integration.
///
/// Compares the current graph.json with historical snapshots to detect
/// architectural drift — communities that are gaining or losing nodes,
/// indicating areas of active change or decay.
///
/// Integration points:
///   - closeout.rs writes snapshots to `.graphify-output/snapshots/`
///   - consolidator2.rs reads DriftReport to weight proposals
///   - CLI: `cortex graph-diff` for manual inspection
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Graph JSON structure (mirrors graphify-rs output) ────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GraphNode {
    pub id:        String,
    pub label:     Option<String>,
    #[serde(rename = "source_file")]
    pub source_file: Option<String>,
    #[serde(rename = "node_type")]
    pub node_type: Option<String>,
    pub community: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphLink {
    pub source: String,
    pub target: String,
    pub relation: Option<String>,
    pub weight:  Option<f64>,
}

/// Wrapper for the top-level graph.json structure.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphData {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub links: Vec<GraphLink>,
}

impl GraphData {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read graph: {}", path.display()))?;
        Ok(serde_json::from_str(&content)?)
    }

    /// Group node IDs by community.
    pub fn community_nodes(&self) -> HashMap<u32, HashSet<String>> {
        let mut map: HashMap<u32, HashSet<String>> = HashMap::new();
        for node in &self.nodes {
            if let Some(c) = node.community {
                map.entry(c).or_default().insert(node.id.clone());
            }
        }
        map
    }

    /// Quick node-count per community.
    pub fn community_sizes(&self) -> HashMap<u32, usize> {
        self.community_nodes().into_iter().map(|(k, v)| (k, v.len())).collect()
    }
}

// ── Drift metrics ────────────────────────────────────────────────────────────

/// Threshold above which a community is considered "high drift" (0.0–1.0).
/// Communities with drift >= this value get flagged in high_drift_communities.
pub const DRIFT_HIGH_THRESHOLD: f64 = 0.3;

/// Threshold above which drift scores get a priority boost in the pipeline.
pub const DRIFT_BOOST_HIGH: f64 = 0.5;
pub const DRIFT_BOOST_MEDIUM: f64 = 0.3;
pub const DRIFT_BOOST_LOW: f64 = 0.1;

#[derive(Debug, Clone, Serialize)]
pub struct CommunityDrift {
    pub community_id:       u32,
    pub nodes_added:        usize,
    pub nodes_removed:      usize,
    pub node_count_current: usize,
    pub node_count_previous: usize,
    /// 0.0 (stable) → 1.0 (high drift).  Computed as
    /// (added + removed) / max(prev_count, current_count).
    pub drift_score:        f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    pub current_graph_path:    String,
    pub previous_graph_path:   String,
    pub total_nodes_current:   usize,
    pub total_nodes_previous:  usize,
    pub total_links_current:   usize,
    pub total_links_previous:  usize,
    pub node_count_delta:      i64,
    pub link_count_delta:      i64,
    pub communities_affected:  usize,
    pub community_drifts:      Vec<CommunityDrift>,
    pub high_drift_communities: Vec<CommunityDrift>,
    pub analyzed_at:           String,
}

// ── Comparison logic ─────────────────────────────────────────────────────────

/// Compare two GraphData snapshots and produce a DriftReport.
pub fn compare_graphs(current: &GraphData, previous: &GraphData) -> DriftReport {
    let curr_comm = current.community_nodes();
    let prev_comm = previous.community_nodes();

    // Match communities across snapshots by MEMBERSHIP, not by id.
    //
    // Community detection does not produce stable ids: rebuild the graph and the
    // same set of nodes comes back as a different number. Comparing
    // curr_comm[id] against prev_comm[id] therefore reported every community as
    // ~100% changed after any rebuild — which is exactly what happened: 1303
    // communities at drift 2.00, each reading "+91/-91 nodes (now 91, was 91)".
    // Same size, same members, different id. The pipeline dutifully raised a
    // digest, the meta-analyser then flagged that nobody ever approved one, and
    // the whole loop spent a fortnight arguing with an artefact.
    //
    // Each current community is paired with the previous one it overlaps most,
    // so a renumbered-but-identical community scores 0.
    let mut pairing: HashMap<u32, u32> = HashMap::new();
    let mut taken: HashSet<u32> = HashSet::new();
    let mut candidates: Vec<(f64, u32, u32)> = Vec::new();
    for (c_id, c_nodes) in &curr_comm {
        for (p_id, p_nodes) in &prev_comm {
            let inter = c_nodes.intersection(p_nodes).count();
            if inter == 0 {
                continue;
            }
            let union = c_nodes.union(p_nodes).count().max(1);
            candidates.push((inter as f64 / union as f64, *c_id, *p_id));
        }
    }
    // Greedy best-overlap first, one previous community per current one.
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (score, c_id, p_id) in candidates {
        if score <= 0.0 || pairing.contains_key(&c_id) || taken.contains(&p_id) {
            continue;
        }
        pairing.insert(c_id, p_id);
        taken.insert(p_id);
    }

    let all_communities: HashSet<u32> = curr_comm
        .keys()
        .copied()
        .chain(prev_comm.keys().copied().filter(|p| !taken.contains(p)))
        .collect();

    let mut community_drifts: Vec<CommunityDrift> = Vec::new();
    let mut communities_affected = 0usize;

    for &c_id in &all_communities {
        let curr_nodes = curr_comm.get(&c_id).cloned().unwrap_or_default();
        // Its partner from the previous snapshot, whatever it was numbered.
        let prev_nodes = match pairing.get(&c_id) {
            Some(p_id) => prev_comm.get(p_id).cloned().unwrap_or_default(),
            // Unpaired: either a genuinely new community, or a previous one
            // that has no successor. Both are real signal.
            None => {
                if curr_comm.contains_key(&c_id) {
                    HashSet::new()
                } else {
                    prev_comm.get(&c_id).cloned().unwrap_or_default()
                }
            }
        };

        let added   = curr_nodes.difference(&prev_nodes).count();
        let removed = prev_nodes.difference(&curr_nodes).count();
        let curr_count = curr_nodes.len();
        let prev_count = prev_nodes.len();

        let drift_score = if prev_count == 0 && curr_count == 0 {
            0.0
        } else {
            let denom = prev_count.max(curr_count).max(1);
            (added + removed) as f64 / denom as f64
        };

        if added > 0 || removed > 0 {
            communities_affected += 1;
        }

        community_drifts.push(CommunityDrift {
            community_id: c_id,
            nodes_added: added,
            nodes_removed: removed,
            node_count_current: curr_count,
            node_count_previous: prev_count,
            drift_score,
        });
    }

    // Sort by drift_score descending.
    community_drifts.sort_by(|a, b| b.drift_score.partial_cmp(&a.drift_score).unwrap_or(std::cmp::Ordering::Equal));

    let high_drift: Vec<CommunityDrift> = community_drifts.iter()
        .filter(|c| c.drift_score >= DRIFT_HIGH_THRESHOLD)
        .cloned()
        .collect();

    DriftReport {
        current_graph_path:  String::new(),  // caller fills
        previous_graph_path: String::new(),
        total_nodes_current:  current.nodes.len(),
        total_nodes_previous: previous.nodes.len(),
        total_links_current:  current.links.len(),
        total_links_previous: previous.links.len(),
        node_count_delta:     current.nodes.len() as i64 - previous.nodes.len() as i64,
        link_count_delta:     current.links.len() as i64 - previous.links.len() as i64,
        communities_affected,
        community_drifts,
        high_drift_communities: high_drift,
        analyzed_at: chrono::Utc::now().to_rfc3339(),
    }
}

// ── Snapshot discovery ───────────────────────────────────────────────────────

/// Find the most recent snapshot before `current_path` (by name sort).
pub fn find_previous_snapshot(snapshots_dir: &Path, current_path: &Path) -> Option<PathBuf> {
    let current_name = current_path.file_name()?.to_str()?;
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(snapshots_dir).ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && p.file_name().and_then(|s| s.to_str()) != Some(current_name)
        })
        .collect();
    candidates.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    candidates.into_iter().next()
}

/// List all available graph snapshots sorted by name (desc).
pub fn list_snapshots(snapshots_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut snapshots: Vec<PathBuf> = std::fs::read_dir(snapshots_dir)
        .with_context(|| format!("cannot read snapshots dir: {}", snapshots_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    snapshots.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(snapshots)
}

// ── High-level runner ────────────────────────────────────────────────────────

/// Run the full graph diff analysis: load current graph, find previous snapshot,
/// compare, and return the report. Returns `None` if no previous snapshot exists.
pub fn run_graph_diff(
    snapshots_dir: &Path,
    current_graph_path: &Path,
) -> Result<Option<DriftReport>> {
    if !current_graph_path.exists() {
        return Ok(None);
    }

    let previous = find_previous_snapshot(snapshots_dir, current_graph_path);
    let previous = match previous {
        Some(p) => p,
        None => return Ok(None),
    };

    let current  = GraphData::from_file(current_graph_path)?;
    let previous_data = GraphData::from_file(&previous)?;

    let mut report = compare_graphs(&current, &previous_data);
    report.current_graph_path  = current_graph_path.display().to_string();
    report.previous_graph_path = previous.display().to_string();

    Ok(Some(report))
}

// ── Community weighting for the consolidation pipeline ───────────────────────

/// Weighting signal for the consolidation pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct CommunityWeight {
    pub community_id:  u32,
    pub drift_score:   f64,
    /// Proposal priority boost factor: 1.0 = normal, >1.0 = boost.
    pub priority_boost: f64,
}

/// Compute community weights from a drift report.
/// Communities with high drift get a priority boost so their proposals
/// are reviewed sooner.
pub fn compute_community_weights(report: &DriftReport) -> Vec<CommunityWeight> {
    report.community_drifts.iter().map(|c| {
        let boost = if c.drift_score >= DRIFT_BOOST_HIGH {
            3.0  // high drift → triple priority
        } else if c.drift_score >= DRIFT_BOOST_MEDIUM {
            2.0  // moderate drift → double priority
        } else if c.drift_score >= DRIFT_BOOST_LOW {
            1.5  // mild drift → slight boost
        } else {
            1.0  // stable → normal priority
        };
        CommunityWeight {
            community_id: c.community_id,
            drift_score: c.drift_score,
            priority_boost: boost,
        }
    }).collect()
}

/// Build a JSON payload suitable for storing in the prefs DB or attaching
/// to a consolidation pipeline run.
pub fn drift_report_to_json(report: &DriftReport) -> serde_json::Value {
    serde_json::json!({
        "current_graph": report.current_graph_path,
        "previous_graph": report.previous_graph_path,
        "total_nodes_current": report.total_nodes_current,
        "total_nodes_previous": report.total_nodes_previous,
        "node_count_delta": report.node_count_delta,
        "link_count_delta": report.link_count_delta,
        "communities_affected": report.communities_affected,
        "high_drift_count": report.high_drift_communities.len(),
        "analyzed_at": report.analyzed_at,
        "community_drifts": report.community_drifts,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(id: &str, community: u32) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            label: Some(id.to_string()),
            source_file: None,
            node_type: None,
            community: Some(community),
        }
    }

    #[test]
    fn identical_graphs_no_drift() {
        let nodes = vec![make_node("a", 1), make_node("b", 1), make_node("c", 2)];
        let current  = GraphData { nodes: nodes.clone(), links: vec![] };
        let previous = GraphData { nodes: nodes.clone(), links: vec![] };
        let report = compare_graphs(&current, &previous);
        assert_eq!(report.total_nodes_current, 3);
        assert_eq!(report.node_count_delta, 0);
        assert_eq!(report.communities_affected, 0);
        assert!(report.high_drift_communities.is_empty());
    }

    #[test]
    fn node_added_detected_as_drift() {
        let prev_nodes = vec![make_node("a", 1), make_node("b", 1)];
        let curr_nodes = vec![make_node("a", 1), make_node("b", 1), make_node("c", 1)];
        let current  = GraphData { nodes: curr_nodes, links: vec![] };
        let previous = GraphData { nodes: prev_nodes, links: vec![] };
        let report = compare_graphs(&current, &previous);
        assert!(report.node_count_delta > 0);
        let c1 = report.community_drifts.iter().find(|c| c.community_id == 1).unwrap();
        assert_eq!(c1.nodes_added, 1);
        assert!(c1.drift_score > 0.0);
    }

    #[test]
    fn node_removed_detected_as_drift() {
        let prev_nodes = vec![make_node("a", 1), make_node("b", 1), make_node("c", 1)];
        let curr_nodes = vec![make_node("a", 1), make_node("b", 1)];
        let current  = GraphData { nodes: curr_nodes, links: vec![] };
        let previous = GraphData { nodes: prev_nodes, links: vec![] };
        let report = compare_graphs(&current, &previous);
        assert!(report.node_count_delta < 0);
        let c1 = report.community_drifts.iter().find(|c| c.community_id == 1).unwrap();
        assert_eq!(c1.nodes_removed, 1);
    }

    #[test]
    fn high_drift_threshold() {
        // Community 1: 3 nodes added, 0 removed, prev=1 → drift = 3/4 = 0.75
        let prev_nodes = vec![make_node("a", 1)];
        let curr_nodes = vec![make_node("a", 1), make_node("b", 1), make_node("c", 1), make_node("d", 1)];
        let current  = GraphData { nodes: curr_nodes, links: vec![] };
        let previous = GraphData { nodes: prev_nodes, links: vec![] };
        let report = compare_graphs(&current, &previous);
        assert_eq!(report.high_drift_communities.len(), 1);
        assert_eq!(report.high_drift_communities[0].community_id, 1);
    }

    #[test]
    fn community_weights_reflect_drift() {
        // Create a report with mixed drift
        let drifts = vec![
            CommunityDrift {
                community_id: 1, nodes_added: 10, nodes_removed: 0,
                node_count_current: 20, node_count_previous: 10, drift_score: 0.5,
            },
            CommunityDrift {
                community_id: 2, nodes_added: 1, nodes_removed: 1,
                node_count_current: 50, node_count_previous: 50, drift_score: 0.04,
            },
        ];
        let high: Vec<CommunityDrift> = drifts.iter()
            .filter(|c| c.drift_score >= 0.3).cloned().collect();
        let report = DriftReport {
            current_graph_path:  "c.json".into(),
            previous_graph_path: "p.json".into(),
            total_nodes_current: 70, total_nodes_previous: 60,
            total_links_current: 100, total_links_previous: 90,
            node_count_delta: 10, link_count_delta: 10,
            communities_affected: 2,
            community_drifts: drifts,
            high_drift_communities: high,
            analyzed_at: String::new(),
        };
        let weights = compute_community_weights(&report);
        let w1 = weights.iter().find(|w| w.community_id == 1).unwrap();
        let w2 = weights.iter().find(|w| w.community_id == 2).unwrap();
        assert_eq!(w1.priority_boost, 3.0);  // high drift
        assert_eq!(w2.priority_boost, 1.0);  // stable
    }

    #[test]
    fn new_community_is_drift() {
        // Community 2 exists in current but not in previous.
        let prev_nodes = vec![make_node("a", 1)];
        let curr_nodes = vec![make_node("a", 1), make_node("b", 2)];
        let current  = GraphData { nodes: curr_nodes, links: vec![] };
        let previous = GraphData { nodes: prev_nodes, links: vec![] };
        let report = compare_graphs(&current, &previous);
        let c2 = report.community_drifts.iter().find(|c| c.community_id == 2).unwrap();
        assert_eq!(c2.nodes_added, 1);
        assert_eq!(c2.node_count_previous, 0);
        assert!(c2.drift_score > 0.0);
    }

    #[test]
    fn snapshot_listing_and_previous() {
        // Create temp snapshots and verify ordering.
        let dir = std::env::temp_dir().join("cortex-graph-diff-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let s1 = dir.join("graph_20260710_120000.json");
        let s2 = dir.join("graph_20260711_010000.json");
        let current = dir.join("graph_20260711_120000.json");

        std::fs::write(&s1, r#"{"nodes":[],"links":[]}"#).unwrap();
        std::fs::write(&s2, r#"{"nodes":[],"links":[]}"#).unwrap();
        std::fs::write(&current, r#"{"nodes":[],"links":[]}"#).unwrap();

        let snapshots = list_snapshots(&dir).unwrap();
        assert_eq!(snapshots.len(), 3);

        let prev = find_previous_snapshot(&dir, &current).unwrap();
        assert_eq!(prev.file_name().unwrap(), s2.file_name().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod drift_tests {
    use super::*;

    fn graph(communities: &[(u32, &[&str])]) -> GraphData {
        let mut nodes = Vec::new();
        for (cid, members) in communities {
            for m in *members {
                nodes.push(GraphNode {
                    id: m.to_string(),
                    label: None,
                    source_file: None,
                    node_type: None,
                    community: Some(*cid),
                });
            }
        }
        GraphData { nodes, links: Vec::new() }
    }

    #[test]
    fn renumbering_a_community_is_not_drift() {
        // The bug that produced 1303 false flags: identical membership, new ids.
        let before = graph(&[(1, &["a", "b", "c"]), (2, &["d", "e"])]);
        let after  = graph(&[(77, &["a", "b", "c"]), (99, &["d", "e"])]);
        let report = compare_graphs(&after, &before);
        assert_eq!(
            report.communities_affected, 0,
            "same members under different ids must score zero drift, got {:?}",
            report.community_drifts
        );
        assert!(report.high_drift_communities.is_empty());
    }

    #[test]
    fn a_real_membership_change_still_registers() {
        let before = graph(&[(1, &["a", "b", "c", "d"])]);
        // Half the members replaced — that is genuine drift.
        let after  = graph(&[(5, &["a", "b", "x", "y"])]);
        let report = compare_graphs(&after, &before);
        assert_eq!(report.communities_affected, 1);
        let d = &report.community_drifts[0];
        assert_eq!(d.nodes_added, 2);
        assert_eq!(d.nodes_removed, 2);
        assert!(d.drift_score > 0.9, "expected high drift, got {}", d.drift_score);
    }

    #[test]
    fn a_brand_new_community_is_reported() {
        let before = graph(&[(1, &["a", "b"])]);
        let after  = graph(&[(1, &["a", "b"]), (2, &["p", "q", "r"])]);
        let report = compare_graphs(&after, &before);
        assert_eq!(report.communities_affected, 1, "only the new one counts");
        let new = report.community_drifts.iter().find(|d| d.nodes_added == 3).unwrap();
        assert_eq!(new.node_count_previous, 0);
    }

    #[test]
    fn growth_within_a_community_is_proportional() {
        let before = graph(&[(1, &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"])]);
        let mut members: Vec<&str> = vec!["a","b","c","d","e","f","g","h","i","j"];
        members.push("k");
        let after = graph(&[(3, &members)]);
        let report = compare_graphs(&after, &before);
        let d = &report.community_drifts[0];
        assert_eq!(d.nodes_added, 1);
        assert_eq!(d.nodes_removed, 0);
        assert!(d.drift_score < DRIFT_HIGH_THRESHOLD,
            "one new node in ten must not trip the high-drift threshold, got {}", d.drift_score);
    }
}
