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
                    .map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_string).collect())
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
}

/// Cluster session snapshots by TF-IDF tool-sequence similarity.
/// Returns a list of clusters ordered by size descending.
pub fn cluster_snapshots(
    snapshots: &[SessionSnapshot],
    threshold: f32,
) -> Vec<SessionCluster> {
    if snapshots.is_empty() { return vec![]; }

    // Build corpus IDF: log(N / df) for each tool token.
    let n = snapshots.len() as f32;
    let mut df: HashMap<String, usize> = HashMap::new();
    for s in snapshots {
        let unique: std::collections::HashSet<_> = s.tool_sequence.iter().collect();
        for t in unique { *df.entry(t.clone()).or_insert(0) += 1; }
    }
    let idf: HashMap<String, f32> = df.iter()
        .map(|(t, &d)| (t.clone(), (n / d as f32).ln() + 1.0))
        .collect();

    // Build TF-IDF vector per snapshot.
    let vecs: Vec<HashMap<String, f32>> = snapshots.iter()
        .map(|s| build_tfidf(&s.tool_sequence, &idf))
        .collect();

    // Greedy clustering: each unassigned snapshot either joins an existing
    // cluster (if cosine ≥ threshold with centroid) or starts a new one.
    let mut cluster_centroids: Vec<usize>  = Vec::new();
    let mut assignments: Vec<Option<usize>> = vec![None; snapshots.len()];

    for i in 0..snapshots.len() {
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
            centroid_key: snapshots[centroid_idx].session_key.clone(),
            members: vec![],
            tool_sequence: snapshots[centroid_idx].tool_sequence.clone(),
            outcome_counts: HashMap::new(),
            total_markers: 0,
            threshold,
        }
    }).collect();

    for (i, snap) in snapshots.iter().enumerate() {
        if let Some(ci) = assignments[i] {
            clusters[ci].members.push(snap.session_key.clone());
            if let Some(o) = &snap.outcome_type {
                *clusters[ci].outcome_counts.entry(o.clone()).or_insert(0) += 1;
            }
            clusters[ci].total_markers += snap.marker_counts.values().sum::<usize>();
        }
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
