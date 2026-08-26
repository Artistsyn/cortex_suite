use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Preferences {
    #[serde(default)]
    pub style: StylePrefs,
    #[serde(default)]
    pub patterns: PatternPrefs,
    #[serde(default)]
    pub api: ApiPrefs,
    #[serde(default)]
    pub project: ProjectPrefs,
    // Phase 0A: self-learning loop configuration sections.
    #[serde(default)]
    pub enforcement: EnforcementPrefs,
    #[serde(default)]
    pub consolidation: ConsolidationPrefs,
    #[serde(default)]
    pub skills: SkillsPrefs,
    #[serde(default)]
    pub memory: MemoryPrefs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StylePrefs {
    #[serde(default)]
    pub line_length: u32,
    #[serde(default)]
    pub indent: String,
    #[serde(default)]
    pub naming: String,
    #[serde(default)]
    pub error_handling: String,
    #[serde(default)]
    pub comments: String,
}

impl Default for StylePrefs {
    fn default() -> Self {
        Self {
            line_length: 0,
            indent: String::new(),
            naming: String::new(),
            error_handling: String::new(),
            comments: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PatternPrefs {
    #[serde(default)]
    pub preferred: Vec<String>,
    #[serde(default)]
    pub avoid: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApiPrefs {
    #[serde(default)]
    pub primary_building_blocks: Vec<String>,
    #[serde(default)]
    pub never_raw: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectPrefs {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub min_rust: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Protocol enforcement configuration (Phase 0A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnforcementPrefs {
    /// "protocol_session_only" (default) or "always".
    /// "protocol_session_only": Phase 0 gating only when begin_protocol_session was called.
    /// "always": gate all work tool calls in every session.
    #[serde(default = "default_protocol_gate_mode")]
    pub protocol_gate_mode: String,
    /// Warn in get_context if previous session has no closeout record.
    #[serde(default = "default_true")]
    pub closeout_warning_enabled: bool,
    /// Hours before a session is considered orphaned without closeout.
    #[serde(default = "default_closeout_grace_hours")]
    pub closeout_grace_period_hours: u32,
}

impl Default for EnforcementPrefs {
    fn default() -> Self {
        Self {
            protocol_gate_mode: default_protocol_gate_mode(),
            closeout_warning_enabled: true,
            closeout_grace_period_hours: default_closeout_grace_hours(),
        }
    }
}

fn default_protocol_gate_mode() -> String { "protocol_session_only".to_string() }
fn default_closeout_grace_hours() -> u32 { 2 }
fn default_true() -> bool { true }

/// Consolidation pipeline configuration (Phase 0A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationPrefs {
    /// Hours of staleness before consolidation runs at VS Code open.
    #[serde(default = "default_staleness_hours")]
    pub staleness_hours: u32,
    /// Maximum proposals committed per consolidation run.
    #[serde(default = "default_max_commits")]
    pub max_commits_per_run: u32,
    /// Minimum sessions in a cluster before generating proposals.
    #[serde(default = "default_min_cluster_sessions")]
    pub min_cluster_sessions: u32,
    /// Minimum occurrences before a skill candidate is drafted.
    #[serde(default = "default_skill_candidate_min")]
    pub skill_candidate_min_occurrences: u32,
    /// Graph snapshot retention days.
    #[serde(default = "default_snapshot_days")]
    pub graph_snapshot_days: u32,
}

impl Default for ConsolidationPrefs {
    fn default() -> Self {
        Self {
            staleness_hours: default_staleness_hours(),
            max_commits_per_run: default_max_commits(),
            min_cluster_sessions: default_min_cluster_sessions(),
            skill_candidate_min_occurrences: default_skill_candidate_min(),
            graph_snapshot_days: default_snapshot_days(),
        }
    }
}

fn default_staleness_hours() -> u32 { 8 }
fn default_max_commits() -> u32 { 5 }
fn default_min_cluster_sessions() -> u32 { 3 }
fn default_skill_candidate_min() -> u32 { 3 }
fn default_snapshot_days() -> u32 { 30 }

/// Skill self-authoring configuration (Phase 0A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsPrefs {
    /// Directory for skill files relative to workspace root.
    #[serde(default = "default_skills_dir")]
    pub skills_dir: String,
    /// Auto-update skill files when revision proposals are approved.
    #[serde(default = "default_true")]
    pub auto_update_skills: bool,
}

impl Default for SkillsPrefs {
    fn default() -> Self {
        Self {
            skills_dir: default_skills_dir(),
            auto_update_skills: true,
        }
    }
}

/// Where an approved skill is published.
///
/// `.claude/skills` because that is a path agents actually load: Claude Code
/// discovers `<repo>/.claude/skills/<name>/SKILL.md` automatically, and
/// `~/.claude/skills` for user-level ones. The previous default,
/// `agent_customization/skills`, is read by nothing — a skill published there is
/// approved, on disk, and invisible to every session.
///
/// Override in `prefs.toml` if your host looks somewhere else.
fn default_skills_dir() -> String { ".claude/skills".to_string() }

/// Memory file lifecycle configuration (Phase 0A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPrefs {
    /// Maximum agent memory mirror files before consolidation is recommended.
    #[serde(default = "default_max_mirror_files")]
    pub max_mirror_files: u32,
    /// Similarity threshold for mirror consolidation proposals (0.0–1.0).
    #[serde(default = "default_consolidation_threshold")]
    pub mirror_consolidation_threshold: f32,
}

impl Default for MemoryPrefs {
    fn default() -> Self {
        Self {
            max_mirror_files: default_max_mirror_files(),
            mirror_consolidation_threshold: default_consolidation_threshold(),
        }
    }
}

fn default_max_mirror_files() -> u32 { 200 }
fn default_consolidation_threshold() -> f32 { 0.75 }

pub fn load(path: &Path) -> Result<Preferences> {
    if !path.exists() {
        return Ok(Preferences::default());
    }
    let src = std::fs::read_to_string(path)?;
    let prefs: Preferences = toml::from_str(&src)?;
    Ok(prefs)
}

pub fn save(prefs: &Preferences, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let src = toml::to_string_pretty(prefs)?;
    std::fs::write(path, src)?;
    Ok(())
}

pub fn render_for_copilot(prefs: &Preferences) -> String {
    let mut out = String::new();
    out.push_str("=== PREFERENCES ===\n");

    if prefs.style.line_length > 0 {
        out.push_str(&format!("line_length: {}\n", prefs.style.line_length));
    }
    if !prefs.style.indent.is_empty() {
        out.push_str(&format!("indent: {}\n", prefs.style.indent));
    }
    if !prefs.style.naming.is_empty() {
        out.push_str(&format!("naming: {}\n", prefs.style.naming));
    }
    if !prefs.style.error_handling.is_empty() {
        out.push_str(&format!("error_handling: {}\n", prefs.style.error_handling));
    }
    if !prefs.style.comments.is_empty() {
        out.push_str(&format!("comments: {}\n", prefs.style.comments));
    }

    if !prefs.patterns.preferred.is_empty() {
        out.push_str(&format!("preferred_patterns: {}\n", prefs.patterns.preferred.join(", ")));
    }
    if !prefs.patterns.avoid.is_empty() {
        out.push_str(&format!("avoid_patterns: {}\n", prefs.patterns.avoid.join(", ")));
    }

    if !prefs.api.primary_building_blocks.is_empty() {
        out.push_str(&format!("primary_api: {}\n", prefs.api.primary_building_blocks.join(", ")));
    }
    if !prefs.api.never_raw.is_empty() {
        out.push_str(&format!("never_raw: {}\n", prefs.api.never_raw.join(", ")));
    }

    if !prefs.project.name.is_empty() {
        out.push_str(&format!("project: {}\n", prefs.project.name));
    }
    if !prefs.project.language.is_empty() {
        out.push_str(&format!("language: {}\n", prefs.project.language));
    }
    if !prefs.project.min_rust.is_empty() {
        out.push_str(&format!("min_rust: {}\n", prefs.project.min_rust));
    }
    if !prefs.project.notes.is_empty() {
        out.push_str(&format!("notes: {}\n", prefs.project.notes.join(" | ")));
    }

    out
}

/// Maximum notes expanded to full text for a single hint. Bounds the worst case
/// when a hint is broad enough to match nearly everything.
const MAX_EXPANDED_NOTES: usize = 15;

/// Characters of a collapsed note kept as its index entry.
const NOTE_INDEX_CHARS: usize = 90;

/// Tier the rendered preferences blob so only hint-relevant notes arrive in full.
///
/// `project.notes` is by far the largest payload in the preferences string — on
/// FlowMake it is ~5k tokens of the ~5.4k total — and the mandated boot sequence
/// pays for it twice, once via `get_preferences` and again inside `get_context`.
///
/// This applies the same contract `get_anti_patterns` already uses: **every note
/// is still listed**, hint-matching ones in full and the rest collapsed to their
/// opening clause, with a footer saying how to get the remainder. Nothing is ever
/// hidden, so the reduction stays lossless by construction.
///
/// `full` returns the input untouched, which is the pre-tiering behaviour.
/// How many notes a hint actually reached, and how many there were.
///
/// Separate from `tier_notes` because the caller that needs it is asking a
/// different question: not "what do I show" but "did this hint find anything at
/// all". A hint reaching ZERO notes is a retrieval miss on the most-called tool
/// in the store, and until this existed there was no way to observe one.
///
/// `(0, 0)` when there is nothing to tier — an unstructured or single-note
/// prefs blob is returned whole, so no hint can miss against it.
pub fn note_match_counts(rendered: &str, hint: Option<&str>) -> (usize, usize) {
    let Some(marker) = rendered.find("\nnotes: ") else { return (0, 0) };
    let blob = rendered[marker + "\nnotes: ".len()..].trim_end();
    let notes: Vec<&str> = blob.split(" | ").map(str::trim).filter(|n| !n.is_empty()).collect();
    if notes.len() < 2 {
        return (0, 0);
    }
    let terms = hint_terms(hint);
    let matched = notes.iter().filter(|n| score_note(n, &terms) > 0).count();
    (matched, notes.len())
}

pub fn tier_notes(rendered: &str, hint: Option<&str>, full: bool) -> String {
    if full {
        return rendered.to_string();
    }
    // `notes:` is emitted last by render_for_copilot, and individual notes may
    // themselves contain newlines, so the blob runs from the marker to the end.
    let Some(marker) = rendered.find("\nnotes: ") else {
        return rendered.to_string();
    };
    let head = &rendered[..marker + 1];
    let blob = rendered[marker + "\nnotes: ".len()..].trim_end();
    let notes: Vec<&str> = blob.split(" | ").map(str::trim).filter(|n| !n.is_empty()).collect();
    if notes.len() < 2 {
        return rendered.to_string();
    }

    let terms = hint_terms(hint);
    let mut scored: Vec<(usize, usize)> = notes
        .iter()
        .enumerate()
        .map(|(i, note)| (i, score_note(note, &terms)))
        .filter(|(_, s)| *s > 0)
        .collect();
    // Strongest match first, then original order, so the cap keeps the best ones.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let expanded: std::collections::HashSet<usize> =
        scored.iter().take(MAX_EXPANDED_NOTES).map(|(i, _)| *i).collect();

    let mut out = String::from(head);
    out.push_str("notes:\n");
    for (i, note) in notes.iter().enumerate() {
        if expanded.contains(&i) {
            out.push_str(&format!("  - {note}\n"));
        } else {
            out.push_str(&format!("  - {}\n", first_clause(note, NOTE_INDEX_CHARS)));
        }
    }
    let collapsed = notes.len() - expanded.len();
    if collapsed > 0 {
        out.push_str(&format!(
            "\n({collapsed} of {} notes shown as opening clause only — every note is listed above. \
             For the rest: get_preferences with a narrower hint, or detail=\"full\".)\n",
            notes.len()
        ));
    }
    out
}

/// Lowercased hint tokens long enough to be discriminating.
fn hint_terms(hint: Option<&str>) -> Vec<String> {
    hint.unwrap_or("")
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 4)
        .map(str::to_string)
        .collect()
}

/// Number of distinct hint terms a note mentions.
fn score_note(note: &str, terms: &[String]) -> usize {
    if terms.is_empty() {
        return 0;
    }
    let lower = note.to_lowercase();
    terms.iter().filter(|t| lower.contains(t.as_str())).count()
}

/// First sentence of a note, or a word-boundary truncation, whichever is shorter.
fn first_clause(note: &str, max_chars: usize) -> String {
    let flat = note.replace('\n', " ");
    let flat = flat.trim();
    // A sentence end inside the budget is already a clean cut — take it whole.
    if let Some(end) = flat.find(". ").map(|i| i + 1).filter(|i| *i <= max_chars) {
        return format!("{} …", &flat[..end]);
    }
    if flat.len() <= max_chars {
        return flat.to_string();
    }
    // Hard truncation: step back to a char boundary, then to the last whole word.
    let mut cut = max_chars;
    while cut > 0 && !flat.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = flat[..cut].rfind(' ').unwrap_or(cut);
    format!("{} …", flat[..cut].trim_end())
}

#[cfg(test)]
mod tier_notes_tests {
    use super::*;

    fn rendered(notes: &[&str]) -> String {
        format!("=== PREFERENCES ===\nproject: FlowMake\nnotes: {}\n", notes.join(" | "))
    }

    /// The whole safety claim: tiering may shorten a note, never remove one.
    #[test]
    fn every_note_is_still_listed_whatever_the_hint() {
        let notes: Vec<String> =
            (0..40).map(|i| format!("note {i} about subject{i} with a long tail of explanatory text \
                                     that would otherwise cost tokens on every single boot")).collect();
        let refs: Vec<&str> = notes.iter().map(String::as_str).collect();
        let input = rendered(&refs);
        for hint in [None, Some("subject3"), Some("nothing matches this"), Some("note")] {
            let out = tier_notes(&input, hint, false);
            let listed = out.lines().filter(|l| l.trim_start().starts_with("- ")).count();
            assert_eq!(listed, 40, "hint {hint:?} dropped notes");
        }
    }

    #[test]
    fn hint_matched_notes_arrive_in_full_and_others_collapse() {
        let long_tail = "and here is a great deal of additional trailing detail that pushes this \
                         note well past the index threshold so truncation is observable";
        let input = rendered(&[
            &format!("grapple constraint behaviour. {long_tail}"),
            &format!("gif decoding behaviour. {long_tail}"),
        ]);
        let out = tier_notes(&input, Some("grapple constraint swing"), false);
        assert!(out.contains(long_tail), "matched note should be complete");
        assert!(out.contains("gif decoding behaviour. …"), "unmatched note should collapse");
        assert!(out.contains("1 of 2 notes"), "footer should report what was collapsed");
    }

    #[test]
    fn detail_full_is_byte_identical_to_the_input() {
        let input = rendered(&["alpha one", "beta two"]);
        assert_eq!(tier_notes(&input, Some("alpha"), true), input);
    }

    #[test]
    fn preferences_without_a_notes_field_pass_through_untouched() {
        let input = "=== PREFERENCES ===\nproject: FlowMake\nlanguage: Rust\n";
        assert_eq!(tier_notes(input, Some("anything"), false), input);
    }

    /// Notes contain em-dashes and other multibyte characters; truncation must
    /// not slice through one.
    #[test]
    fn truncation_respects_multibyte_boundaries() {
        let note = "café — naïve façade ".repeat(20);
        let input = rendered(&[&note, "unrelated"]);
        let out = tier_notes(&input, Some("zzz"), false);
        assert!(out.contains(" …"));
    }
}

/// Reports real tiering savings against the live prefs file. Ignored by default
/// because it depends on the workspace's own .cortex/prefs.toml.
/// Run: cargo test --quiet measure_real_prefs -- --ignored --nocapture
#[cfg(test)]
#[test]
#[ignore]
fn measure_real_prefs_savings() {
    let path = std::path::Path::new("../.cortex/prefs.toml");
    let Ok(prefs) = load(path) else { return };
    let full = render_for_copilot(&prefs);
    println!("  notes entries      : {}", prefs.project.notes.len());
    println!("  full render        : {:>7} chars (~{} tokens)", full.len(), full.len() / 4);
    for (label, hint) in [
        ("boot, no hint      ", None),
        ("hint: quartz plugin", Some("quartz plugin action dispatch on_action on_call")),
        ("hint: vr renderer  ", Some("space_soup xr_renderer scope optic avatar rig")),
    ] {
        let t = tier_notes(&full, hint, false);
        println!(
            "  {label}: {:>7} chars (~{} tokens)  saved {:>6} chars (~{} tokens, {:.0}%)",
            t.len(), t.len() / 4, full.len() - t.len(), (full.len() - t.len()) / 4,
            100.0 * (full.len() - t.len()) as f64 / full.len() as f64
        );
    }
    /// A hint that reaches nothing is the signal; the counts are how it is seen.
    #[test]
    fn note_match_counts_reports_a_hint_that_reached_nothing() {
        let input = "style: terse\nnotes: prefer the shared helper here | naming is snake_case\n";
        assert_eq!(note_match_counts(input, Some("shared helper")), (1, 2));
        assert_eq!(
            note_match_counts(input, Some("database migration rollback")),
            (0, 2),
            "a hint about something the prefs say nothing about must read as zero"
        );
    }

    /// Nothing to tier means no hint can miss against it, so it must not be
    /// reported as a gap — otherwise every call on a small prefs file is a miss.
    #[test]
    fn an_untierable_blob_reports_no_notes_rather_than_a_miss() {
        assert_eq!(note_match_counts("style: terse\n", Some("anything")), (0, 0));
        assert_eq!(
            note_match_counts("style: terse\nnotes: only one\n", Some("anything")),
            (0, 0),
            "a single note is returned whole, so it cannot be missed"
        );
    }

}
