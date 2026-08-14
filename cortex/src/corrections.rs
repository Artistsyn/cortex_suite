//! Learning from the moments the user pushed back — but only once someone checked.
//!
//! This is the highest-value signal in the whole system and the easiest to turn
//! into garbage. A user challenge is not evidence. Measured on this workspace's
//! own transcripts, both directions really happen: the user was right that a
//! reported 4 m hand-gizmo gap was authored on purpose, and the user was right
//! again that a "not realistic" claim about multi-language support was wrong.
//! But the agent was also right on occasions when it was challenged — twice a
//! test assertion written from an assumption failed against correct code and
//! invited a "fix" to working behaviour.
//!
//! Storing the user's side automatically would therefore teach the store things
//! that are false, in the one category an agent is least able to question later.
//! So the rule here is the user's own: record nothing until the disagreement has
//! been **resolved by checking**, record whichever way it resolved, and record
//! nothing at all when it was never settled.
//!
//! Three parts, and each exists because the other two cannot be trusted alone:
//!
//! 1. A `UserPromptSubmit` hook notes that a challenge was raised. This is the
//!    part that cannot be skipped — an agent that has just been corrected is
//!    exactly the agent least likely to volunteer the fact, and the failure mode
//!    is silent.
//! 2. The agent supplies the verdict and, mandatorily, the evidence. `evidence`
//!    is a required parameter rather than a documented expectation, because
//!    measured across 823 calls on this workspace required parameters ran at
//!    100% compliance and documented ones at 2–5%.
//! 3. The result is a **proposal**. Nothing here writes to memory directly.

use anyhow::Result;
use rusqlite::params;

use crate::memory::Store;

/// How a disagreement came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The user was right. What gets proposed is an anti-pattern describing the
    /// way the agent went wrong — not merely the conclusion, which would be
    /// unusable next time.
    UserRight,
    /// The agent was right, and checking confirmed it. Proposed as a note that
    /// strengthens the already-verified thing, citing the challenge as the
    /// evidence it survived. This direction exists because the user asked for
    /// it: a challenge that turns out to be mistaken is still information, and
    /// dropping it means the same question gets re-litigated forever.
    AgentRight,
    /// Checked, and it turned out both were partly right, or the question was
    /// wrong. Recorded as settled so it stops being counted as open — but
    /// nothing is proposed, because there is no single claim to store.
    Mixed,
    /// Never established. Stores NOTHING. Present so an honest agent has
    /// somewhere to put "we moved on without finding out", instead of being
    /// pushed toward inventing a verdict to clear the queue.
    Unresolved,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "user_right" | "user" => Some(Self::UserRight),
            "agent_right" | "agent" | "i_was_right" => Some(Self::AgentRight),
            "mixed" | "both" | "partly" => Some(Self::Mixed),
            "unresolved" | "unknown" | "none" => Some(Self::Unresolved),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserRight => "user_right",
            Self::AgentRight => "agent_right",
            Self::Mixed => "mixed",
            Self::Unresolved => "unresolved",
        }
    }

    /// Does this verdict produce something worth proposing?
    fn proposes(&self) -> bool {
        matches!(self, Self::UserRight | Self::AgentRight)
    }
}

/// Phrases that mark a message as disputing something rather than directing it.
///
/// Deliberately narrow. A false positive costs a human a line in the review
/// queue and, worse, teaches them to skim it; a false negative costs one
/// recorded lesson. The asymmetry says: only match language that is *about a
/// claim already made*, never language that merely asks for work.
///
/// Every entry below is matched against a lowercased prompt.
const CUES: &[(&str, &str)] = &[
    // Direct disputes.
    ("you don't think", "disputes a conclusion"),
    ("you dont think", "disputes a conclusion"),
    ("don't you think", "disputes a conclusion"),
    ("i disagree", "explicit disagreement"),
    ("that's not right", "asserts the claim is wrong"),
    ("thats not right", "asserts the claim is wrong"),
    ("that's not true", "asserts the claim is wrong"),
    ("that's wrong", "asserts the claim is wrong"),
    ("you're wrong", "asserts the claim is wrong"),
    ("youre wrong", "asserts the claim is wrong"),
    ("that is incorrect", "asserts the claim is wrong"),
    // Doubt aimed at a claim.
    ("are you sure", "doubts a claim"),
    ("are you certain", "doubts a claim"),
    ("you sure about", "doubts a claim"),
    ("double check", "asks for verification of a claim"),
    ("check again", "asks for verification of a claim"),
    // Appeals to the agent's own record — the strongest cue in practice, and
    // the one that produced the multi-language reversal on this workspace.
    ("you have told me that before", "cites a past claim that was wrong"),
    ("you told me that before", "cites a past claim that was wrong"),
    ("you said before", "cites a past claim"),
    ("but you said", "cites a past claim"),
    ("you previously said", "cites a past claim"),
    ("last time you said", "cites a past claim"),
    // Confirmation-seeking about a specific stated fact.
    (", correct?", "asks the agent to confirm a stated fact"),
    (" correct?", "asks the agent to confirm a stated fact"),
    ("is that right?", "asks the agent to confirm a stated fact"),
    ("isn't it?", "asks the agent to confirm a stated fact"),
    ("if i'm wrong", "invites the agent to contradict them"),
    ("if im wrong", "invites the agent to contradict them"),
];

/// Does this user message dispute something? Returns the reason it matched.
///
/// Returns `None` for the overwhelming majority of messages, which is the point:
/// this runs on every prompt and must cost nothing and say nothing when there is
/// no disagreement.
pub fn detect(prompt: &str) -> Option<&'static str> {
    let p = prompt.to_lowercase();
    CUES.iter().find(|(cue, _)| p.contains(cue)).map(|(_, why)| *why)
}

/// A short, quotable slice of the prompt — enough for a human to recognise the
/// moment without storing the whole message.
fn excerpt(prompt: &str) -> String {
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 240 {
        return flat;
    }
    let cut: String = flat.chars().take(237).collect();
    format!("{cut}...")
}

/// Record that the hook ran, and whether it matched.
///
/// A mechanism designed to be silent cannot prove it is alive through its own
/// output. `note_challenge` sees every message and writes a row only when a
/// claim is disputed, so an empty `challenges` table means either "nobody
/// argued" or "the hook was never installed" — and those need opposite
/// responses. This is the only thing that tells them apart.
pub fn beat(store: &Store, matched: bool) -> Result<()> {
    store.conn().execute(
        "INSERT INTO hook_heartbeat (hook, fired, matched, last_fired)
         VALUES ('note_challenge', 1, ?1, unixepoch())
         ON CONFLICT(hook) DO UPDATE SET
             fired      = fired + 1,
             matched    = matched + ?1,
             last_fired = unixepoch()",
        params![i64::from(matched)],
    )?;
    Ok(())
}

/// How many times the hook ran, how many times it matched, and when it last ran.
pub fn heartbeat(store: &Store) -> Option<(i64, i64, i64)> {
    store
        .conn()
        .query_row(
            "SELECT fired, matched, last_fired FROM hook_heartbeat WHERE hook = 'note_challenge'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
}

/// Record that a challenge was raised. Idempotent per (session, excerpt).
///
/// Returns the row id when something new was recorded, `None` when the message
/// was not a challenge or was already noted.
pub fn note(store: &Store, session_id: &str, prompt: &str) -> Result<Option<i64>> {
    let Some(cue) = detect(prompt) else { return Ok(None) };
    let ex = excerpt(prompt);

    let already: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM challenges WHERE session_id = ?1 AND excerpt = ?2",
        params![session_id, ex],
        |r| r.get(0),
    )?;
    if already > 0 {
        return Ok(None);
    }

    store.conn().execute(
        "INSERT INTO challenges (session_id, cue, excerpt) VALUES (?1, ?2, ?3)",
        params![session_id, cue, ex],
    )?;
    Ok(Some(store.conn().last_insert_rowid()))
}

pub struct OpenChallenge {
    pub id: i64,
    pub cue: String,
    pub excerpt: String,
}

/// Challenges raised in this session that nobody has settled.
pub fn open(store: &Store, session_id: &str) -> Result<Vec<OpenChallenge>> {
    let mut st = store.conn().prepare(
        "SELECT id, cue, excerpt FROM challenges
         WHERE session_id = ?1 AND verdict IS NULL ORDER BY id",
    )?;
    let rows = st.query_map(params![session_id], |r| {
        Ok(OpenChallenge { id: r.get(0)?, cue: r.get(1)?, excerpt: r.get(2)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Settle a challenge, and propose what it taught — if anything.
///
/// `evidence` is what was actually checked: a command that was run, a file and
/// line that was read, an observed behaviour. It is required and must be
/// non-trivial, because a verdict without it is just the agent's memory of the
/// argument, which is the one witness with a stake in the outcome.
pub fn resolve(
    store: &Store,
    id: i64,
    verdict: Verdict,
    subject: &str,
    evidence: &str,
) -> Result<String> {
    let ev = evidence.trim();
    if verdict.proposes() && ev.chars().count() < 20 {
        anyhow::bail!(
            "a verdict of `{}` needs evidence of what was actually checked \
             (a command run, a file:line read, an observed behaviour). \
             Got {} characters. If nothing was checked, the verdict is `unresolved`.",
            verdict.as_str(),
            ev.chars().count()
        );
    }

    let row: rusqlite::Result<(String, String)> = store.conn().query_row(
        "SELECT excerpt, COALESCE(verdict, '') FROM challenges WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let (excerpt, existing) = match row {
        Ok(v) => v,
        Err(_) => anyhow::bail!("no challenge with id {id}"),
    };
    if !existing.is_empty() {
        anyhow::bail!("challenge {id} was already settled as `{existing}`");
    }

    store.conn().execute(
        "UPDATE challenges SET verdict = ?2, subject = ?3, evidence = ?4,
                               resolved_at = unixepoch() WHERE id = ?1",
        params![id, verdict.as_str(), subject, ev],
    )?;

    if !verdict.proposes() {
        return Ok(format!(
            "challenge {id} settled as `{}` — nothing proposed, which is correct: \
             an unsettled disagreement is not knowledge.",
            verdict.as_str()
        ));
    }

    let (kind, text) = match verdict {
        Verdict::UserRight => (
            "anti_pattern",
            format!(
                "{subject}\n\nThe agent claimed otherwise and was corrected. \
                 What was checked: {ev}\n\nThe user's challenge: \"{excerpt}\""
            ),
        ),
        Verdict::AgentRight => (
            "pref_note",
            format!(
                "{subject}\n\nThis was challenged and held up under checking, \
                 so it is more trustworthy than an unchallenged claim, not less. \
                 What was checked: {ev}\n\nThe challenge it survived: \"{excerpt}\""
            ),
        ),
        _ => unreachable!("guarded by proposes()"),
    };

    let hash = format!("challenge-{id}-{}", verdict.as_str());
    let evidence_json = serde_json::json!({
        "source": "user_correction",
        "challenge_id": id,
        "verdict": verdict.as_str(),
        "checked": ev,
        "prompt_excerpt": excerpt,
    })
    .to_string();

    store.conn().execute(
        "INSERT OR IGNORE INTO proposals
             (proposal_type, content_hash, target_file, proposed_text, evidence, status)
         VALUES (?1, ?2, 'user-correction', ?3, ?4, 'pending')",
        params![kind, hash, text, evidence_json],
    )?;

    Ok(format!(
        "challenge {id} settled as `{}` — raised a `{kind}` proposal for review. \
         It is NOT in memory yet; `cortex review-proposals` decides that.",
        verdict.as_str()
    ))
}

/// Open challenges across the whole store, for the review queue.
///
/// A challenge left open is not a failure to report — sometimes a conversation
/// genuinely moves on. But an agent that never settles any of them is an agent
/// quietly dropping every correction it receives, and that is worth seeing.
pub fn open_count(store: &Store) -> Result<i64> {
    Ok(store.conn().query_row(
        "SELECT COUNT(*) FROM challenges WHERE verdict IS NULL",
        [],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "cortex-corr-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Store::open(&p).unwrap(), p)
    }

    // ── detection ──────────────────────────────────────────────────────────
    //
    // The fixtures below are REAL messages from this workspace's transcripts,
    // both kinds, so the filter is tuned against what people actually type here
    // rather than against invented examples that flatter it.

    #[test]
    fn real_challenges_are_detected() {
        for msg in [
            "you don't think it's just from where I keep moving the hand out from the grab point to test the finger posing without obstruction?",
            "do more research because you have told me that before about other things and when I asked you to make sure and deep research it more, you ended up changing your answer",
            "and we have individual part pulls restricted to weapons that are being held, correct?",
            "The user corrections idea is a definite (after you confirm that I'm right at least, if I'm wrong then incorrect corrections should not be stored)",
            "are you sure that hook is actually firing?",
        ] {
            assert!(detect(msg).is_some(), "missed a real challenge: {msg}");
        }
    }

    #[test]
    fn ordinary_instructions_are_not_challenges() {
        // Every one of these is a real instruction from this workspace. If any
        // of them trips the filter, the review queue fills with noise and stops
        // being read — which is the failure mode that kills the whole feature.
        for msg in [
            "commit and push it",
            "implement it, start with the failing dpr 2 test",
            "fix it all please",
            "yes",
            "go on to the thumb rotation control",
            "reject workflow-general",
            "do all of them",
            "build test-outcome scoring",
            "correct the spelling in that comment",
            "yes, do both and drive it in your own window",
        ] {
            assert!(detect(msg).is_none(), "false positive on: {msg}");
        }
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert!(detect("ARE YOU SURE about that?").is_some());
    }

    // ── recording ──────────────────────────────────────────────────────────

    #[test]
    fn a_challenge_is_recorded_once_per_session() {
        let (s, p) = store();
        let msg = "are you sure that is what the field does?";
        assert!(note(&s, "sess", msg).unwrap().is_some());
        assert!(note(&s, "sess", msg).unwrap().is_none(), "duplicated a challenge");
        assert_eq!(open(&s, "sess").unwrap().len(), 1);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_plain_instruction_records_nothing() {
        let (s, p) = store();
        assert!(note(&s, "sess", "commit and push it").unwrap().is_none());
        assert_eq!(open(&s, "sess").unwrap().len(), 0);
        let _ = std::fs::remove_file(p);
    }

    // ── the gate ───────────────────────────────────────────────────────────

    #[test]
    fn a_verdict_without_evidence_is_refused() {
        let (s, p) = store();
        let id = note(&s, "sess", "are you sure?").unwrap().unwrap();
        let err = resolve(&s, id, Verdict::UserRight, "subject", "yeah").unwrap_err();
        assert!(err.to_string().contains("evidence"), "{err}");
        // And it must stay open — a refused resolve that silently marked the
        // row settled would lose the challenge entirely.
        assert_eq!(open(&s, "sess").unwrap().len(), 1);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn unresolved_settles_the_row_and_proposes_nothing() {
        let (s, p) = store();
        let id = note(&s, "sess", "are you sure?").unwrap().unwrap();
        // No evidence required — this is the honest exit, so it must not be
        // harder to reach than inventing a verdict.
        resolve(&s, id, Verdict::Unresolved, "n/a", "").unwrap();
        assert_eq!(open(&s, "sess").unwrap().len(), 0);
        let n: i64 = s.conn()
            .query_row("SELECT COUNT(*) FROM proposals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "an unsettled disagreement must not reach the store");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn user_right_proposes_an_anti_pattern_and_does_not_commit_it() {
        let (s, p) = store();
        let id = note(&s, "sess", "you don't think it's the offset field?").unwrap().unwrap();
        let out = resolve(
            &s, id, Verdict::UserRight,
            "hand_offset_pos being None is authored intent, not a defect",
            "read GripPointDef in grab_detect.rs:88; both m4a1 grips leave hand_offset_pos None",
        ).unwrap();
        assert!(out.contains("anti_pattern"), "{out}");
        assert!(out.contains("NOT in memory yet"), "must not claim it was stored: {out}");

        let (kind, status): (String, String) = s.conn()
            .query_row("SELECT proposal_type, status FROM proposals", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kind, "anti_pattern");
        assert_eq!(status, "pending", "nothing here may bypass human review");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn agent_right_strengthens_rather_than_recording_the_users_claim() {
        // The user's own condition: being challenged and holding up is evidence
        // FOR the verified thing. The mistaken challenge must never be stored as
        // if it were the finding.
        let (s, p) = store();
        let id = note(&s, "sess", "are you sure the chord is spread-independent?").unwrap().unwrap();
        resolve(
            &s, id, Verdict::AgentRight,
            "the raw fingertip chord is already spread-independent",
            "reverted the 'fix' and ran cargo test: `reads a sideways drag as spread` goes red with it, green without",
        ).unwrap();
        let (kind, text): (String, String) = s.conn()
            .query_row("SELECT proposal_type, proposed_text FROM proposals", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kind, "pref_note", "must not be filed as a trap the agent fell into");
        assert!(text.contains("held up under checking"), "{text}");
        assert!(text.contains("spread-independent"), "the verified claim must be the subject: {text}");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_settled_challenge_cannot_be_settled_again() {
        let (s, p) = store();
        let id = note(&s, "sess", "are you sure?").unwrap().unwrap();
        resolve(&s, id, Verdict::Unresolved, "x", "").unwrap();
        assert!(resolve(&s, id, Verdict::UserRight, "x", "checked the thing thoroughly").is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn verdict_parsing_accepts_the_obvious_spellings_and_rejects_junk() {
        assert_eq!(Verdict::parse("user_right"), Some(Verdict::UserRight));
        assert_eq!(Verdict::parse("agent-right"), Some(Verdict::AgentRight));
        assert_eq!(Verdict::parse("UNRESOLVED"), Some(Verdict::Unresolved));
        assert_eq!(Verdict::parse("probably"), None);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "cortex-hb-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Store::open(&p).unwrap(), p)
    }

    /// The distinction the audit could not make before: a hook that runs and
    /// correctly finds nothing looks identical to a hook that was never
    /// installed. Both leave `challenges` empty.
    #[test]
    fn the_hook_proves_it_ran_even_when_it_matched_nothing() {
        let (s, p) = store();
        assert!(heartbeat(&s).is_none(), "no beat before the hook ever runs");

        beat(&s, false).unwrap();
        beat(&s, false).unwrap();
        let (fired, matched, _) = heartbeat(&s).expect("hook ran but left no trace");
        assert_eq!(fired, 2, "the silent path must still beat");
        assert_eq!(matched, 0);

        beat(&s, true).unwrap();
        let (fired, matched, _) = heartbeat(&s).unwrap();
        assert_eq!((fired, matched), (3, 1));
        let _ = std::fs::remove_file(p);
    }
}
