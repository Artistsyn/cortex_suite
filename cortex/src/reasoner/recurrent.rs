//! Recurrent loop — Mythos-inspired iterative refinement.
//! Propose → Critique → Refine → Assess → Halt or Continue

use super::scratchpad::Scratchpad;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Words that negate an anti-pattern match when they precede the matched text.
const NEGATION_WORDS: &[&str] = &[
    "not ", "n't ", "avoid ", "never ", "instead of ",
    "shouldn't ", "should not ", "don't ", "do not ",
    "without ", "except ", "skip ", "omit ",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurrentContext {
    pub loop_index: u8,
    pub confidence: f32,
    pub critiques: Vec<String>,
    pub should_continue: bool,
    pub halt_reason: Option<String>,
    pub next_prompt: String,
}

/// Run one iteration of the recurrent loop:
/// 1. Critique the current hypothesis against anti-patterns + graph conflicts
/// 2. Score confidence
/// 3. Check halting conditions
/// 4. Generate next prompt or halt signal
pub fn run_recurrent_loop(
    scratchpad: &mut Scratchpad,
    conn: &Connection,
    loop_index: u8,
    max_loops: u8,
) -> crate::Result<RecurrentContext> {
    let hypothesis = scratchpad.hypotheses.last()
        .ok_or_else(|| anyhow::anyhow!("No hypothesis to critique"))?
        .content.clone();

    // Step 1: Critique hypothesis
    let critiques = critique_hypothesis(&hypothesis, conn)?;
    
    for c in &critiques {
        scratchpad.add_critique(c)?;
    }

    // Step 2: Score confidence
    let confidence = score_confidence(&critiques, conn, &hypothesis)?;
    scratchpad.set_confidence(confidence);

    // Step 3: Check halting conditions
    let (should_halt, halt_reason) = should_halt(scratchpad, max_loops);

    // Step 4: Generate response
    let should_continue = !should_halt;
    let halt_reason_str = halt_reason.clone();
    
    if should_halt {
        scratchpad.set_halted(&halt_reason);
    }

    let next_prompt = if should_halt {
        format!("HALT: {}. Final hypothesis ready.", halt_reason)
    } else {
        generate_refine_prompt(scratchpad, loop_index + 1, &critiques)
    };

    Ok(RecurrentContext {
        loop_index,
        confidence,
        critiques,
        should_continue,
        halt_reason: if should_halt { Some(halt_reason_str) } else { None },
        next_prompt,
    })
}

/// Check whether text found at `match_start` is negated by surrounding context.
/// Looks backwards up to 30 chars for negation words.
fn is_negated(text: &str, match_start: usize) -> bool {
    let window_start = match_start.saturating_sub(30);
    let prefix = &text[window_start..match_start];
    let prefix_lower = prefix.to_lowercase();
    NEGATION_WORDS.iter().any(|&neg| prefix_lower.contains(neg))
}

/// Critique hypothesis against anti-patterns and graph conflicts.
/// Negation-aware: if the hypothesis uses "not X" or "avoid X",
/// a match on X is NOT flagged as a violation.
fn critique_hypothesis(hypothesis: &str, conn: &Connection) -> crate::Result<Vec<String>> {
    let mut critiques = vec![];

    // Load anti-patterns
    let mut stmt = conn.prepare(
        "SELECT wrong, correct FROM anti_patterns LIMIT 50"
    )?;
    
    let anti_patterns = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
        ))
    })?;

    for ap_result in anti_patterns {
        let (wrong, right) = ap_result?;
        // Negation-aware: find ALL occurrences of `wrong` in hypothesis
        // and only flag if at least one is NOT negated.
        if hypothesis.contains(&wrong) {
            let mut is_effectively_negated = false;
            let mut search_start = 0usize;
            while let Some(pos) = hypothesis[search_start..].find(&wrong) {
                let abs_pos = search_start + pos;
                if is_negated(hypothesis, abs_pos) {
                    is_effectively_negated = true;
                    break;
                }
                search_start = abs_pos + 1;
            }
            if !is_effectively_negated {
                critiques.push(format!(
                    "Anti-pattern detected: uses '{}'. Should use '{}' instead.",
                    wrong, right
                ));
            }
        }
    }

    // Check graph conflicts (if any nodes are referenced)
    // Simple heuristic: look for type names in hypothesis that might conflict
    let mut stmt = conn.prepare(
        "SELECT ge1.from_id, ge2.to_id
         FROM graph_edges ge1
         JOIN graph_edges ge2 ON ge1.to_id = ge2.from_id
         WHERE ge1.relation = ? AND ge2.relation = ?
         LIMIT 10"
    )?;

    let conflicts = stmt.query_map(
        rusqlite::params!["pairs", "conflicts"],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;

    for conf_result in conflicts {
        let (from_id, to_id) = conf_result?;
        if hypothesis.contains(&from_id) && hypothesis.contains(&to_id) {
            critiques.push(format!(
                "Graph conflict: '{}' and '{}' marked as conflicting. Reconsider usage.",
                from_id, to_id
            ));
        }
    }

    Ok(critiques)
}

/// Score confidence along four independent dimensions so the loop doesn't
/// trivially halt just because no anti-pattern keywords matched.
///
/// Dimensions (each 0.0–1.0):
///   1. Anti-pattern compliance  (30%) — keyword match avoidance
///   2. Internal consistency     (25%) — no contradictory statements
///   3. Completeness             (25%) — required structural sections present
///   4. Grounding                (20%) — references concrete evidence (files/tools/APIs)
///
/// Final score = weighted average of the four dimensions.
/// Halt threshold of 0.85 requires ALL dimensions to be reasonably high.
fn score_confidence(
    critiques: &[String],
    conn: &Connection,
    hypothesis: &str,
) -> crate::Result<f32> {
    // ── Dimension 1: Anti-pattern compliance (30%) ────────────────────────────
    let ap_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM anti_patterns", [], |r| r.get(0)
    ).unwrap_or(0);

    let ap_score = if ap_count == 0 {
        0.8 // No checks defined → moderate
    } else {
        let violation_rate = critiques.len() as f32 / ap_count.max(1) as f32;
        (1.0 - violation_rate).max(0.0).min(1.0)
    };

    // ── Dimension 2: Internal consistency (25%) ───────────────────────────────
    // Look for contradictory pairs in the hypothesis text.
    let contradiction_pairs = [
        ("always", "never"),
        ("never", "always"),
        ("must", "must not"),
        ("should", "should not"),
        ("enable", "disable"),
        ("add", "remove"),
    ];
    let h_lower = hypothesis.to_lowercase();
    let contradiction_count = contradiction_pairs.iter()
        .filter(|(a, b)| h_lower.contains(a) && h_lower.contains(b))
        .count();
    let consistency_score = match contradiction_count {
        0 => 1.0,
        1 => 0.7,
        _ => 0.4,
    };

    // ── Dimension 3: Completeness (25%) ───────────────────────────────────────
    // Heuristic: longer, more structured hypotheses score higher.
    // Penalize very short hypotheses (< 50 chars) and reward structured ones.
    let word_count = hypothesis.split_whitespace().count();
    let has_structure = hypothesis.contains(':') || hypothesis.contains('\n') ||
                        hypothesis.contains(" — ") || hypothesis.contains(" because ");
    let completeness_score = if word_count < 5 {
        0.2
    } else if word_count < 15 {
        0.5
    } else if has_structure {
        0.95
    } else {
        0.75
    };

    // ── Dimension 4: Grounding (20%) ──────────────────────────────────────────
    // Check for concrete evidence references: file paths, function names (::),
    // tool names, or numeric evidence.
    let grounding_signals = [
        "::", ".rs", ".toml", "canvas.", "Action::", "GameEvent::",
        "cortex", "quartz", "store.", "fn ", "let ", "true", "false",
    ];
    let grounding_hits = grounding_signals.iter()
        .filter(|&&sig| hypothesis.contains(sig))
        .count();
    let grounding_score = match grounding_hits {
        0 => 0.5,  // purely conceptual — moderate
        1 => 0.7,
        2 => 0.85,
        _ => 0.95,
    };

    // ── Weighted average ──────────────────────────────────────────────────────
    let score = ap_score * 0.30
              + consistency_score * 0.25
              + completeness_score * 0.25
              + grounding_score * 0.20;

    Ok(score.max(0.0).min(1.0))
}

/// Determine if we should halt the loop.
/// Halt threshold is 0.85 — achievable when all four dimensions score well,
/// but requires concrete, consistent, grounded hypotheses. A short conceptual
/// hypothesis will score ~0.60 and need at least 2 iterations.
fn should_halt(scratchpad: &Scratchpad, max_loops: u8) -> (bool, String) {
    // Halt if multi-dimensional confidence threshold reached
    if scratchpad.confidence >= 0.85 {
        return (true, format!("confidence threshold reached (≥0.85, score={:.2})", scratchpad.confidence));
    }

    // Halt if max loops reached
    if scratchpad.loop_index >= max_loops {
        return (true, format!("max loops ({}) reached", max_loops));
    }

    // Halt if hypothesis is stable
    if scratchpad.is_stable() {
        return (true, "hypothesis stable — no further refinement needed".to_string());
    }

    (false, String::new())
}

/// Generate the prompt for the next refinement loop.
fn generate_refine_prompt(
    scratchpad: &Scratchpad,
    next_loop: u8,
    critiques: &[String],
) -> String {
    let critique_summary = if critiques.is_empty() {
        "No critiques found. Refine for completeness.".to_string()
    } else {
        let top = critiques.iter().take(3).map(|c| format!("  • {}", c)).collect::<Vec<_>>();
        format!("Address these issues:\n{}", top.join("\n"))
    };

    format!(
        "Refine hypothesis (loop {}): Current confidence: {:.0}%\n{}\n\nGenerate improved hypothesis:",
        next_loop,
        scratchpad.confidence * 100.0,
        critique_summary
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_negated_detects_negation() {
        assert!(is_negated("avoid using Action::SetPosition", 20));
        assert!(is_negated("do not use unwrap()", 12));
        assert!(is_negated("shouldn't use raw pointers", 14));
        assert!(is_negated("without momentum, this breaks", 10));
    }

    #[test]
    fn test_is_negated_passes_non_negated() {
        assert!(!is_negated("use Action::SetPosition to teleport", 30));
        assert!(!is_negated("momentum carries through", 10));
        assert!(!is_negated("this uses unwrap() safely", 12));
    }

    #[test]
    fn test_is_negated_edge_cases() {
        // Match at start of string (no room for negation prefix)
        assert!(!is_negated("Action::SetPosition is fine", 0));
        // Single char string
        assert!(!is_negated("x", 0));
        // Empty string after match
        assert!(!is_negated("", 0));
    }

    #[test]
    fn test_stability_detection() {
        let mut scratchpad = Scratchpad::new("test task", None);
        scratchpad.add_hypothesis(1, "spawn player at x=0, y=0").ok();
        scratchpad.add_hypothesis(2, "spawn player at x=0, y=0").ok();
        assert!(scratchpad.is_stable(), "Identical hypotheses should be stable");
    }

    #[test]
    fn test_confidence_bounds() {
        let confidence = (1.0_f32 - 0.5_f32).max(0.0_f32).min(1.0_f32);
        assert_eq!(confidence, 0.5);
        assert!((0.0..=1.0).contains(&confidence));
    }

    // ── Phase 5a: multi-dimensional scoring tests ─────────────────────────────

    fn make_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS anti_patterns (
                id INTEGER PRIMARY KEY,
                description TEXT, wrong TEXT, correct TEXT, tags TEXT, added_at TEXT
             );
             CREATE TABLE IF NOT EXISTS graph_edges (
                id INTEGER PRIMARY KEY,
                from_id TEXT, to_id TEXT, relation TEXT, weight REAL, source TEXT
             );"
        ).unwrap();
        conn
    }

    #[test]
    fn short_hypothesis_scores_low() {
        let conn = make_conn();
        let hypothesis = "x";
        let (score, _) = {
            let critiques: Vec<String> = vec![];
            let s = futures_or_inline_score(&critiques, &conn, hypothesis);
            (s, ())
        };
        assert!(score < 0.70, "single-char hypothesis should score < 0.70, got {score}");
    }

    #[test]
    fn well_grounded_hypothesis_scores_high() {
        let conn = make_conn();
        // Specific, grounded hypothesis referencing concrete API and reasoning
        let hypothesis = "Use Action::SetMomentum instead of Action::SetPosition \
                          because SetPosition zeroes momentum, causing physics drift. \
                          canvas.run(action) dispatches safely.";
        let critiques: Vec<String> = vec![];
        let score = futures_or_inline_score(&critiques, &conn, hypothesis);
        assert!(score >= 0.75, "grounded hypothesis should score >= 0.75, got {score}");
    }

    #[test]
    fn contradictory_hypothesis_penalized() {
        let conn = make_conn();
        let hypothesis = "You should always enable this feature, but you should never \
                          enable this feature in production environments.";
        let critiques: Vec<String> = vec![];
        let score = futures_or_inline_score(&critiques, &conn, hypothesis);
        // Contradictions lower consistency dimension
        assert!(score < 0.85, "contradictory hypothesis should score < 0.85, got {score}");
    }

    #[test]
    fn halt_threshold_not_reached_for_vague_hypothesis() {
        let conn = make_conn();
        let hypothesis = "The system should work better.";
        let critiques: Vec<String> = vec![];
        let score = futures_or_inline_score(&critiques, &conn, hypothesis);
        // Should NOT trigger halt (< 0.85)
        assert!(score < 0.85, "vague hypothesis should not halt at {score}");
    }

    // Helper: synchronously call score_confidence (no async, no DB deps beyond conn)
    fn futures_or_inline_score(critiques: &[String], conn: &rusqlite::Connection, hypothesis: &str) -> f32 {
        score_confidence(critiques, conn, hypothesis).unwrap_or(0.0)
    }
}
