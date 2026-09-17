/// Phase 1: Session trajectory miner.
///
/// Reads `.cortex/mined-tasks/session_*.json` files written by closeout_session
/// and clusters them by task domain using TF-IDF cosine similarity.
///
/// Also reads the VS Code session store for richer user-message context
/// when the store is available.
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Session snapshot (read from .cortex/mined-tasks/*.json) ──────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionSnapshot {
    pub session_key:    String,
    pub outcome_type:   Option<String>,
    pub tool_sequence:  Vec<String>,
    pub marker_counts:  HashMap<String, usize>,
    pub domain_tags:    Vec<String>,
    pub created_at:     Option<String>,
}

/// Load all session snapshots from `.cortex/mined-tasks/`.
pub fn load_snapshots(mined_tasks_dir: &Path) -> Result<Vec<SessionSnapshot>> {
    let mut snapshots = Vec::new();
    let repo_root = mined_tasks_dir.parent().and_then(Path::parent);

    if !mined_tasks_dir.exists() {
        return Ok(snapshots);
    }

    for entry in std::fs::read_dir(mined_tasks_dir)
        .with_context(|| format!("reading {}", mined_tasks_dir.display()))?
    {
        let entry = entry?;
        let path  = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            let snap = SessionSnapshot {
                session_key:   v["session_key"].as_str().unwrap_or("").to_string(),
                outcome_type:  v["outcome_type"].as_str().map(str::to_string),
                tool_sequence: v["tool_sequence"].as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_string).collect())
                    .unwrap_or_default(),
                marker_counts: {
                    let mut m = HashMap::new();
                    if let Some(mc) = v["marker_counts"].as_object() {
                        for (k, v) in mc {
                            if let Some(n) = v.as_u64() {
                                m.insert(k.clone(), n as usize);
                            }
                        }
                    }
                    m
                },
                domain_tags: v["domain_tags"].as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_str())
                        .filter(|t| is_repo_dir(repo_root, t))
                        .map(str::to_string).collect())
                    .unwrap_or_default(),
                created_at: v["created_at"].as_str().map(str::to_string),
            };
            if !snap.session_key.is_empty() {
                snapshots.push(snap);
            }
        }
    }

    // Sort newest-first.
    snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(snapshots)
}

/// Whether a domain tag names a real top-level directory of the repository.
///
/// Snapshots written before the tagger resolved its repo root carry path
/// fragments instead of domains -- on a live store `C:`, `Users`, `G:` and
/// `private` outnumbered real domains roughly two to one -- and clustering on
/// them grouped unrelated sessions by the drive they ran on.
fn is_repo_dir(repo_root: Option<&Path>, tag: &str) -> bool {
    let Some(root) = repo_root else { return false };
    !tag.is_empty() && !tag.contains(['/', '\\', ':']) && root.join(tag).is_dir()
}

// ── Signal ────────────────────────────────────────────────────────────────────

/// Tools that say nothing about what a session was for. Hooks fire the first
/// three on every prompt, command and edit; the session protocol calls the next
/// group in every session; and every coding session reads, edits and runs things.
///
/// Clustering on them grouped sessions by era rather than by work -- sessions
/// from before the hooks existed formed clusters of their own -- and naming from
/// them resolved nearly every cluster to `workflow-general`.
pub const NON_SIGNAL_TOOLS: &[&str] = &[
    "note_challenge", "compact_output", "edit_guard",
    "get_delta", "get_preferences", "get_anti_patterns", "get_context", "list_patterns",
    "begin_protocol_session", "get_session_health", "flush_knowledge_markers", "closeout_session",
    "Bash", "Read", "Write", "Edit", "MultiEdit", "NotebookEdit", "Glob", "Grep", "LS", "TodoWrite",
];

/// A tool sequence with the non-signal tools removed, order kept.
pub fn signal_tools(sequence: &[String]) -> Vec<String> {
    sequence.iter().filter(|t| !NON_SIGNAL_TOOLS.contains(&t.as_str())).cloned().collect()
}

/// What a session is clustered on: its signal tools and its domain tags.
fn cluster_tokens(s: &SessionSnapshot) -> Vec<String> {
    let mut tokens = signal_tools(&s.tool_sequence);
    tokens.extend(s.domain_tags.iter().map(|d| format!("domain:{d}")));
    tokens
}

/// Items present in at least half of the lists, most common first, ties by name.
fn shared(lists: impl Iterator<Item = Vec<String>>) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut n = 0usize;
    for list in lists {
        n += 1;
        let unique: std::collections::HashSet<String> = list.into_iter().collect();
        for item in unique {
            *counts.entry(item).or_insert(0) += 1;
        }
    }
    let mut common: Vec<(String, usize)> = counts.into_iter().filter(|(_, c)| c * 2 >= n).collect();
    common.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    common.into_iter().map(|(t, _)| t).collect()
}

// ── TF-IDF term vector ────────────────────────────────────────────────────────

/// Build a TF-IDF term vector from a list of tokens.
/// Returns a map of token → weight.
fn build_tfidf(tokens: &[String], idf: &HashMap<String, f32>) -> HashMap<String, f32> {
    if tokens.is_empty() { return HashMap::new(); }
    let total = tokens.len() as f32;
    let mut tf: HashMap<String, usize> = HashMap::new();
    for t in tokens {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf.into_iter()
        .map(|(t, count)| {
            let tf_score = count as f32 / total;
            let idf_score = idf.get(&t).copied().unwrap_or(1.0);
            (t, tf_score * idf_score)
        })
        .collect()
}

/// Cosine similarity between two TF-IDF vectors.
fn cosine(a: &HashMap<String, f32>, b: &HashMap<String, f32>) -> f32 {
    let dot: f32 = a.iter()
        .filter_map(|(k, va)| b.get(k).map(|vb| va * vb))
        .sum();
    let mag_a: f32 = a.values().map(|v| v * v).sum::<f32>().sqrt();
    let mag_b: f32 = b.values().map(|v| v * v).sum::<f32>().sqrt();
    if mag_a < 1e-9 || mag_b < 1e-9 { 0.0 } else { dot / (mag_a * mag_b) }
}

// ── Clustering ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCluster {
    /// Index of the centroid snapshot in the input list.
    pub centroid_key: String,
    /// Session keys in this cluster.
    pub members:      Vec<String>,
    /// Dominant tool sequence (most common across members).
    pub tool_sequence: Vec<String>,
    /// Outcome distribution.
    pub outcome_counts: HashMap<String, usize>,
    /// Total markers across members.
    pub total_markers: usize,
    /// Similarity threshold used.
    pub threshold: f32,
    /// Signal tools used by at least half the members, most common first.
    #[serde(default)]
    pub signal_tools: Vec<String>,
    /// Domain tags shared by at least half the members, most common first.
    #[serde(default)]
    pub domain_tags: Vec<String>,
}

/// Cluster session snapshots by TF-IDF similarity of their signal tools and
/// domain tags. Returns a list of clusters ordered by size descending.
pub fn cluster_snapshots(
    snapshots: &[SessionSnapshot],
    threshold: f32,
) -> Vec<SessionCluster> {
    // A session with nothing but non-signal tools and no domain says nothing
    // about what kind of work it was; clustering it only yields a cluster of
    // "sessions that happened".
    let usable: Vec<(&SessionSnapshot, Vec<String>)> = snapshots.iter()
        .map(|s| (s, cluster_tokens(s)))
        .filter(|(_, tokens)| !tokens.is_empty())
        .collect();
    if usable.is_empty() { return vec![]; }

    // Build corpus IDF: log(N / df) for each token.
    let n = usable.len() as f32;
    let mut df: HashMap<String, usize> = HashMap::new();
    for (_, tokens) in &usable {
        let unique: std::collections::HashSet<_> = tokens.iter().collect();
        for t in unique { *df.entry(t.clone()).or_insert(0) += 1; }
    }
    let idf: HashMap<String, f32> = df.iter()
        .map(|(t, &d)| (t.clone(), (n / d as f32).ln() + 1.0))
        .collect();

    // Build TF-IDF vector per snapshot.
    let vecs: Vec<HashMap<String, f32>> = usable.iter()
        .map(|(_, tokens)| build_tfidf(tokens, &idf))
        .collect();

    // Greedy clustering: each unassigned snapshot either joins an existing
    // cluster (if cosine ≥ threshold with centroid) or starts a new one.
    let mut cluster_centroids: Vec<usize>  = Vec::new();
    let mut assignments: Vec<Option<usize>> = vec![None; usable.len()];

    for i in 0..usable.len() {
        let mut best_cluster = None;
        let mut best_sim = 0.0f32;

        for (ci, &centroid_idx) in cluster_centroids.iter().enumerate() {
            let sim = cosine(&vecs[i], &vecs[centroid_idx]);
            if sim >= threshold && sim > best_sim {
                best_sim = sim;
                best_cluster = Some(ci);
            }
        }

        if let Some(ci) = best_cluster {
            assignments[i] = Some(ci);
        } else {
            assignments[i] = Some(cluster_centroids.len());
            cluster_centroids.push(i);
        }
    }

    // Build cluster structs.
    let num_clusters = cluster_centroids.len();
    let mut clusters: Vec<SessionCluster> = (0..num_clusters).map(|ci| {
        let centroid_idx = cluster_centroids[ci];
        SessionCluster {
            centroid_key: usable[centroid_idx].0.session_key.clone(),
            members: vec![],
            tool_sequence: usable[centroid_idx].0.tool_sequence.clone(),
            outcome_counts: HashMap::new(),
            total_markers: 0,
            threshold,
            signal_tools: vec![],
            domain_tags: vec![],
        }
    }).collect();

    for (i, (snap, _)) in usable.iter().enumerate() {
        if let Some(ci) = assignments[i] {
            clusters[ci].members.push(snap.session_key.clone());
            if let Some(o) = &snap.outcome_type {
                *clusters[ci].outcome_counts.entry(o.clone()).or_insert(0) += 1;
            }
            clusters[ci].total_markers += snap.marker_counts.values().sum::<usize>();
        }
    }

    // What the members have in common -- the basis a candidate is named on.
    for (ci, cluster) in clusters.iter_mut().enumerate() {
        let members: Vec<&SessionSnapshot> = usable.iter().enumerate()
            .filter(|(i, _)| assignments[*i] == Some(ci))
            .map(|(_, (s, _))| *s)
            .collect();
        cluster.signal_tools = shared(members.iter().map(|s| signal_tools(&s.tool_sequence)));
        cluster.domain_tags = shared(members.iter().map(|s| s.domain_tags.clone()));
    }

    clusters.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    clusters
}

// ── Report generation ─────────────────────────────────────────────────────────

/// Generate a human-readable cluster report.
pub fn format_cluster_report(clusters: &[SessionCluster]) -> String {
    let total: usize = clusters.iter().map(|c| c.members.len()).sum();
    let mut out = format!(
        "Session Cluster Report\n{} sessions → {} clusters\n\n",
        total, clusters.len()
    );

    for (i, c) in clusters.iter().enumerate() {
        let pass = c.outcome_counts.get("build_pass").copied().unwrap_or(0);
        let fail = c.outcome_counts.values().sum::<usize>().saturating_sub(pass);
        out.push_str(&format!(
            "Cluster {} ({} sessions, {} pass / {} fail, {} markers)\n",
            i + 1, c.members.len(), pass, fail, c.total_markers
        ));
        out.push_str(&format!("  centroid: {}\n", c.centroid_key));
        if !c.tool_sequence.is_empty() {
            let seq: Vec<_> = c.tool_sequence.iter().take(5).collect();
            out.push_str(&format!("  tool sequence: {}\n", seq.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" → ")));
        }
        out.push('\n');
    }

    out
}

// ── JSON serialisation ────────────────────────────────────────────────────────

pub fn clusters_to_json(clusters: &[SessionCluster]) -> String {
    serde_json::to_string_pretty(clusters).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(key: &str, tools: &[&str], domains: &[&str]) -> SessionSnapshot {
        SessionSnapshot {
            session_key: key.into(),
            outcome_type: Some("build_pass".into()),
            tool_sequence: tools.iter().map(|t| t.to_string()).collect(),
            marker_counts: HashMap::new(),
            domain_tags: domains.iter().map(|d| d.to_string()).collect(),
            created_at: None,
        }
    }

    /// Sessions from before the hooks existed formed clusters of their own,
    /// apart from identical work done after.
    #[test]
    fn hooks_and_protocol_tools_do_not_split_the_same_work() {
        let snaps = vec![
            snap("new1", &["note_challenge", "compact_output", "get_anti_patterns", "get_item", "recall", "Edit"], &["engine"]),
            snap("old1", &["get_delta", "get_preferences", "get_item", "recall", "Bash"], &["engine"]),
            snap("new2", &["note_challenge", "edit_guard", "semantic_search", "query_graph"], &["docs"]),
        ];
        let clusters = cluster_snapshots(&snaps, 0.55);

        let with_new1 = clusters.iter().find(|c| c.members.contains(&"new1".to_string())).unwrap();
        assert!(with_new1.members.contains(&"old1".to_string()), "{clusters:?}");
        assert!(!with_new1.members.contains(&"new2".to_string()), "{clusters:?}");
        assert_eq!(with_new1.signal_tools, vec!["get_item", "recall"]);
        assert_eq!(with_new1.domain_tags, vec!["engine"]);
    }

    #[test]
    fn a_session_with_nothing_but_noise_is_not_clustered() {
        let snaps = vec![snap("noise", &["note_challenge", "closeout_session", "Bash", "Edit"], &[])];
        assert!(cluster_snapshots(&snaps, 0.55).is_empty());
    }

    /// Path fragments written by the old tagger must not become domains.
    #[test]
    fn only_real_top_level_directories_survive_as_domain_tags() {
        let root = crate::test_support::TempDir::new("miner_tags").unwrap();
        std::fs::create_dir_all(root.join("engine")).unwrap();
        let mined = root.join(".cortex").join("mined-tasks");
        std::fs::create_dir_all(&mined).unwrap();
        std::fs::write(
            mined.join("session_a.json"),
            r#"{"session_key":"a","tool_sequence":["get_item"],"domain_tags":["engine","Users","C:","private"]}"#,
        ).unwrap();

        let snaps = load_snapshots(&mined).unwrap();
        assert_eq!(snaps[0].domain_tags, vec!["engine"]);
    }
}
