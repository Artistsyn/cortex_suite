/// Pattern crystallization — always manual, always Syn's decision.
///
/// The crystallizer never reads git history, never auto-approves anything.
/// It presents observed file changes or Copilot suggestions and waits for
/// explicit `approve` or `dismiss` commands before anything enters memory.
///
/// Workflow:
///   cortex review              — list pending observations
///   cortex crystallize <id>    — promote an observation to a named pattern
///   cortex dismiss <id>        — discard an observation
///   cortex pattern add         — add a pattern directly without an observation
///   cortex anti-pattern add    — add a known bad approach
use anyhow::Result;

use crate::memory::Store;
use crate::model::{AntiPattern, Annotation, Pattern, PendingObservation};

// ── Review pending observations ───────────────────────────────────────────────

pub fn list_pending(store: &Store) -> Result<()> {
    let observations = store.all_observations()?;

    if observations.is_empty() {
        println!("No pending observations.");
        println!("Run `cortex watch` to start observing file changes.");
        return Ok(());
    }

    println!("{} pending observation(s):\n", observations.len());
    for obs in &observations {
        let id = obs.id.unwrap_or(0);
        println!("  [{}] {}", id, obs.path);
        println!("      {}", obs.summary);
        if !obs.diff_hint.is_empty() {
            for line in obs.diff_hint.lines().take(5) {
                println!("      {}", line);
            }
        }
        println!();
    }

    println!("Commands:");
    println!("  cortex crystallize <id> --name <name> --intent \"<intent>\"");
    println!("  cortex dismiss <id>");

    Ok(())
}

// ── Crystallize an observation into a pattern ─────────────────────────────────

pub fn crystallize_observation(
    store: &Store,
    obs_id: i64,
    name: &str,
    intent: &str,
    body: Option<&str>,
    uses: Vec<String>,
    tags: Vec<String>,
) -> Result<()> {
    let observations = store.all_observations()?;
    let obs = observations
        .iter()
        .find(|o| o.id == Some(obs_id))
        .ok_or_else(|| anyhow::anyhow!("no observation with id {}", obs_id))?;

    let body = body.unwrap_or(&obs.diff_hint).to_string();

    let pattern = Pattern {
        id: None,
        name: name.to_string(),
        intent: intent.to_string(),
        body,
        uses,
        tags,
        approved_at: chrono::Utc::now(),
        use_count: 0,
        reverted_count: 0,
        survival_rate: 1.0,
    };

    let pid = store.insert_pattern(&pattern)?;
    store.dismiss_observation(obs_id)?;

    println!("✓ Pattern `{}` saved (id: {}).", name, pid);
    println!("  Intent: {}", intent);
    Ok(())
}

pub fn dismiss_observation(store: &Store, obs_id: i64) -> Result<()> {
    store.dismiss_observation(obs_id)?;
    println!("Dismissed observation {}.", obs_id);
    Ok(())
}

// ── Add patterns directly ─────────────────────────────────────────────────────

pub fn add_pattern(
    store: &Store,
    name: &str,
    intent: &str,
    body: &str,
    uses: Vec<String>,
    tags: Vec<String>,
) -> Result<()> {
    let pattern = Pattern {
        id: None,
        name: name.to_string(),
        intent: intent.to_string(),
        body: body.to_string(),
        uses,
        tags,
        approved_at: chrono::Utc::now(),
        use_count: 0,
        reverted_count: 0,
        survival_rate: 1.0,
    };
    let id = store.insert_pattern(&pattern)?;
    println!("✓ Pattern `{}` added (id: {}).", name, id);
    Ok(())
}

pub fn remove_pattern(store: &Store, id: i64) -> Result<()> {
    store.delete_pattern(id)?;
    println!("Removed pattern {}.", id);
    Ok(())
}

pub fn list_patterns(store: &Store) -> Result<()> {
    let patterns = store.all_patterns()?;
    if patterns.is_empty() {
        println!("No patterns yet. Use `cortex pattern add` to add one.");
        return Ok(());
    }
    println!("{} pattern(s):\n", patterns.len());
    for p in &patterns {
        println!(
            "  [{}] {} (used {} time(s), reverted {}, survival {:.0}%)",
            p.id.unwrap_or(0),
            p.name,
            p.use_count,
            p.reverted_count,
            p.survival_rate * 100.0
        );
        println!("       Intent: {}", p.intent);
        if !p.tags.is_empty() {
            println!("       Tags: {}", p.tags.join(", "));
        }
        println!();
    }
    Ok(())
}

pub fn report_revert(store: &Store, pattern_id: i64) -> Result<()> {
    store.pattern_reverted(pattern_id)?;
    println!("Marked pattern {} as reverted once.", pattern_id);
    Ok(())
}

pub fn list_pattern_health(store: &Store) -> Result<()> {
    let rows = store.pattern_health_rows()?;
    if rows.is_empty() {
        println!("No patterns available for health view.");
        return Ok(());
    }

    println!("pattern health:\n");
    for (_id, name, use_count, reverted_count, survival_rate) in rows {
        let marker = if survival_rate < 0.4 { "⚠" } else { "✓" };
        println!(
            "  {} {} ({:.0}%) use={} reverted={}",
            marker,
            name,
            survival_rate * 100.0,
            use_count,
            reverted_count
        );
    }
    Ok(())
}

// ── Anti-patterns ─────────────────────────────────────────────────────────────

pub fn add_anti_pattern(
    store: &Store,
    description: &str,
    wrong: &str,
    correct: &str,
    tags: Vec<String>,
) -> Result<()> {
    let ap = AntiPattern {
        id: None,
        description: description.to_string(),
        wrong: wrong.to_string(),
        correct: correct.to_string(),
        tags,
        added_at: chrono::Utc::now(),
    };
    let id = store.insert_anti_pattern(&ap)?;
    println!("✓ Anti-pattern added (id: {}).", id);
    Ok(())
}

pub fn remove_anti_pattern(store: &Store, id: i64) -> Result<()> {
    store.delete_anti_pattern(id)?;
    println!("Removed anti-pattern {}.", id);
    Ok(())
}

pub fn list_anti_patterns(store: &Store) -> Result<()> {
    let aps = store.all_anti_patterns()?;
    if aps.is_empty() {
        println!("No anti-patterns yet.");
        return Ok(());
    }
    println!("{} anti-pattern(s):\n", aps.len());
    for ap in &aps {
        println!("  [{}] {}", ap.id.unwrap_or(0), ap.description);
        println!("       ✗ wrong:   {}", ap.wrong);
        println!("       ✓ correct: {}", ap.correct);
        if !ap.tags.is_empty() { println!("       tags: {}", ap.tags.join(", ")); }
        println!();
    }
    Ok(())
}

// ── Annotations ───────────────────────────────────────────────────────────────

pub fn add_annotation(store: &Store, topic: &str, body: &str, tags: Vec<String>) -> Result<()> {
    let a = Annotation {
        id: None,
        topic: topic.to_string(),
        body: body.to_string(),
        tags,
        added_at: chrono::Utc::now(),
    };
    let id = store.insert_annotation(&a)?;
    println!("✓ Annotation added (id: {}).", id);
    Ok(())
}

pub fn remove_annotation(store: &Store, id: i64) -> Result<()> {
    store.delete_annotation(id)?;
    println!("Removed annotation {}.", id);
    Ok(())
}

pub fn list_annotations(store: &Store) -> Result<()> {
    let annotations = store.all_annotations()?;
    if annotations.is_empty() {
        println!("No annotations yet.");
        return Ok(());
    }
    println!("{} annotation(s):\n", annotations.len());
    for a in &annotations {
        println!("  [{}] {}", a.id.unwrap_or(0), a.topic);
        println!("       {}", a.body);
        if !a.tags.is_empty() { println!("       tags: {}", a.tags.join(", ")); }
        println!();
    }
    Ok(())
}

// ── File watcher observation recording ───────────────────────────────────────

/// Record that a file changed — does NOT promote it to a pattern.
/// Queues it for Syn's review only.
pub fn record_observation(store: &Store, path: &str, summary: &str, diff_hint: &str) -> Result<()> {
    let obs = PendingObservation {
        id: None,
        path: path.to_string(),
        summary: summary.to_string(),
        diff_hint: diff_hint.to_string(),
        observed_at: chrono::Utc::now(),
    };
    let id = store.add_observation(&obs)?;
    eprintln!("  [obs:{}] recorded change: {}", id, path);
    Ok(())
}
