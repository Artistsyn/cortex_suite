//! Capture knowledge markers when they are written (docs/self-learning-loop-2026-09-30.md, L1).
//!
//! Knowledge used to reach the store only through `markers_text` at a closeout
//! the user approved. Measured 2026-09-30 over the local transcripts: 123 of 411
//! markers written in chat never arrived, although the user had approved 138 of
//! the 140 raw misses. 98 were lost because a context compaction came between
//! writing and closeout, and the summary the agent then worked from no longer
//! carried them. The transcript on disk always has them.
//!
//! So the Stop and PreCompact hooks hand this module the transcript path. It
//! reads the assistant's own text from where it last stopped (complete lines
//! only), skips fenced code and placeholder examples, and commits each marker
//! through the same gates as closeout (`closeout::commit_one`). While the
//! approval gate is on (`loop.auto_commit` = 0) markers are staged instead, and
//! a closeout with inline_approve releases them.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::closeout::{self, CommitOutcome};
use crate::loop_ledger;
use crate::markers::{self, KnowledgeMarker};
use crate::memory::Store;

/// Tails of other transcripts older than this are left alone by the sweep.
const SWEEP_MAX_AGE_DAYS: u64 = 14;

/// One block of assistant text, and when it was written.
pub struct Written {
    pub text: String,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Assistant text blocks in the complete lines after `offset`, and the offset
/// just past the last complete line. A file shorter than `offset` was replaced,
/// so it is read from the start.
pub fn assistant_text_since(path: &Path, offset: u64) -> Result<(Vec<Written>, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = if offset > len { 0 } else { offset };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok((Vec::new(), start));
    };
    let mut texts = Vec::new();
    for line in buf[..last_nl].split(|&b| b == b'\n') {
        // Only lines that can hold a marker are parsed: a first capture of a
        // long transcript reads tens of megabytes, almost none of it relevant.
        if line.is_empty() || !line.windows(7).any(|w| w == b"CORTEX-") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant")
            || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        for block in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    texts.push(Written { text: t.to_string(), at });
                }
            }
        }
    }
    Ok((texts, start + last_nl as u64 + 1))
}

/// The text with fenced code blocks removed: markers shown inside ``` fences
/// are examples of the syntax, not knowledge.
pub fn strip_fenced_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The field a marker is known by.
pub fn marker_key(m: &KnowledgeMarker) -> String {
    match m {
        KnowledgeMarker::Pattern { name, .. } => name.clone(),
        KnowledgeMarker::AntiPattern { description, .. } => description.clone(),
        KnowledgeMarker::Correction { attempted, .. } => attempted.clone(),
        KnowledgeMarker::Adr { title, .. } => title.clone(),
        KnowledgeMarker::PrefsNote { body, .. } => body.clone(),
        KnowledgeMarker::SkillCandidate { name, .. } => name.clone(),
        KnowledgeMarker::Wall(w) => w.claim.clone(),
    }
}

/// `...`, `<description>` and empty keys are the syntax being explained.
pub fn is_placeholder(m: &KnowledgeMarker) -> bool {
    let k = marker_key(m);
    let k = k.trim();
    k.is_empty() || k.starts_with("...") || (k.starts_with('<') && k.ends_with('>'))
}

/// The markers in one block of assistant text, with their source text.
pub fn markers_in(text: &str) -> Vec<(KnowledgeMarker, String)> {
    markers::parse_markers_with_raw(&strip_fenced_code(text))
        .into_iter()
        .filter(|(m, _)| !is_placeholder(m))
        .collect()
}

/// The store key capture files a transcript's markers under.
pub fn session_key_for(transcript: &Path) -> String {
    let stem = transcript.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("cc:{stem}")
}

/// A path the agent-callable tool may read: a Claude Code transcript, or one
/// under `CORTEX_TRANSCRIPTS_DIR`. The CLI, run by a person, reads any file.
pub fn is_transcript_path(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    if let Some(dir) = std::env::var_os("CORTEX_TRANSCRIPTS_DIR") {
        if path.starts_with(PathBuf::from(dir)) {
            return true;
        }
    }
    path.to_string_lossy().contains("/.claude/projects/")
}

#[derive(Debug, Default)]
pub struct Captured {
    pub transcript: PathBuf,
    pub bytes_read: u64,
    pub markers: usize,
    pub committed: Vec<String>,
    pub known: usize,
    pub staged: usize,
    pub merged: Vec<String>,
    pub refused: Vec<String>,
}

impl Captured {
    /// One line for logs; empty when nothing was found.
    pub fn summary(&self) -> String {
        if self.markers == 0 {
            return String::new();
        }
        let stem = self.transcript.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut s = format!(
            "captured {} marker(s) from {stem}: {} committed, {} already known, {} staged, {} refused",
            self.markers,
            self.committed.len(),
            self.known,
            self.staged,
            self.refused.len()
        );
        for m in &self.merged {
            s.push_str(&format!("\n  merged: {m}"));
        }
        for r in &self.refused {
            s.push_str(&format!("\n  refused: {r}"));
        }
        s
    }
}

/// Capture everything written since the last capture of this transcript.
pub fn capture(store: &Store, transcript: &Path, prefs_path: Option<&Path>) -> Result<Captured> {
    let offset = loop_ledger::capture_offset(store, transcript);
    let (texts, new_offset) = assistant_text_since(transcript, offset)?;
    let session_key = session_key_for(transcript);
    let evidence = format!("{}:{offset}", transcript.display());
    let auto = loop_ledger::auto_commit_enabled(store);
    let mut out = Captured {
        transcript: transcript.to_path_buf(),
        bytes_read: new_offset.saturating_sub(offset),
        ..Default::default()
    };
    for w in &texts {
        for (marker, raw) in markers_in(&w.text) {
            out.markers += 1;
            apply(store, &session_key, &marker, &raw, prefs_path, auto, "entry", &evidence, w.at, &mut out);
        }
    }
    loop_ledger::set_capture_offset(store, transcript, new_offset)?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn apply(
    store: &Store,
    session_key: &str,
    marker: &KnowledgeMarker,
    raw: &str,
    prefs_path: Option<&Path>,
    auto: bool,
    class: &str,
    evidence: &str,
    written_at: Option<chrono::DateTime<chrono::Utc>>,
    out: &mut Captured,
) {
    if !auto {
        if closeout::already_committed(store, marker) {
            out.known += 1;
        } else {
            let _ = closeout::record_marker_raw(store, session_key, marker, false, raw);
            out.staged += 1;
        }
        return;
    }
    match closeout::commit_one(store, session_key, marker, raw, prefs_path, class, evidence, true, written_at) {
        CommitOutcome::Committed { target, merged } => {
            out.committed.push(target);
            out.merged.extend(merged);
        }
        CommitOutcome::Updated { target, replaced } => {
            out.merged.push(format!("{target} replaces {replaced} (same name, new text)"));
            out.committed.push(target);
        }
        CommitOutcome::Known => out.known += 1,
        CommitOutcome::Refused(e) => {
            out.refused.push(format!("{} \"{}\": {e}", marker.marker_type(), marker.display_name()))
        }
    }
}

/// The unread tails of other transcripts capture has seen before: the last
/// message of a session that ended, or anything written after its last Stop.
pub fn sweep_tails(store: &Store, prefs_path: Option<&Path>, exclude: &Path) -> Vec<Captured> {
    let now = std::time::SystemTime::now();
    let mut done = Vec::new();
    for (path, offset) in loop_ledger::known_transcripts(store) {
        if path == exclude {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let recent = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() < SWEEP_MAX_AGE_DAYS * 86_400);
        if meta.len() <= offset || !recent {
            continue;
        }
        if let Ok(c) = capture(store, &path, prefs_path) {
            done.push(c);
        }
    }
    done
}

// ── backfill and coverage ────────────────────────────────────────────────────

fn norm_key(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().chars().take(40).collect()
}

/// Everything the store already holds, by the first 40 characters of the
/// field each kind is known by -- loose on purpose: a lesson stored under a
/// slightly different ending is still stored.
pub struct KnownKeys(HashSet<String>);

impl KnownKeys {
    pub fn load(store: &Store, prefs_path: Option<&Path>) -> Self {
        let mut keys = HashSet::new();
        for sql in [
            "SELECT description FROM anti_patterns",
            "SELECT name FROM patterns",
            "SELECT attempted FROM self_corrections",
            "SELECT title FROM adrs",
            "SELECT claim FROM walls",
            "SELECT name FROM knowledge_markers WHERE promoted = 1",
            "SELECT body FROM annotations",
        ] {
            if let Ok(mut stmt) = store.conn().prepare(sql) {
                if let Ok(rows) = stmt.query_map([], |r| r.get::<_, Option<String>>(0)) {
                    keys.extend(rows.filter_map(|r| r.ok().flatten()).map(|v| norm_key(&v)));
                }
            }
        }
        if let Some(prefs) = prefs_path.and_then(|p| crate::prefs::load(p).ok()) {
            keys.extend(prefs.project.notes.iter().map(|n| norm_key(n)));
        }
        Self(keys)
    }

    pub fn knows(&self, m: &KnowledgeMarker) -> bool {
        self.0.contains(&norm_key(&marker_key(m)))
    }
}

/// `*.jsonl` directly under `dir`, oldest first.
pub fn transcripts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    files.sort();
    Ok(files.into_iter().map(|(_, p)| p).collect())
}

pub struct Candidate {
    pub transcript: PathBuf,
    pub marker: KnowledgeMarker,
    pub raw: String,
    pub written_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Markers written in any transcript under `dir` that the store does not hold,
/// each once, and how many markers were seen in all.
pub fn backfill_candidates(store: &Store, dir: &Path, prefs_path: Option<&Path>) -> Result<(Vec<Candidate>, usize)> {
    let known = KnownKeys::load(store, prefs_path);
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    let (mut out, mut total) = (Vec::new(), 0usize);
    for path in transcripts(dir)? {
        let (texts, _) = assistant_text_since(&path, 0)?;
        for w in &texts {
            for (marker, raw) in markers_in(&w.text) {
                total += 1;
                if !seen.insert((marker.marker_type(), norm_key(&marker_key(&marker)))) || known.knows(&marker) {
                    continue;
                }
                out.push(Candidate { transcript: path.clone(), marker, raw, written_at: w.at });
            }
        }
    }
    Ok((out, total))
}

fn tagged_backfill(m: &KnowledgeMarker) -> KnowledgeMarker {
    let mut m = m.clone();
    match &mut m {
        KnowledgeMarker::Pattern { tags, .. }
        | KnowledgeMarker::AntiPattern { tags, .. }
        | KnowledgeMarker::Correction { tags, .. }
        | KnowledgeMarker::Adr { tags, .. }
        | KnowledgeMarker::PrefsNote { tags, .. } => {
            if !tags.iter().any(|t| t == "backfill") {
                tags.push("backfill".into());
            }
        }
        KnowledgeMarker::SkillCandidate { .. } | KnowledgeMarker::Wall(_) => {}
    }
    m
}

/// Commit backfill candidates through the same gates, tagged `backfill` and
/// recorded as `backfill` changes so they can be audited as a group.
pub fn backfill_write(store: &Store, candidates: &[Candidate], prefs_path: Option<&Path>) -> Captured {
    let mut out = Captured::default();
    for c in candidates {
        out.markers += 1;
        let marker = tagged_backfill(&c.marker);
        let session = format!("backfill:{}", session_key_for(&c.transcript));
        let evidence = c.transcript.display().to_string();
        apply(store, &session, &marker, &c.raw, prefs_path, true, "backfill", &evidence, c.written_at, &mut out);
    }
    out
}

pub struct Coverage {
    pub transcripts: usize,
    pub written: usize,
    pub stored: usize,
    pub missing: Vec<String>,
}

/// Markers written in transcripts modified since `since` (unix seconds), and
/// how many of them the store holds: the check that capture is working, made
/// from the other side of the boundary.
pub fn coverage(store: &Store, dir: &Path, since: i64, prefs_path: Option<&Path>) -> Result<Coverage> {
    let known = KnownKeys::load(store, prefs_path);
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    let mut cov = Coverage { transcripts: 0, written: 0, stored: 0, missing: Vec::new() };
    for path in transcripts(dir)? {
        let modified = std::fs::metadata(&path)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if modified < since {
            continue;
        }
        cov.transcripts += 1;
        let (texts, _) = assistant_text_since(&path, 0)?;
        for w in &texts {
            for (marker, _) in markers_in(&w.text) {
                if !seen.insert((marker.marker_type(), norm_key(&marker_key(&marker)))) {
                    continue;
                }
                cov.written += 1;
                if known.knows(&marker) {
                    cov.stored += 1;
                } else {
                    cov.missing.push(format!("{}: {}", marker.marker_type(), marker.display_name()));
                }
            }
        }
    }
    Ok(cov)
}

/// A path the MCP tool was handed, checked before anything reads it.
pub fn checked_transcript(raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("missing `transcript_path`");
    }
    if raw.starts_with("${") {
        bail!(
            "capture_markers received an uninterpolated template ({raw}). The hook is installed \
             but its input variable is wrong, so no marker will ever be captured. Fix the `input` \
             mapping in .claude/settings.local.json (`cortex hooks-init --force`)."
        );
    }
    let path = PathBuf::from(raw);
    if !is_transcript_path(&path) {
        bail!("not a transcript: {raw} (expected a .jsonl under ~/.claude/projects/ or CORTEX_TRANSCRIPTS_DIR)");
    }
    if !path.exists() {
        bail!("no transcript at {raw}");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> crate::test_support::TempStore {
        crate::test_support::TempStore::new("capture").unwrap()
    }

    fn line(kind: &str, text: &str) -> String {
        line_at(kind, text, &chrono::Utc::now().to_rfc3339())
    }

    fn line_at(kind: &str, text: &str, at: &str) -> String {
        json!({"type": kind, "timestamp": at, "message": {"content": [{"type": "text", "text": text}]}}).to_string()
    }

    const AP: &str = "[CORTEX-AP: description=\"a captured trap about widgets\" tags=\"widgets,test\"]\nwrong: poke the widget\ncorrect: ask the widget\n[/CORTEX-AP]";

    fn transcript(name: &str, lines: &[String]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cortex-capture-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}.jsonl"));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    fn ap_count(s: &Store, description: &str) -> i64 {
        s.conn()
            .query_row("SELECT COUNT(*) FROM anti_patterns WHERE description = ?1 AND superseded_by IS NULL", [description], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn only_the_assistants_own_text_outside_code_fences_is_read() {
        let example = format!("Here is the syntax:\n```\n{AP}\n```\n");
        let placeholder = "[CORTEX-AP: description=\"...\" tags=\"...\"]wrong: ...\ncorrect: ...[/CORTEX-AP]";
        let lines = vec![
            line("user", AP),
            line("assistant", &example),
            line("assistant", placeholder),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": AP}]}}).to_string(),
            line("assistant", &format!("Learned this:\n{AP}")),
        ];
        let p = transcript("own-text", &lines);
        let (texts, _) = assistant_text_since(&p, 0).unwrap();
        let found: Vec<_> = texts.iter().flat_map(|w| markers_in(&w.text)).collect();
        assert_eq!(found.len(), 1, "only the real marker in the assistant's own text");
        assert_eq!(marker_key(&found[0].0), "a captured trap about widgets");
    }

    #[test]
    fn capture_commits_once_and_resumes_from_its_offset() {
        let s = store();
        let p = transcript("resume", &[line("assistant", AP)]);
        let first = capture(&s, &p, None).unwrap();
        assert_eq!((first.markers, first.committed.len()), (1, 1), "{}", first.summary());
        assert_eq!(ap_count(&s, "a captured trap about widgets"), 1);
        let change = loop_ledger::recent(&s, 1).unwrap().remove(0);
        assert_eq!(change.class, "entry");
        assert!(change.target.starts_with("anti_patterns:"), "{}", change.target);

        // Nothing new: nothing read, nothing committed.
        let again = capture(&s, &p, None).unwrap();
        assert_eq!((again.markers, again.bytes_read), (0, 0));

        // A later message repeating the marker is known, not committed twice.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        use std::io::Write;
        writeln!(f, "{}", line("assistant", AP)).unwrap();
        let third = capture(&s, &p, None).unwrap();
        assert_eq!((third.markers, third.known, third.committed.len()), (1, 1, 0));
        assert_eq!(ap_count(&s, "a captured trap about widgets"), 1);
    }

    #[test]
    fn a_partial_last_line_is_left_for_the_next_capture() {
        let s = store();
        let full = line("assistant", AP);
        let p = transcript("partial", &[]);
        std::fs::write(&p, &full[..full.len() / 2]).unwrap();
        let first = capture(&s, &p, None).unwrap();
        assert_eq!(first.markers, 0);
        std::fs::write(&p, format!("{full}\n")).unwrap();
        let second = capture(&s, &p, None).unwrap();
        assert_eq!(second.committed.len(), 1, "{}", second.summary());
    }

    #[test]
    fn with_the_gate_on_markers_are_staged_not_committed() {
        let s = store();
        loop_ledger::set_auto_commit(&s, false, "test").unwrap();
        let p = transcript("gated", &[line("assistant", AP)]);
        let c = capture(&s, &p, None).unwrap();
        assert_eq!((c.staged, c.committed.len()), (1, 0));
        assert_eq!(ap_count(&s, "a captured trap about widgets"), 0);
        let raw: String = s
            .conn()
            .query_row("SELECT raw_tag FROM knowledge_markers WHERE promoted = 0", [], |r| r.get(0))
            .unwrap();
        assert!(raw.contains("[/CORTEX-AP]"), "staged markers keep their source text: {raw}");
    }

    #[test]
    fn a_restatement_supersedes_the_older_entry() {
        let s = store();
        // TF-IDF needs a corpus: in a store of two entries every shared word is
        // in every document and weighs nothing. The 0.9 threshold was
        // calibrated on 744 real entries, so give the test unrelated ones.
        for i in 0..25 {
            s.conn()
                .execute(
                    "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at) VALUES (?1, 'w', 'c', '[]', ?2)",
                    rusqlite::params![
                        format!("unrelated trap {i} about subsystem_{i} and its module_{i} handler_{i}"),
                        chrono::Utc::now().to_rfc3339()
                    ],
                )
                .unwrap();
        }
        let older = "[CORTEX-AP: description=\"design_canvas pan zoom mutates scene canvas after closures capture an immutable borrow\" tags=\"canvas\"]\nwrong: mutate scene.canvas pan_x pan_y after the render closures borrow it\ncorrect: apply pan and zoom first, then build the closures\n[/CORTEX-AP]";
        let newer = older.replace("design_canvas pan zoom", "design_canvas pan and zoom");
        let p = transcript("dup", &[line("assistant", older), line("assistant", &newer)]);
        let c = capture(&s, &p, None).unwrap();
        assert_eq!(c.committed.len(), 2, "{}", c.summary());
        assert_eq!(c.merged.len(), 1, "{}", c.summary());
        let live: i64 = s.conn().query_row("SELECT COUNT(*) FROM anti_patterns WHERE superseded_by IS NULL AND description LIKE 'design_canvas%'", [], |r| r.get(0)).unwrap();
        assert_eq!(live, 1, "the older restatement is superseded, not deleted");
        let kept: i64 = s.conn().query_row("SELECT COUNT(*) FROM anti_patterns WHERE description LIKE 'design_canvas%'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 2);
        assert!(loop_ledger::recent(&s, 10).unwrap().iter().any(|c| c.class == "duplicate"));
    }

    #[test]
    fn a_same_name_pattern_with_new_text_replaces_the_old_version() {
        let s = store();
        let v1 = "[CORTEX-PATTERN: name=\"widget-polishing\" intent=\"Polish widgets\" tags=\"w\" trust=\"verified\"]Rub them.[/CORTEX-PATTERN]";
        let v2 = "[CORTEX-PATTERN: name=\"widget-polishing\" intent=\"Polish widgets\" tags=\"w\" trust=\"verified\"]Rub them, then buff with wax.[/CORTEX-PATTERN]";
        let p = transcript("pattern-update", &[line("assistant", v1), line("assistant", v1), line("assistant", v2)]);
        let c = capture(&s, &p, None).unwrap();
        assert_eq!((c.markers, c.known), (3, 1), "{}", c.summary());
        assert!(c.merged.iter().any(|m| m.contains("same name")), "{}", c.summary());
        let live: Vec<String> = s
            .conn()
            .prepare("SELECT body FROM patterns WHERE name = 'widget-polishing' AND superseded_by IS NULL")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(live.len(), 1);
        assert!(live[0].contains("wax"), "{live:?}");
    }

    /// The bug a first capture on a copy of the live store exposed: a session's
    /// 03:43 draft, still in its transcript, replaced the version committed at
    /// 04:40. An older draft must never replace a newer entry.
    #[test]
    fn an_older_draft_never_replaces_a_newer_entry() {
        let s = store();
        for i in 0..25 {
            s.conn()
                .execute(
                    "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at) VALUES (?1, 'w', 'c', '[]', ?2)",
                    rusqlite::params![format!("unrelated trap {i} about subsystem_{i} and module_{i}"), chrono::Utc::now().to_rfc3339()],
                )
                .unwrap();
        }
        let newer_pattern = "[CORTEX-PATTERN: name=\"cue-replay\" intent=\"Pick cues\" tags=\"t\" trust=\"verified\"]26 of 2,195.[/CORTEX-PATTERN]";
        let older_pattern = "[CORTEX-PATTERN: name=\"cue-replay\" intent=\"Pick cues\" tags=\"t\" trust=\"verified\"]23 of 2,194.[/CORTEX-PATTERN]";
        let newer_ap = "[CORTEX-AP: description=\"a benchmark with only indoor views hid a 10-13 ms terrain cost\" tags=\"bench\"]\nwrong: bench indoor views only terrain_cost hidden\ncorrect: add outdoor views to the benchmark terrain_cost\n[/CORTEX-AP]";
        let older_ap = newer_ap.replace("10-13", "10\u{2013}13");

        // The newer versions are committed first (as a closeout would), at 04:40.
        let committed = transcript("newer", &[line_at("assistant", newer_pattern, "2026-09-30T04:40:00Z"), line_at("assistant", newer_ap, "2026-09-30T04:40:01Z")]);
        let c = capture(&s, &committed, None).unwrap();
        assert_eq!(c.committed.len(), 2, "{}", c.summary());

        // Then a transcript still holding the 03:43 drafts is captured.
        let drafts = transcript("drafts", &[line_at("assistant", older_pattern, "2026-09-30T03:43:00Z"), line_at("assistant", &older_ap, "2026-09-30T03:43:01Z")]);
        let d = capture(&s, &drafts, None).unwrap();
        assert_eq!((d.markers, d.known, d.committed.len()), (2, 2, 0), "{}", d.summary());
        assert!(d.merged.is_empty(), "{}", d.summary());
        let body: String = s
            .conn()
            .query_row("SELECT body FROM patterns WHERE name = 'cue-replay' AND superseded_by IS NULL", [], |r| r.get(0))
            .unwrap();
        assert!(body.contains("26 of 2,195"), "the newer version must stay live: {body}");
        let live_ap: String = s
            .conn()
            .query_row("SELECT description FROM anti_patterns WHERE description LIKE 'a benchmark with only indoor%' AND superseded_by IS NULL", [], |r| r.get(0))
            .unwrap();
        assert!(live_ap.contains("10-13"), "{live_ap}");

        // A genuinely newer version still replaces.
        let newest = transcript("newest", &[line_at("assistant", &newer_pattern.replace("26 of 2,195.", "27 of 2,301."), "2026-10-01T09:00:00Z")]);
        let n = capture(&s, &newest, None).unwrap();
        assert!(n.merged.iter().any(|m| m.contains("same name")), "{}", n.summary());
    }

    #[test]
    fn corrections_and_adrs_are_not_duplicated_by_a_replay() {
        let s = store();
        let corr1 = "[CORTEX-CORRECTION: attempted=\"ran the widget migration twice\" reason=\"the second run doubled rows\" fix=\"guard it\"][/CORTEX-CORRECTION]";
        let corr2 = "[CORTEX-CORRECTION: attempted=\"Ran the widget migration twice \" reason=\"rows doubled on the second run\" fix=\"guard it with a version\"][/CORTEX-CORRECTION]";
        let adr = "[CORTEX-ADR: title=\"Widgets live in one table\" tags=\"w\"]Context: two tables drifted. Decision: one table.[/CORTEX-ADR]";
        let p = transcript("corr-adr", &[line("assistant", corr1), line("assistant", adr)]);
        let c = capture(&s, &p, None).unwrap();
        assert_eq!(c.committed.len(), 2, "{}", c.summary());
        let q = transcript("corr-adr-again", &[line("assistant", corr2), line("assistant", &adr.replace("one table.", "one table, as decided."))]);
        let d = capture(&s, &q, None).unwrap();
        assert_eq!((d.committed.len(), d.known), (0, 2), "{}", d.summary());
        let count = |sql: &str| -> i64 { s.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM self_corrections"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM adrs"), 1);
    }

    #[test]
    fn backfill_finds_only_what_the_store_lacks_and_tags_it() {
        let s = store();
        let dir = std::env::temp_dir().join(format!("cortex-backfill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = AP.replace("a captured trap about widgets", "a lost trap about gadgets");
        std::fs::write(dir.join("a.jsonl"), line("assistant", AP) + "\n").unwrap();
        std::fs::write(dir.join("b.jsonl"), format!("{}\n{}\n", line("assistant", &other), line("assistant", &other))).unwrap();
        capture(&s, &dir.join("a.jsonl"), None).unwrap();

        let (cands, total) = backfill_candidates(&s, &dir, None).unwrap();
        assert_eq!(total, 3);
        assert_eq!(cands.len(), 1, "the stored one and the repeat are skipped");
        let out = backfill_write(&s, &cands, None);
        assert_eq!(out.committed.len(), 1, "{}", out.summary());
        let tags: String = s
            .conn()
            .query_row("SELECT tags FROM anti_patterns WHERE description = 'a lost trap about gadgets'", [], |r| r.get(0))
            .unwrap();
        assert!(tags.contains("backfill"), "{tags}");
        assert!(loop_ledger::recent(&s, 5).unwrap().iter().any(|c| c.class == "backfill"));

        let cov = coverage(&s, &dir, 0, None).unwrap();
        assert_eq!((cov.written, cov.stored), (2, 2), "{:?}", cov.missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tool_only_reads_transcripts() {
        assert!(checked_transcript("${transcript_path}").unwrap_err().to_string().contains("uninterpolated"));
        assert!(checked_transcript("/etc/passwd").is_err());
        assert!(checked_transcript("/Users/x/.claude/projects/p/missing.jsonl").unwrap_err().to_string().contains("no transcript"));
    }
}
