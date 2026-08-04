/// Session context planner — pre-compiles a minimal, high-signal context packet
/// for the current task. Keeps token usage as low as possible by serving only
/// what's relevant rather than the full index.
use std::path::Path;

use crate::git;
use crate::memory::Store;
use crate::model::ContextPacket;
use crate::search::semantic_search;
use anyhow::Result;

/// Build a context packet relevant to the given hint (open file paths or task description).
/// Caps output at `token_budget` estimated tokens (1 token ≈ 4 chars).
pub fn build_context_packet(
    store: &Store,
    hint: &str,
    token_budget: usize,
    repo_root: Option<&Path>,
    delta_options: Option<&git::DeltaOptions>,
) -> Result<ContextPacket> {
    let all_units = store.all_units()?;
    let all_patterns = store.all_patterns()?;
    let all_anti_patterns = store.all_anti_patterns()?;
    let all_annotations = store.all_annotations()?;

    let mut budget_remaining = token_budget;

    // Semantic search for relevant units
    let search_results = semantic_search(hint, &all_units, 8);
    let mut relevant_units = Vec::new();
    for result in search_results {
        let cost = estimate_tokens(&result.unit.compressed);
        if cost <= budget_remaining {
            budget_remaining = budget_remaining.saturating_sub(cost);
            relevant_units.push(result.unit.clone());
        }
    }

    // Patterns that reference any of the relevant unit names
    let relevant_names: Vec<&str> = relevant_units.iter().map(|u| u.name.as_str()).collect();
    let patterns: Vec<_> = all_patterns
        .into_iter()
        .filter(|p| {
            p.uses.iter().any(|u| relevant_names.contains(&u.as_str()))
                || relevant_names.iter().any(|n| p.intent.to_lowercase().contains(n))
        })
        .take(4)
        .collect();

    for p in &patterns {
        budget_remaining = budget_remaining.saturating_sub(estimate_tokens(&p.body));
    }

    // Anti-patterns — always include all, they're short and critical
    let anti_patterns: Vec<_> = all_anti_patterns.into_iter().take(6).collect();
    for ap in &anti_patterns {
        budget_remaining = budget_remaining.saturating_sub(estimate_tokens(&ap.wrong) + estimate_tokens(&ap.correct));
    }

    // Annotations matching the hint
    let hint_lower = hint.to_lowercase();
    let annotations: Vec<_> = all_annotations
        .into_iter()
        .filter(|a| {
            a.topic.to_lowercase().contains(&hint_lower)
                || a.body.to_lowercase().contains(&hint_lower)
                || a.tags.iter().any(|t| hint_lower.contains(t))
        })
        .take(3)
        .collect();

    // ADRs relevant to this context (accepted, matched by title words or concept tags).
    let hint_words: Vec<&str> = hint_lower.split_whitespace().collect();
    let adrs: Vec<_> = store.all_adrs().unwrap_or_default()
        .into_iter()
        .filter(|a| {
            a.status == "accepted" && (
                a.title.to_lowercase().split_whitespace().any(|w| hint_lower.contains(w))
                || a.concept_tags.iter().any(|t| {
                    let tl = t.to_lowercase();
                    hint_lower.contains(&tl) || hint_words.iter().any(|w| tl.contains(*w))
                })
            )
        })
        .take(3)
        .collect();

    let used_tokens = token_budget.saturating_sub(budget_remaining);

    let deltas = if let Some(root) = repo_root {
        let opts = delta_options.cloned().unwrap_or_else(|| git::DeltaOptions {
            include: None,
            exclude: None,
            max_files: 8,
            max_patch_lines: 40,
        });

        git::head_deltas_with_options(root, &opts)?
            .into_iter()
            .map(|d| git::compress_delta(&d))
            .collect()
    } else {
        vec![]
    };

    Ok(ContextPacket {
        relevant_units,
        patterns,
        anti_patterns,
        annotations,
        adrs,
        deltas,
        estimated_tokens: used_tokens,
    })
}

/// Render a context packet as a dense, injected preamble string.
/// This is what actually gets sent to Copilot — minimal, structured, no filler.
pub fn render_packet(packet: &ContextPacket) -> String {
    let mut s = String::new();

    // API units
    if !packet.relevant_units.is_empty() {
        s.push_str("=== RELEVANT API ===\n");
        for unit in &packet.relevant_units {
            s.push_str(&unit.compressed);
            s.push('\n');
        }
    }

    // ADRs — architectural constraints injected before patterns
    if !packet.adrs.is_empty() {
        s.push_str("=== ARCHITECTURE DECISIONS ===\n");
        for adr in &packet.adrs {
            s.push_str(&format!(
                "## ADR-{:03}: {} [{}]\nContext: {}\nDecision: {}\n\n",
                adr.adr_number, adr.title, adr.status, adr.context, adr.decision
            ));
        }
    }

    // Patterns
    if !packet.patterns.is_empty() {
        s.push_str("=== KNOWN PATTERNS ===\n");
        for p in &packet.patterns {
            s.push_str(&format!("# {} — {}\n", p.name, p.intent));
            s.push_str(&p.body);
            s.push('\n');
        }
    }

    // Anti-patterns — injected as hard constraints
    if !packet.anti_patterns.is_empty() {
        s.push_str("=== DO NOT DO ===\n");
        for ap in &packet.anti_patterns {
            s.push_str(&format!("✗ {}\n  wrong:   {}\n  correct: {}\n",
                ap.description, ap.wrong, ap.correct));
        }
    }

    // Annotations
    if !packet.annotations.is_empty() {
        s.push_str("=== NOTES ===\n");
        for a in &packet.annotations {
            s.push_str(&format!("[{}] {}\n", a.topic, a.body));
        }
    }

    // Deltas
    if !packet.deltas.is_empty() {
        s.push_str("=== RECENT CHANGES ===\n");
        for d in &packet.deltas {
            s.push_str(&format!("{} {} — {}\n", d.change, d.path, d.summary));
        }
    }

    if !s.is_empty() {
        s.push_str(&format!("\n[~{} tokens]\n", packet.estimated_tokens));
    }

    s
}

fn estimate_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}
