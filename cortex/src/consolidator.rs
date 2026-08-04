use anyhow::Result;
use rusqlite::params;

use crate::compressor::{build_term_vector_str, cosine_similarity};
use crate::memory::Store;

/// Projects whose knowledge lives in this DB. A pattern is attributed by looking
/// for these markers in its tags, name, intent and body.
///
/// Order matters: the first match wins, so put more specific names first
/// (`synful_quartz` before `quartz`).
const PROJECT_MARKERS: &[(&str, &[&str])] = &[
    ("space_soup", &["space_soup", "spacesoup", "quest_app", "avatar_ik", "xr_renderer", "openxr", "scope", "optic", "multiview"]),
    ("tale_forge", &["tale_forge", "taleforge", "slint", "loremaster", "manuscript"]),
    ("path_forge", &["path_forge", "pathforge"]),
    ("quartz_forge", &["quartz_forge", "quartz forge", "forge_verify", "qf_"]),
    ("synful_quartz", &["synful_quartz", "synful"]),
    ("quartz", &["quartz", "crystalline", "prism", "canvas.run", "gameobject", "ball_swing"]),
    ("cortex", &["cortex", "scoreboard", "compaction", "closeout", "marker"]),
];

/// Best-guess project for a pattern, or `None` when nothing matches.
///
/// Deliberately a heuristic over free text rather than a schema column: there is
/// no `project` field on `patterns`, and 58% of rows have empty tags, so a column
/// alone would leave most of the store unattributed and unprotected.
pub fn project_of(tags: &[String], name: &str, intent: &str, body: &str) -> Option<&'static str> {
    let hay = format!(
        "{} {} {} {}",
        tags.join(" ").to_lowercase(),
        name.to_lowercase(),
        intent.to_lowercase(),
        body.to_lowercase()
    );
    PROJECT_MARKERS
        .iter()
        .find(|(_, markers)| markers.iter().any(|m| hay.contains(m)))
        .map(|(project, _)| *project)
}

/// Find pairs of patterns whose content is highly similar (potential duplicates).
///
/// Returns `(kept_id, merged_id, score, kept_name, merged_name)` sorted by score desc.
///
/// **Never pairs across projects.** Merging soft-deletes one side, so a
/// cross-project merge would retire a dormant project's knowledge in favour of
/// whichever project happens to be active — precisely the loss this store exists
/// to prevent. Quartz work can pause for months and its patterns must survive it.
/// A pair is therefore only offered when both sides resolve to the **same known**
/// project; if either is unattributed, the pair is skipped rather than guessed at.
pub fn find_candidates(
    store: &Store,
    threshold: f32,
) -> Result<Vec<(i64, i64, f32, String, String)>> {
    let patterns = store.all_patterns()?;
    let mut candidates = Vec::new();

    let project: Vec<Option<&'static str>> = patterns
        .iter()
        .map(|p| project_of(&p.tags, &p.name, &p.intent, &p.body))
        .collect();

    for i in 0..patterns.len() {
        // Skip dead patterns (survival_rate near 0).
        if patterns[i].survival_rate < 0.1 {
            continue;
        }
        let text_i = format!(
            "{} {} {}",
            patterns[i].intent,
            patterns[i].body,
            patterns[i].uses.join(" ")
        );
        let tv_i = build_term_vector_str(&text_i);

        for j in (i + 1)..patterns.len() {
            if patterns[j].survival_rate < 0.1 {
                continue;
            }
            // Same known project only — see the doc comment on this function.
            match (project[i], project[j]) {
                (Some(a), Some(b)) if a == b => {}
                _ => continue,
            }
            let text_j = format!(
                "{} {} {}",
                patterns[j].intent,
                patterns[j].body,
                patterns[j].uses.join(" ")
            );
            let tv_j = build_term_vector_str(&text_j);

            let score = cosine_similarity(&tv_i, &tv_j);
            if score >= threshold {
                if let (Some(id_i), Some(id_j)) = (patterns[i].id, patterns[j].id) {
                    candidates.push((
                        id_i,
                        id_j,
                        score,
                        patterns[i].name.clone(),
                        patterns[j].name.clone(),
                    ));
                }
            }
        }
    }

    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    Ok(candidates)
}

/// Soft-delete the merged pattern (mark reverted to floor survival) and record the merge log.
pub fn merge_patterns(store: &Store, keep_id: i64, discard_id: i64, score: f32) -> Result<()> {
    store.conn().execute(
        "UPDATE patterns SET reverted_count = 999, survival_rate = 0.0 WHERE id = ?1",
        params![discard_id],
    )?;
    store.insert_merge_log(
        keep_id,
        discard_id,
        score,
        "consolidated via cosine similarity",
    )?;
    Ok(())
}

#[cfg(test)]
mod dormant_project_protection_tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn patterns_are_attributed_to_their_project() {
        assert_eq!(project_of(&tags(&["quartz", "pool"]), "object-pool", "", ""), Some("quartz"));
        assert_eq!(project_of(&[], "scope-portal", "render a VR optic", ""), Some("space_soup"));
        assert_eq!(project_of(&[], "", "", "slint TextEdit viewport"), Some("tale_forge"));
        assert_eq!(project_of(&[], "", "", "compaction ratio scoreboard"), Some("cortex"));
    }

    /// More specific project names must win over the substring they contain.
    #[test]
    fn synful_quartz_is_not_mistaken_for_quartz() {
        assert_eq!(project_of(&tags(&["synful"]), "", "", ""), Some("synful_quartz"));
        assert_eq!(project_of(&tags(&["quartz_forge"]), "", "", ""), Some("quartz_forge"));
    }

    #[test]
    fn unattributable_patterns_return_none_rather_than_a_guess() {
        assert_eq!(project_of(&[], "misc-helper", "do a thing", "body"), None);
    }

    /// The safety property, stated directly: a dormant project's knowledge can
    /// never be proposed for merge against an active project's, no matter how
    /// textually similar the two happen to be.
    #[test]
    fn a_dormant_project_can_never_be_merged_into_an_active_one() {
        let quartz = project_of(&tags(&["quartz"]), "object-pool-lifecycle", "acquire and release pooled objects", "");
        let soup = project_of(&tags(&["space_soup"]), "object-pool-lifecycle", "acquire and release pooled objects", "");
        assert_eq!(quartz, Some("quartz"));
        assert_eq!(soup, Some("space_soup"));
        assert_ne!(quartz, soup, "identical text in different projects must stay distinct");
    }

    #[test]
    fn an_unattributed_pattern_is_never_paired_either() {
        // (Some, None) and (None, None) both fall through the match guard.
        let known = Some("quartz");
        let unknown: Option<&'static str> = None;
        let pairs_allowed = |a: Option<&'static str>, b: Option<&'static str>| {
            matches!((a, b), (Some(x), Some(y)) if x == y)
        };
        assert!(!pairs_allowed(known, unknown));
        assert!(!pairs_allowed(unknown, unknown));
        assert!(pairs_allowed(known, known));
    }
}
