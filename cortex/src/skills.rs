/// Phase 1: Skill candidate detection from mcp_calls sequences.
///
/// Scans session snapshots for repeated tool-call patterns.
/// When the same tool sequence appears >= min_occurrences times,
/// it becomes a skill candidate in the skill_candidates table.
///
/// Also handles skill draft generation and health metrics.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::params;

use crate::memory::Store;
use crate::miner::SessionCluster;

// ── Skill candidate detection ─────────────────────────────────────────────────

/// Detect skill candidates from session clusters.
/// A cluster with >= min_occurrences and a stable tool sequence becomes a candidate.
pub fn detect_skill_candidates(
    store: &Store,
    clusters: &[SessionCluster],
    min_occurrences: u32,
) -> Result<Vec<SkillCandidateSummary>> {
    let mut promoted = Vec::new();

    for cluster in clusters {
        if cluster.members.len() < min_occurrences as usize { continue; }
        if cluster.tool_sequence.is_empty() { continue; }

        // Derive a name from the top tools in the sequence.
        let name = derive_skill_name(&cluster.tool_sequence);
        if name.is_empty() { continue; }

        // Compute confidence: fraction of sessions that produced build_pass.
        let pass = cluster.outcome_counts.get("build_pass").copied().unwrap_or(0);
        let total = cluster.members.len();
        let confidence = if total > 0 { pass as f32 / total as f32 } else { 0.0 };

        let seq_json = serde_json::to_string(&cluster.tool_sequence)
            .unwrap_or_else(|_| "[]".to_string());
        let keys_json = serde_json::to_string(&cluster.members)
            .unwrap_or_else(|_| "[]".to_string());

        // Upsert into skill_candidates table.
        store.conn().execute(
            "INSERT INTO skill_candidates
                 (name, trigger_hint, tool_sequence, session_keys, occurrence_count,
                  confidence, status, first_seen_at, last_seen_at)
             VALUES (?1, '', ?2, ?3, ?4, ?5, 'candidate', unixepoch(), unixepoch())
             ON CONFLICT(name) DO UPDATE SET
                 tool_sequence    = excluded.tool_sequence,
                 session_keys     = excluded.session_keys,
                 occurrence_count = excluded.occurrence_count,
                 confidence       = excluded.confidence,
                 last_seen_at     = unixepoch()",
            params![name, seq_json, keys_json, total as i64, confidence],
        )?;

        promoted.push(SkillCandidateSummary {
            name,
            occurrence_count: total,
            confidence,
            tool_sequence: cluster.tool_sequence.clone(),
        });
    }

    Ok(promoted)
}

/// Derive a human-readable skill name from a tool sequence.
fn derive_skill_name(tools: &[String]) -> String {
    // Strip common bootstrap tools — they appear in every session.
    let skip = ["get_delta", "get_preferences", "get_anti_patterns", "get_context",
                "list_patterns", "begin_protocol_session", "get_session_health",
                "flush_knowledge_markers", "closeout_session"];

    let domain_tools: Vec<&str> = tools.iter()
        .filter(|t| !skip.contains(&t.as_str()))
        .map(|t| t.as_str())
        .collect();

    if domain_tools.is_empty() { return String::new(); }

    // Map tool names to domain keywords.
    let keyword_for = |t: &str| -> &'static str {
        if t.contains("quartz") || t.contains("forge") { return "quartz-forge"; }
        if t.contains("graph") || t.contains("simulate") { return "graph-analysis"; }
        if t.contains("recall") || t.contains("semantic") { return "knowledge-lookup"; }
        if t.contains("get_item") || t.contains("get_syntax") { return "api-lookup"; }
        if t.contains("crystallize") || t.contains("suggest_pattern") { return "pattern-capture"; }
        "general"
    };

    let first_key = keyword_for(domain_tools[0]);
    format!("workflow-{}", first_key)
}

// ── Draft SKILL.md generation ─────────────────────────────────────────────────

/// Generate a draft SKILL.md for a skill candidate and write it to the proposals dir.
pub fn draft_skill_file(
    name: &str,
    tool_sequence: &[String],
    occurrence_count: usize,
    confidence: f32,
    proposals_dir: &Path,
    skills_dir_hint: &str,
) -> Result<String> {
    std::fs::create_dir_all(proposals_dir)?;

    let safe_name = name.replace(['/', '\\', ' '], "-");
    let filename  = format!("skill_{safe_name}.md");
    let path      = proposals_dir.join(&filename);

    // Build the procedure section from the tool sequence (skip bootstrap tools).
    let skip = ["get_delta", "get_preferences", "get_anti_patterns", "get_context",
                "list_patterns", "begin_protocol_session", "get_session_health"];
    let domain_tools: Vec<&str> = tool_sequence.iter()
        .filter(|t| !skip.contains(&t.as_str()))
        .map(|t| t.as_str())
        .collect();

    let procedure = domain_tools.iter().enumerate()
        .map(|(i, t)| format!("{}. Call `{}`", i + 1, t))
        .collect::<Vec<_>>()
        .join("\n");

    let tool_list = tool_sequence.join(", ");

    let content = format!(
r#"# {name}
<!-- Auto-generated by cortex detect-skills -->
<!-- Occurrences: {occurrence_count} | Confidence: {confidence:.0}% | Detected: {date} -->
<!-- Review and approve: cortex skill-approve {safe_name} -->
<!-- Target: {skills_dir_hint}/{safe_name}/SKILL.md -->

## Purpose

Auto-detected workflow pattern appearing in {occurrence_count} sessions with {confidence:.0}% success rate.
Review the procedure below, refine as needed, then approve.

## Use This Skill When

- [Edit: describe when to invoke this skill based on user requests]

## Do Not Use This Skill When

- [Edit: describe when NOT to use this skill]

## Required Inputs

- Active Cortex PROTOCOL session with Phase 0 complete

## Procedure

{procedure}

## Tool Routing Rules

- Required tools: {tool_list}
- Required verification: get_session_health after completion

## Output Contract

- Report outcome with Trust: verified/inferred
- Write CORTEX-PATTERN or CORTEX-AP markers for any discoveries
- Call closeout_session at end

## Validation Checklist

- [ ] begin_protocol_session called
- [ ] Phase 0 complete (get_session_health confirmed)
- [ ] Knowledge markers written for any discoveries
- [ ] closeout_session called with correct outcome_type
"#,
        name = name,
        occurrence_count = occurrence_count,
        confidence = confidence * 100.0,
        date = chrono::Utc::now().format("%Y-%m-%d"),
        safe_name = safe_name,
        skills_dir_hint = skills_dir_hint,
        procedure = procedure,
        tool_list = tool_list,
    );

    std::fs::write(&path, content)?;
    Ok(path.to_string_lossy().to_string())
}

// ── Agent-authored skill drafts ───────────────────────────────────────────────

/// Write a SKILL.md draft from content the agent actually authored.
///
/// Unlike `draft_skill_file` (a placeholder template derived from tool names),
/// this preserves the agent's own procedure text — written at closeout time
/// when the full session experience is still in its context window. This is
/// the real self-authoring path; the template version is only a fallback for
/// pipeline-detected candidates no agent has authored yet.
#[allow(clippy::too_many_arguments)]
pub fn write_authored_skill_file(
    name: &str,
    trigger: &str,
    procedure: &str,
    when_not_to_use: &str,
    tool_sequence: &[String],
    proposals_dir: &Path,
    skills_dir_hint: &str,
) -> Result<String> {
    std::fs::create_dir_all(proposals_dir)?;

    let safe_name = name.replace(['/', '\\', ' '], "-");
    let filename  = format!("skill_{safe_name}.md");
    let path      = proposals_dir.join(&filename);

    let trigger_section = if trigger.is_empty() {
        "- [Edit: describe when to invoke this skill]".to_string()
    } else {
        trigger.lines()
            .map(|l| if l.trim_start().starts_with('-') { l.to_string() } else { format!("- {l}") })
            .collect::<Vec<_>>().join("\n")
    };
    let avoid_section = if when_not_to_use.is_empty() {
        "- [Edit: describe when NOT to use this skill]".to_string()
    } else {
        when_not_to_use.lines()
            .map(|l| if l.trim_start().starts_with('-') { l.to_string() } else { format!("- {l}") })
            .collect::<Vec<_>>().join("\n")
    };
    let tools_section = if tool_sequence.is_empty() {
        String::new()
    } else {
        format!("\n## Tool Routing Rules\n\n- Tools used: {}\n", tool_sequence.join(", "))
    };

    let content = format!(
r#"# {name}
<!-- Agent-authored via propose_skill | Drafted: {date} -->
<!-- Review and approve: cortex skill-approve {safe_name} -->
<!-- Target: {skills_dir_hint}/{safe_name}/SKILL.md -->

## Use This Skill When

{trigger_section}

## Do Not Use This Skill When

{avoid_section}

## Procedure

{procedure}
{tools_section}
## Output Contract

- Report outcome with Trust: verified/inferred
- Write CORTEX-PATTERN or CORTEX-AP markers for any discoveries
- Call closeout_session at end
"#,
        name = name,
        date = chrono::Utc::now().format("%Y-%m-%d"),
        safe_name = safe_name,
        skills_dir_hint = skills_dir_hint,
        trigger_section = trigger_section,
        avoid_section = avoid_section,
        procedure = procedure,
        tools_section = tools_section,
    );

    std::fs::write(&path, content)?;
    Ok(path.to_string_lossy().to_string())
}

/// Ensure a skill candidate row exists for an agent-proposed skill, so the
/// pipeline tracks it (status/draft_path/usage) like detected candidates.
pub fn upsert_agent_candidate(
    store: &Store,
    name: &str,
    trigger: &str,
    session_key: &str,
    tool_sequence: &[String],
) -> Result<()> {
    let seq_json = serde_json::to_string(tool_sequence).unwrap_or_else(|_| "[]".to_string());
    store.conn().execute(
        "INSERT INTO skill_candidates
             (name, trigger_hint, tool_sequence, session_keys, occurrence_count,
              confidence, status, first_seen_at, last_seen_at)
         VALUES (?1, ?2, ?3, json_array(?4), 1, 0.8, 'candidate', unixepoch(), unixepoch())
         ON CONFLICT(name) DO UPDATE SET
             trigger_hint = CASE WHEN excluded.trigger_hint != '' THEN excluded.trigger_hint
                                 ELSE skill_candidates.trigger_hint END,
             last_seen_at = unixepoch()",
        params![name, trigger, seq_json, session_key],
    )?;
    Ok(())
}

// ── Skill health metrics ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SkillCandidateSummary {
    pub name:             String,
    pub occurrence_count: usize,
    pub confidence:       f32,
    pub tool_sequence:    Vec<String>,
}

/// Load all skill candidates from the DB.
pub fn list_skill_candidates(store: &Store) -> Result<Vec<SkillCandidateSummary>> {
    let mut stmt = store.conn().prepare(
        "SELECT name, occurrence_count, confidence, tool_sequence
         FROM skill_candidates
         ORDER BY occurrence_count DESC, confidence DESC"
    )?;
    let rows = stmt.query_map([], |row| {
        let seq_json: String = row.get(3)?;
        let seq: Vec<String> = serde_json::from_str(&seq_json).unwrap_or_default();
        Ok(SkillCandidateSummary {
            name:             row.get(0)?,
            occurrence_count: row.get::<_, i64>(1)? as usize,
            confidence:       row.get::<_, f32>(2)?,
            tool_sequence:    seq,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Set a skill candidate status (candidate / drafted / approved / rejected).
/// Copy an approved skill draft to the live skills directory.
///
/// `skill-approve` used to flip a status column and stop there, printing
/// "marked as approved" — so every skill approved before 2026-08-05 was recorded
/// as live and existed nowhere on disk. Approval that does not publish is
/// approval that reports success and changes nothing.
///
/// Returns the path written. Refuses rather than publishing when:
///   - there is no draft file (nothing to publish)
///   - the draft still carries `[Edit: ...]` placeholders, unless `force`
///
/// The placeholder guard matters because skill detection fires on repeated tool
/// sequences: a thin signal produces a draft whose body is template text, and
/// publishing that costs tokens in every future session while teaching nothing.
pub fn publish_skill(
    name: &str,
    draft_path: &Path,
    skills_dir: &Path,
    copilot_prompts_dir: Option<&Path>,
    force: bool,
) -> Result<Vec<PathBuf>> {
    if !draft_path.exists() {
        anyhow::bail!(
            "no draft file at {} - nothing to publish. Re-draft with propose_skill \
             before approving.",
            draft_path.display()
        );
    }

    let body = std::fs::read_to_string(draft_path)?;

    if !force {
        if let Some(line) = body.lines().find(|l| l.contains("[Edit:")) {
            anyhow::bail!(
                "draft is still a template - {} contains a placeholder:\n    {}\n\
                 Fill it in, or re-run with --force to publish as-is.",
                draft_path.display(),
                line.trim()
            );
        }
    }

    // Drop the review-workflow comments; they are scaffolding for a decision
    // that has now been made. Provenance lines are kept.
    let published: String = body
        .lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("<!--")
                && (t.contains("Review and approve:") || t.contains("Target:")))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let safe_name = name.replace(['/', '\\', ' '], "-");
    let mut written = Vec::new();

    // Claude Code: <repo>/.claude/skills/<name>/SKILL.md is auto-discovered.
    let dest_dir = skills_dir.join(&safe_name);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("SKILL.md");
    std::fs::write(&dest, &published)?;
    written.push(dest);

    // VS Code Copilot reads none of that. Its equivalent surface is a prompt
    // file under `.github/prompts/`, invoked as `/<name>` in Copilot Chat.
    // Publishing to only one host is how a skill ends up live for half the team.
    if let Some(prompts_dir) = copilot_prompts_dir {
        std::fs::create_dir_all(prompts_dir)?;
        let summary = first_prose_line(&published)
            .unwrap_or_else(|| format!("Skill: {name}"));
        let prompt = format!(
            "---\nmode: agent\ndescription: \"{}\"\n---\n\n{}\n",
            summary.replace('"', "'"),
            published,
        );
        let prompt_path = prompts_dir.join(format!("{safe_name}.prompt.md"));
        std::fs::write(&prompt_path, prompt)?;
        written.push(prompt_path);
    }

    Ok(written)
}

/// First real sentence of a skill body, for a prompt-file description.
/// Skips the heading, HTML comments, blank lines and section headers.
fn first_prose_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("<!--"))
        .map(|l| l.chars().take(160).collect())
}

pub fn set_skill_status(store: &Store, name: &str, status: &str) -> Result<usize> {
    let changed = store.conn().execute(
        "UPDATE skill_candidates SET status = ?1 WHERE name = ?2",
        params![status, name],
    )?;
    Ok(changed)
}

/// Set the draft_path for a skill candidate.
pub fn set_skill_draft_path(store: &Store, name: &str, path: &str) -> Result<()> {
    store.conn().execute(
        "UPDATE skill_candidates SET draft_path = ?1, status = 'drafted' WHERE name = ?2",
        params![path, name],
    )?;
    Ok(())
}

/// Format the skill candidate list as a human-readable table.
pub fn format_skill_status(candidates: &[SkillCandidateSummary]) -> String {
    if candidates.is_empty() {
        return "No skill candidates detected yet. More sessions are needed.\n".to_string();
    }

    let mut out = format!("{} skill candidate(s):\n\n", candidates.len());
    for c in candidates {
        let first_tools: Vec<_> = c.tool_sequence.iter().take(3).collect();
        out.push_str(&format!(
            "  {} — {} sessions, {:.0}% success\n    tools: {}\n",
            c.name,
            c.occurrence_count,
            c.confidence * 100.0,
            first_tools.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" → "),
        ));
    }
    out
}

// ── Gap-driven proposal detection ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GapProposal {
    pub query_text: String,
    pub tool_name:  String,
    pub seen_count: i64,
    pub proposed_note: String,
}

/// Find query gaps that have been seen >= min_count times and propose prefs notes.
pub fn detect_gap_proposals(store: &Store, min_count: i64) -> Result<Vec<GapProposal>> {
    let mut stmt = store.conn().prepare(
        "SELECT tool_name, query_text, seen_count
         FROM query_gap_log
         WHERE seen_count >= ?1
         ORDER BY seen_count DESC
         LIMIT 20"
    )?;
    let rows = stmt.query_map(params![min_count], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let mut proposals = Vec::new();
    for row in rows {
        let (tool, query, count) = row?;
        // Skip if we already have a prefs note or annotation covering this query.
        let already_covered: i64 = store.conn().query_row(
            "SELECT COUNT(*) FROM annotations
             WHERE lower(body) LIKE lower(?1) OR lower(topic) LIKE lower(?1)",
            params![format!("%{}%", query.chars().take(40).collect::<String>())],
            |r| r.get(0),
        ).unwrap_or(0);

        if already_covered == 0 {
            let note = format!(
                "Frequent retrieval miss ({count}x): '{query}' via {tool} — consider adding a prefs note or annotation."
            );
            proposals.push(GapProposal {
                query_text: query,
                tool_name: tool,
                seen_count: count,
                proposed_note: note,
            });
        }
    }

    Ok(proposals)
}

#[cfg(test)]
mod tests {
    /// Approval must produce a file or an error - never a success message with
    /// nothing on disk, which is what `skill-approve` did for every skill
    /// approved before 2026-08-05.
    #[test]
    fn publish_writes_a_file_and_refuses_templates() {
        let base = std::env::temp_dir().join("cortex_publish_skill_test");
        let _ = std::fs::remove_dir_all(&base);
        let drafts = base.join("proposals");
        let skills = base.join("skills");
        std::fs::create_dir_all(&drafts).unwrap();

        // No draft -> refuse. Nothing to publish is not an approval.
        let missing = drafts.join("skill_absent.md");
        assert!(super::publish_skill("absent", &missing, &skills, None, false).is_err());

        // Placeholder draft -> refuse, and name the offending line.
        let tmpl = drafts.join("skill_thin.md");
        std::fs::write(&tmpl, "# thin

## Procedure

- [Edit: describe when to use this]
").unwrap();
        let err = super::publish_skill("thin", &tmpl, &skills, None, false).unwrap_err().to_string();
        assert!(err.contains("[Edit:"), "{err}");
        assert!(!skills.join("thin").join("SKILL.md").exists(), "refusal must not write");

        // ...unless forced.
        assert!(super::publish_skill("thin", &tmpl, &skills, None, true).is_ok());

        // A real draft publishes, minus the review scaffolding.
        let real = drafts.join("skill_real.md");
        std::fs::write(&real,
            "# real
<!-- Agent-authored via propose_skill | Drafted: 2026-08-05 -->
             <!-- Review and approve: cortex skill-approve real -->
             <!-- Target: agent_customization/skills/real/SKILL.md -->

## Procedure

1. Do the thing.
").unwrap();
        let prompts = base.join(".github").join("prompts");
        let out_paths = super::publish_skill("real", &real, &skills, Some(&prompts), false).unwrap();
        assert_eq!(out_paths.len(), 2, "both hosts must get a copy: {out_paths:?}");
        let dest = &out_paths[0];
        assert!(dest.exists());
        assert_eq!(dest, &skills.join("real").join("SKILL.md"));
        let out = std::fs::read_to_string(dest).unwrap();
        assert!(out.contains("Do the thing."));
        assert!(out.contains("Agent-authored"), "provenance is kept");
        assert!(!out.contains("Review and approve:"), "decision scaffolding is dropped");
        assert!(!out.contains("Target:"), "decision scaffolding is dropped");

        // Copilot has no skills mechanism; its surface is a prompt file
        // invoked as /real in Copilot Chat.
        let prompt = &out_paths[1];
        assert_eq!(prompt, &prompts.join("real.prompt.md"));
        let ptext = std::fs::read_to_string(prompt).unwrap();
        assert!(ptext.starts_with("---\nmode: agent\n"), "{ptext}");
        assert!(ptext.contains("description: \"1. Do the thing.\""), "{ptext}");
        assert!(ptext.contains("Do the thing."));

        let _ = std::fs::remove_dir_all(&base);
    }
}
