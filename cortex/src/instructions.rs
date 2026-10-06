//! The cortex_suite section of a workspace's agent instructions.
//!
//! `CLAUDE.md` and `.github/copilot-instructions.md` belong to the user. Setup
//! used to copy the templates only where neither file existed, and to replace
//! the whole file under `--force`: anyone with instructions of their own got
//! none of cortex_suite's, and nobody could take an update without losing
//! theirs. The part that is ours now sits between two markers, and this module
//! adds or replaces that part and nothing else.
//!
//! The begin marker carries a hash of the section as it was written. A section
//! whose text still matches its hash came from here and is replaced freely; one
//! that does not was edited by hand, and is replaced only with `--force`, after
//! a backup.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

const BEGIN: &str = "<!-- cortex_suite:begin";
const END: &str = "<!-- cortex_suite:end -->";
/// What the begin marker tells a person reading the file.
const NOTE: &str =
    "managed by `cortex instructions`: an update replaces this section, so keep your own rules outside it";

/// One instruction file and the template its section comes from.
pub struct Doc {
    /// Path under the workspace root.
    pub rel: &'static str,
    template: &'static str,
    /// Headings of cortex_suite sections in copies made before the markers
    /// existed (lowercase, without a `1) ` number), each with how much it
    /// counts towards calling a file such a copy.
    legacy_sections: &'static [(&'static str, u32)],
    /// Opening words of cortex_suite paragraphs above the first section in
    /// those copies.
    legacy_paragraphs: &'static [&'static str],
}

/// The README published this as a snippet to paste. It routed agents to calls
/// the server now refuses (no `hint`), so on its own it marks a stale copy.
const README_SNIPPET: (&str, u32) = ("cortex (semantic memory layer)", 2);

/// A heading match counts only if its section names one of these, so a user's
/// own "Limits" or "API facts" section is never taken for ours.
const MENTIONS: &[&str] = &[
    "cortex",
    "quartz-ctx",
    "get_anti_patterns",
    "list_patterns",
    "get_api_context",
    "get_item",
    "get_walls",
    "semantic_search",
    "`recall`",
];

pub const DOCS: [Doc; 2] = [
    Doc {
        rel: "CLAUDE.md",
        template: include_str!("../../templates/CLAUDE.md"),
        legacy_sections: &[
            ("tool routing", 1),
            ("the `hint` is required", 1),
            ("always pass a `hint`", 1),
            ("pre-code check", 1),
            ("mid-task checks", 1),
            ("capturing what you learn", 1),
            ("launcher commands", 1),
            README_SNIPPET,
        ],
        legacy_paragraphs: &[],
    },
    Doc {
        rel: ".github/copilot-instructions.md",
        template: include_str!("../../templates/copilot-instructions.md"),
        legacy_sections: &[
            ("before writing any non-trivial code", 1),
            ("the hint is required", 1),
            ("always pass a hint", 1),
            ("when you get stuck", 1),
            ("when a `[cortex]` warning arrives", 1),
            ("limits", 1),
            ("api facts", 1),
            ("recording what you learn", 1),
            ("launcher commands", 1),
            README_SNIPPET,
        ],
        legacy_paragraphs: &[
            "Two MCP servers back this workspace",
            "- **quartz-ctx**",
            "Reading code under the indexed roots",
        ],
    },
];

/// Where a file's section sits, as offsets into its LF text.
struct Found {
    start: usize,
    body: Range<usize>,
    end: usize,
    stamp: Option<String>,
}

fn find(text: &str) -> Result<Option<Found>, String> {
    let mut open: Option<(usize, usize, Option<String>)> = None;
    let mut found = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with(BEGIN) {
            if open.is_some() || found.is_some() {
                return Err("more than one begin marker".into());
            }
            let stamp = t[BEGIN.len()..]
                .split_whitespace()
                .next()
                .filter(|w| w.len() == 16 && w.chars().all(|c| c.is_ascii_hexdigit()))
                .map(str::to_string);
            open = Some((pos, pos + line.len(), stamp));
        } else if t == END {
            let Some((start, body_start, stamp)) = open.take() else {
                return Err("an end marker with no begin marker before it".into());
            };
            found = Some(Found { start, body: body_start..pos, end: pos + line.len(), stamp });
        }
        pos += line.len();
    }
    if open.is_some() {
        return Err("a begin marker with no end marker".into());
    }
    Ok(found)
}

/// A section's text as compared: trailing spaces and blank lines at either end
/// do not count, so an editor that tidies whitespace has not edited it.
fn canonical(body: &str) -> String {
    let lines: Vec<&str> = body.lines().map(str::trim_end).collect();
    lines.join("\n").trim_matches('\n').to_string()
}

fn stamp_of(body: &str) -> String {
    Sha256::digest(canonical(body).as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The template before, inside and after its section.
fn template_parts(doc: &Doc) -> (&'static str, &'static str, &'static str) {
    let t = doc.template;
    let f = find(t).ok().flatten().unwrap_or_else(|| panic!("templates for {} have no readable section", doc.rel));
    (&t[..f.start], &t[f.body], &t[f.end..])
}

/// The section as written into a file, stamped with its own hash.
pub(crate) fn render(body: &str) -> String {
    format!("{BEGIN} {} · {NOTE} -->\n{}\n{END}\n", stamp_of(body), canonical(body))
}

#[derive(Debug, PartialEq)]
pub enum State {
    Missing,
    /// The user's own file, with no cortex_suite section.
    NoSection,
    /// A copy made before the markers existed; the cortex_suite headings found.
    Legacy(Vec<String>),
    /// Matches the shipped section; `restamp` when its marker's hash is wrong.
    Current { restamp: bool },
    /// Written here, unedited, and older than the shipped section.
    Outdated,
    /// Changed by hand since it was written; `newer` when there is an update
    /// it would lose.
    Edited { newer: bool },
    /// Markers that cannot be read.
    Broken(String),
}

pub fn state(text: Option<&str>, doc: &Doc) -> State {
    let Some(text) = text else { return State::Missing };
    let shipped = stamp_of(template_parts(doc).1);
    match find(text) {
        Err(why) => State::Broken(why),
        Ok(None) => {
            let sections = legacy_sections(text, doc);
            if sections.is_empty() {
                State::NoSection
            } else {
                State::Legacy(sections.into_iter().map(|(_, h)| h).collect())
            }
        }
        Ok(Some(f)) => {
            let now = stamp_of(&text[f.body]);
            if now == shipped {
                return State::Current { restamp: f.stamp.as_deref() != Some(now.as_str()) };
            }
            match f.stamp {
                Some(s) if s == now => State::Outdated,
                Some(s) => State::Edited { newer: s != shipped },
                // Unstamped: copied by hand, so nothing says what it was.
                None => State::Edited { newer: true },
            }
        }
    }
}

/// Headings outside code fences: line, level, and the title lowercased
/// without a `1) ` number.
fn headings(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        let level = l.bytes().take_while(|&b| b == b'#').count();
        if fenced || level == 0 || level > 6 {
            continue;
        }
        let Some(title) = l[level..].strip_prefix(' ') else { continue };
        let mut title = title.trim();
        if let Some((n, rest)) = title.split_once(") ") {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                title = rest;
            }
        }
        out.push((i, level, title.to_lowercase()));
    }
    out
}

/// The cortex_suite sections of a copy made before the markers, as line ranges
/// with their headings as written. Empty unless they add up to such a copy.
fn legacy_sections(text: &str, doc: &Doc) -> Vec<(Range<usize>, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let hs = headings(&lines);
    let mut found: Vec<(Range<usize>, String)> = Vec::new();
    let mut weight = 0;
    for (n, (line, level, key)) in hs.iter().enumerate() {
        let Some(&(_, w)) = doc.legacy_sections.iter().find(|(p, _)| key.starts_with(p)) else { continue };
        // A subsection of one already taken goes with it.
        if found.iter().any(|(r, _)| r.contains(line)) {
            continue;
        }
        let end = hs[n + 1..].iter().find(|(_, l, _)| l <= level).map_or(lines.len(), |(i, _, _)| *i);
        let body = lines[*line..end].join("\n").to_lowercase();
        if !MENTIONS.iter().any(|m| body.contains(m)) {
            continue;
        }
        weight += w;
        found.push((*line..end, lines[*line].trim().to_string()));
    }
    if weight >= 2 { found } else { Vec::new() }
}

/// Replace a pre-marker copy's cortex_suite sections (and, for Copilot, its
/// opening paragraphs) with `section`, placed where the first of them was.
fn adopt(text: &str, doc: &Doc, section: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut remove: Vec<Range<usize>> = legacy_sections(text, doc).into_iter().map(|(r, _)| r).collect();
    let first = remove.iter().map(|r| r.start).min().unwrap_or(lines.len());
    // Paragraphs above the first section, each with the blank line after it.
    let mut i = 0;
    while i < first {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        while i < first && !lines[i].trim().is_empty() {
            i += 1;
        }
        if doc.legacy_paragraphs.iter().any(|p| lines[start].trim_start().starts_with(p)) {
            let end = if i < first && lines[i].trim().is_empty() { i + 1 } else { i };
            remove.push(start..end);
        }
    }
    let at = remove.iter().map(|r| r.start).min().unwrap_or(lines.len());
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == at {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(section);
            out.push('\n');
        }
        if !remove.iter().any(|r| r.contains(&i)) {
            out.push_str(l);
            out.push('\n');
        }
    }
    if at >= lines.len() {
        out.push_str(section);
    }
    out
}

fn new_file(doc: &Doc, project: &str) -> String {
    let (before, body, after) = template_parts(doc);
    format!("{}{}{}", before.replace("<PROJECT>", project), render(body), after.replace("<PROJECT>", project))
}

fn append(text: &str, section: &str) -> String {
    let base = text.trim_end();
    if base.is_empty() { section.to_string() } else { format!("{base}\n\n{section}") }
}

fn replace(text: &str, section: &str) -> String {
    match find(text) {
        Ok(Some(f)) => format!("{}{}{}", &text[..f.start], section, &text[f.end..]),
        _ => text.to_string(),
    }
}

/// Text with LF line endings and no BOM, and how to put both back.
fn decode(raw: &str) -> (bool, bool, String) {
    let bom = raw.starts_with('\u{feff}');
    let t = raw.trim_start_matches('\u{feff}');
    (bom, t.contains("\r\n"), t.replace("\r\n", "\n"))
}

fn encode(text: &str, bom: bool, crlf: bool) -> String {
    let body = if crlf { text.replace('\n', "\r\n") } else { text.to_string() };
    if bom { format!("\u{feff}{body}") } else { body }
}

pub struct Opts {
    /// Report only; write nothing.
    pub check: bool,
    /// Replace a section edited by hand.
    pub force: bool,
    /// Convert a copy made before the markers.
    pub adopt: bool,
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub file: &'static str,
    /// missing | none | legacy | current | outdated | edited | broken
    pub state: &'static str,
    /// What was done, or with --check what would be.
    pub action: String,
    /// The file was rewritten (with --check: would be).
    pub changed: bool,
    /// Something here waits on a person's decision.
    pub attention: bool,
    /// Where the previous version went, when it could not be regenerated.
    pub backup: Option<String>,
}

/// Bring every instruction file under `root` up to date.
pub fn sync(root: &Path, project: &str, opts: &Opts) -> Result<Vec<Outcome>> {
    DOCS.iter().map(|doc| sync_one(root, doc, project, opts)).collect()
}

fn sync_one(root: &Path, doc: &Doc, project: &str, opts: &Opts) -> Result<Outcome> {
    let path = root.join(doc.rel);
    let raw = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let (bom, crlf, text) = raw.as_deref().map_or((false, false, String::new()), decode);
    let section = render(template_parts(doc).1);
    let st = state(raw.as_ref().map(|_| text.as_str()), doc);
    // A backup only where the old text cannot be had back from a template.
    let mut keep = false;
    let (label, new_text, action, attention) = match &st {
        State::Missing => ("missing", Some(new_file(doc, project)), "written from the template".to_string(), false),
        State::NoSection => (
            "none",
            Some(append(&text, &section)),
            "cortex_suite section added at the end; nothing else in the file changed".to_string(),
            false,
        ),
        State::Legacy(hs) if opts.adopt => {
            keep = true;
            let msg = format!("its old cortex_suite sections ({}) replaced by the managed section", hs.join(", "));
            ("legacy", Some(adopt(&text, doc, &section)), msg, false)
        }
        State::Legacy(hs) => (
            "legacy",
            None,
            format!(
                "a copy of an older cortex_suite template, from before the section markers; left alone. \
                 `{}` replaces its cortex_suite sections ({}) with the managed section and keeps a backup",
                crate::cache::launcher_command("instructions --adopt"),
                hs.join(", ")
            ),
            true,
        ),
        State::Current { restamp: true } => {
            ("current", Some(replace(&text, &section)), "section current; its marker restamped".to_string(), false)
        }
        State::Current { restamp: false } => ("current", None, "section current".to_string(), false),
        State::Outdated => (
            "outdated",
            Some(replace(&text, &section)),
            "section updated; nothing else in the file changed".to_string(),
            false,
        ),
        State::Edited { .. } if opts.force => {
            keep = true;
            ("edited", Some(replace(&text, &section)), "hand-edited section replaced".to_string(), false)
        }
        State::Edited { newer: true } => (
            "edited",
            None,
            format!(
                "a newer cortex_suite section ships with this cortex, but this one was edited by hand; left alone. \
                 Move your changes outside the section, then run `{}` (a backup is kept)",
                crate::cache::launcher_command("instructions --force")
            ),
            true,
        ),
        State::Edited { newer: false } => {
            ("edited", None, "section edited by hand; nothing newer to apply".to_string(), false)
        }
        State::Broken(why) => (
            "broken",
            None,
            format!("its cortex_suite markers cannot be read ({why}); left alone, fix them by hand"),
            true,
        ),
    };
    let changed = new_text.is_some();
    let mut out = Outcome { file: doc.rel, state: label, action, changed, attention, backup: None };
    if opts.check {
        if changed {
            out.action = format!("would change: {}", out.action);
        }
        return Ok(out);
    }
    if let Some(new_text) = new_text {
        if keep {
            if let Some(raw) = &raw {
                out.backup = Some(backup(root, &path, raw)?.display().to_string());
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        }
        // Written beside the file and renamed over it, so an interrupted write
        // cannot leave a user's instructions half there.
        let tmp = path.with_extension("md.cortex-tmp");
        std::fs::write(&tmp, encode(&new_text, bom, crlf)).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("failed to replace {}", path.display()))?;
    }
    Ok(out)
}

/// The previous version, under `.cortex/backups/` with a microsecond stamp so
/// two runs in one second cannot overwrite each other's copy.
fn backup(root: &Path, path: &Path, raw: &str) -> Result<PathBuf> {
    let dir = root.join(".cortex").join("backups");
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let to = dir.join(format!("{name}.{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")));
    std::fs::write(&to, raw).with_context(|| format!("failed to write {}", to.display()))?;
    Ok(to)
}

/// Lines for AWAITING YOUR REVIEW: sections this cortex wrote and can update.
/// A file with no section, or a copy from before the markers, is not listed,
/// so a deliberate choice is not raised at every closeout.
pub fn review_items(root: &Path) -> String {
    let mut out = String::new();
    for doc in &DOCS {
        let Ok(raw) = std::fs::read_to_string(root.join(doc.rel)) else { continue };
        let (_, _, text) = decode(&raw);
        match state(Some(&text), doc) {
            State::Outdated => out.push_str(&format!(
                "  {}: a newer cortex_suite section ships with this cortex\n    update: {}  (replaces that section only)\n",
                doc.rel,
                crate::cache::launcher_command("instructions")
            )),
            State::Edited { newer: true } => out.push_str(&format!(
                "  {}: a newer cortex_suite section ships with this cortex, and this one was edited by hand\n    \
                 move your changes outside it, then: {}\n",
                doc.rel,
                crate::cache::launcher_command("instructions --force")
            )),
            _ => {}
        }
    }
    out
}

/// A rule an older cortex_suite gave agents that is no longer true.
///
/// `instructions` replaces only its own section, so an older rule survives
/// wherever a person kept it: their own text beside the section, a copy made
/// before the markers, a skill, the notes in `.cortex/prefs.toml` (which
/// `get_preferences` serves to every agent), or the user-level CLAUDE.md.
/// Nothing here rewrites what a person wrote. It names each line and what is
/// true now, and whoever runs the update fixes it (SETUP_HANDOFF.md section 0).
struct Superseded {
    /// Fragments of the old wording, lowercase, that appear in this order on
    /// one line. They pin the old CLAIM, not its subject: the current templates
    /// discuss most of these subjects, and a test keeps every rule off them.
    old: &'static [&'static str],
    /// A line the rule must catch, so a typo cannot leave it matching nothing.
    example: &'static str,
    /// What is true now.
    now: &'static str,
}

const MARKERS_NOW: &str = "markers commit themselves: Claude Code's Stop and PreCompact hooks capture them \
     from the transcript, so `markers_text` is optional there (`cortex knowledge status` says whether the \
     hooks have run), and closeout in VS Code reads them from the chat";
const CLOSEOUT_NOW: &str = "end a verified task with the TASK COMPLETE block and call \
     closeout_session(outcome_type=\"build_pass\"); ask for KNOWLEDGE COMMITTED only when closeout reports \
     staged mode, which means the audit switched automatic commit off";
const HINT_NOW: &str =
    "get_anti_patterns, list_patterns and get_preferences refuse a call without a `hint` naming what you are about to write";
const PRECODE_NOW: &str = "the pre-code check covers any non-trivial function (anything that constructs, \
     ticks, spawns or touches shared state): get_api_context, get_anti_patterns and list_patterns, each with a \
     hint naming what you are about to write";
const COMPACT_NOW: &str = "compact_output is the build/test observer cortex's hooks call; agents do not call \
     it, and nothing shortens command output (no hook can replace a shell result)";
const GRAPH_NOW: &str = "graphify is served through `cortex graphify-serve`, which rebuilds graph.json and \
     reloads it when the source moves; there is nothing to rebuild by hand";

const SUPERSEDED: &[Superseded] = &[
    Superseded {
        old: &["omitting", "markers_text", "commits nothing"],
        example: "so omitting `markers_text` commits nothing and reports success.",
        now: MARKERS_NOW,
    },
    Superseded {
        old: &["markers_text", "is required"],
        example: "**`markers_text` is required on Claude Code.**",
        now: MARKERS_NOW,
    },
    Superseded { old: &["store to scrape"], example: "There is no host chat-store to scrape; omit it", now: MARKERS_NOW },
    Superseded { old: &["markers alone do not save"], example: "Markers alone do not save anything.", now: MARKERS_NOW },
    Superseded {
        old: &["anything else to skip"],
        example: "Reply KNOWLEDGE COMMITTED to commit, anything else to skip.",
        now: CLOSEOUT_NOW,
    },
    Superseded {
        old: &["wrong:", "\\ncorrect:"],
        example: "[CORTEX-AP: description=\"...\"]wrong: ...\\ncorrect: ...[/CORTEX-AP]",
        now: "`correct:` must begin its own line: written after a literal \\n, the whole body is stored as `wrong` \
              and the fix as a placeholder",
    },
    Superseded {
        old: &["knowledge committed", "markers_text"],
        example: "On KNOWLEDGE COMMITTED, call closeout_session with inline_approve=true and markers_text set",
        now: MARKERS_NOW,
    },
    Superseded {
        old: &["type knowledge committed"],
        example: "session-end: after any coding session, type KNOWLEDGE COMMITTED to trigger closeout",
        now: CLOSEOUT_NOW,
    },
    Superseded { old: &["to commit: reply knowledge committed"], example: "To commit: reply KNOWLEDGE COMMITTED", now: CLOSEOUT_NOW },
    Superseded { old: &["session end", "knowledge committed"], example: "## Session end — KNOWLEDGE COMMITTED", now: CLOSEOUT_NOW },
    Superseded {
        old: &["run post-session"],
        example: "session-end mandatory: after any coding session run post-session then annotate new bugs",
        now: CLOSEOUT_NOW,
    },
    Superseded { old: &["cortex.ps1 post-session"], example: "Session-End: cortex.ps1 post-session", now: CLOSEOUT_NOW },
    Superseded {
        old: &["flush_knowledge_markers"],
        example: "call flush_knowledge_markers(text=...) to stage markers",
        now: "don't route agents to flush_knowledge_markers: on Claude Code its quoted attributes arrive mangled; \
              markers commit through the capture hooks or closeout_session",
    },
    Superseded { old: &["get_anti_patterns()"], example: "1. `get_anti_patterns()` — known traps", now: HINT_NOW },
    Superseded { old: &["list_patterns()"], example: "call list_patterns() at session start", now: HINT_NOW },
    Superseded { old: &["get_preferences()"], example: "Follow `get_preferences()` for naming", now: HINT_NOW },
    Superseded { old: &["hint is optional"], example: "The hint is optional but recommended.", now: HINT_NOW },
    Superseded {
        old: &["baseline retrieval"],
        example: "baseline retrieval: get_delta → get_preferences → get_anti_patterns",
        now: HINT_NOW,
    },
    Superseded {
        old: &["get_anti_patterns + get_preferences + list_patterns"],
        example: "MANDATORY PRE-CODE CHECK: call get_anti_patterns + get_preferences + list_patterns",
        now: PRECODE_NOW,
    },
    Superseded {
        old: &["factory/tick"],
        example: "Before writing ANY factory/tick/spawn/physics/pool function:",
        now: PRECODE_NOW,
    },
    Superseded { old: &["factory, tick"], example: "factory, tick/update, spawn, pool: check first", now: PRECODE_NOW },
    Superseded {
        old: &["get_variants(enum"],
        example: "- `get_variants(enum)` — exact variants with field types",
        now: "get_variants takes `name`, as get_item does; passing `enum` fails with missing `name`",
    },
    Superseded {
        old: &["delete from response_cache"],
        example: "DELETE FROM response_cache;",
        now: "nothing caches responses any more; there is nothing to clear after a rebuild",
    },
    Superseded {
        old: &["clear the response cache"],
        example: "Then clear the response cache.",
        now: "nothing caches responses any more; there is nothing to clear after a rebuild",
    },
    Superseded {
        old: &["may predate your edits"],
        example: "[stale index] answers may predate your edits.",
        now: "every call checks the disk first; `[stale index]` appears only when a refresh failed, naming the \
              root and the error, and reindex is never needed for correct answers",
    },
    Superseded {
        old: &["real front end agreed"],
        example: "`resolved` means a real front end agreed the types",
        now: "no language's types are inferred: `resolved` (Rust) means the language requires declared types, so \
              a signature is complete as written",
    },
    Superseded {
        old: &["grep stays right for free text"],
        example: "grep stays right for free text, logs and config",
        now: "search_code and get_source(file, lines) read logs and config too; grep stays right only for \
              filtering a command's output",
    },
    Superseded {
        old: &["reconnect it once from"],
        example: "after a rebuild, reconnect it once from `/mcp`",
        now: "a server waiting for a request moves onto a rebuild by itself (macOS, Linux); only a terminal \
              session can reconnect with /mcp, and the desktop app needs a restart",
    },
    Superseded {
        old: &["graph is a snapshot"],
        example: "The graph is a snapshot: rebuild it after significant changes.",
        now: GRAPH_NOW,
    },
    Superseded { old: &["stale graph answers"], example: "A stale graph answers confidently and wrongly.", now: GRAPH_NOW },
    Superseded {
        old: &["agent_customization/skills"],
        example: "skills_dir = \"agent_customization/skills\"",
        now: "approved skills publish to .claude/skills (Claude Code) and .github/prompts (Copilot); set \
              skills_dir = \".claude/skills\" under [skills] in .cortex/prefs.toml",
    },
    Superseded {
        old: &["snippet from cortex/readme"],
        example: "5. Copy the copilot-instructions.md snippet from cortex/README.md",
        now: "`cortex instructions` adds and updates the cortex_suite section; there is no snippet to copy",
    },
    Superseded {
        old: &["compact_output", "losslessly"],
        example: "compact_output (MCP) losslessly strips only provably-redundant command output",
        now: COMPACT_NOW,
    },
    Superseded {
        old: &["call the compact_output"],
        example: "it cannot auto-compact; call the compact_output MCP tool directly instead",
        now: COMPACT_NOW,
    },
    Superseded {
        old: &["check-mcp", "relative paths"],
        example: "| `check-mcp` | validate both MCP configs: relative paths, no drift between hosts |",
        now: "check-mcp checks that each command resolves (an absolute path is fine when it exists) and that \
              the two MCP configs agree",
    },
    Superseded {
        old: &["existing root first"],
        example: "Keep an existing root first in index-sources.json.",
        now: "the order of index-sources.json means nothing: a missing root is skipped wherever it sits, and \
              the server starts anyway",
    },
];

/// Tools an older cortex served, or an older plan named, that no server has
/// now. A rule naming one fails at the call, after the agent chose it.
const REMOVED_TOOLS: &[&str] = &[
    "recurrent_think",
    "get_code_examples",
    "check_anti_patterns",
    "validate_physics_config",
    "check_lifetime_constraints",
    "suggest_action_for_intent",
    "get_tick_loop_order",
    "explain_behavior",
    "get_usage_patterns",
    "get_engine_constants",
];
const REMOVED_NOW: &str = "no server has this tool any more; route to the tools the cortex_suite section names";

/// The old wording's fragments, in order, on this lowercased line.
fn says(line: &str, old: &[&str]) -> bool {
    let mut rest = line;
    old.iter().all(|f| match rest.find(f) {
        Some(i) => {
            rest = &rest[i + f.len()..];
            true
        }
        None => false,
    })
}

/// What is true now, when this lowercased line repeats an older rule.
fn older_rule(lower: &str) -> Option<&'static str> {
    SUPERSEDED
        .iter()
        .find(|r| says(lower, r.old))
        .map(|r| r.now)
        .or_else(|| REMOVED_TOOLS.iter().any(|t| lower.contains(t)).then_some(REMOVED_NOW))
}

/// One line of older cortex guidance, and what is true now.
#[derive(Debug, Serialize)]
pub struct Stale {
    /// `file:line`, or `store annotation <id> (<topic>)`.
    pub place: String,
    pub text: String,
    pub now: &'static str,
    /// The command that removes it, where an edit cannot reach it.
    pub fix: Option<String>,
}

/// The text with its whitespace collapsed, cut at 140 characters.
fn excerpt(text: &str) -> String {
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(140) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// The files agents read instructions from, as (label, path), existing ones
/// only: the workspace's instruction files, skills and prompt files, the notes
/// in `.cortex/prefs.toml`, and `user_claude` (the user-level CLAUDE.md).
fn guidance_files(root: &Path, user_claude: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = [
        "CLAUDE.md",
        "CLAUDE.local.md",
        ".claude/CLAUDE.md",
        ".github/copilot-instructions.md",
        "AGENTS.md",
        ".cortex/prefs.toml",
    ]
    .iter()
    .map(|rel| (rel.to_string(), root.join(rel)))
    .collect();
    let listed = |dir: &str, keep: &dyn Fn(&Path) -> Option<PathBuf>| -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return Vec::new() };
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| keep(&e.path()))
            .filter_map(|p| Some((p.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/"), p)))
            .collect();
        found.sort();
        found
    };
    let ending = |suffix: &'static str| move |p: &Path| p.to_string_lossy().ends_with(suffix).then(|| p.to_path_buf());
    out.extend(listed(".github/instructions", &ending(".instructions.md")));
    out.extend(listed(".github/prompts", &ending(".prompt.md")));
    out.extend(listed(".claude/skills", &|p: &Path| Some(p.join("SKILL.md"))));
    if let Some(p) = user_claude {
        out.push((p.display().to_string(), p.to_path_buf()));
    }
    out.retain(|(_, p)| p.is_file());
    out
}

/// Every line of older cortex guidance in the files agents read, outside the
/// cortex_suite section (that one is current, or `instructions` reports it).
pub fn stale_guidance(root: &Path, user_claude: Option<&Path>) -> Vec<Stale> {
    let mut out = Vec::new();
    for (label, path) in guidance_files(root, user_claude) {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let (_, _, text) = decode(&raw);
        let ours = find(&text).ok().flatten().map(|f| f.start..f.end);
        let mut pos = 0;
        for (n, line) in text.split_inclusive('\n').enumerate() {
            let inside = ours.as_ref().is_some_and(|r| r.contains(&pos));
            pos += line.len();
            if inside {
                continue;
            }
            if let Some(now) = older_rule(&line.to_lowercase()) {
                out.push(Stale { place: format!("{label}:{}", n + 1), text: excerpt(line), now, fix: None });
            }
        }
    }
    out
}

/// The store's side of an update.
#[derive(Debug, Default, Serialize)]
pub struct StoreTidy {
    /// Copies of cortex's tool descriptions an older first run seeded, by
    /// topic: removed, or under `--check` to be removed.
    pub seeded_copies: Vec<String>,
    /// Where the store was copied before they were removed.
    pub backup: Option<String>,
}

/// A copy of one of cortex's tool descriptions as a first run used to seed
/// it: topic `MCP: <tool>`, body `Params: ...`. Each tool describes itself to
/// every client in tools/list, and that changes with the code; the copy stayed
/// as written and recall and get_context served it beside the tool. By
/// 2026-10 five of FlowMake's thirteen contradicted their tool, three with
/// `Params: none` for a required hint, and one named a tool no server has.
fn seeded_tool_copy(topic: &str, body: &str) -> bool {
    topic.starts_with("MCP: ") && body.trim_start().starts_with("Params:")
}

/// Remove the seeded tool copies from the workspace's store after a backup
/// (under `check`, only name them), and list the annotations a person wrote
/// that repeat an older rule. A store that does not exist is not created.
pub fn tidy_store(root: &Path, db: &Path, check: bool) -> Result<(StoreTidy, Vec<Stale>)> {
    let mut tidy = StoreTidy::default();
    let mut stale = Vec::new();
    if !db.is_file() {
        return Ok((tidy, stale));
    }
    let store = crate::memory::Store::open(db)?;
    let mut seeded = Vec::new();
    for a in store.all_annotations()? {
        let Some(id) = a.id else { continue };
        if seeded_tool_copy(&a.topic, &a.body) {
            seeded.push((id, a.topic));
            continue;
        }
        if let Some(now) = older_rule(&format!("{} {}", a.topic, a.body).to_lowercase()) {
            let fix = Some(crate::cache::launcher_command(&format!("annotate remove {id}")));
            let place = format!("store annotation {id} ({})", excerpt(&a.topic));
            stale.push(Stale { place, text: excerpt(&a.body), now, fix });
        }
    }
    seeded.sort();
    if !check && !seeded.is_empty() {
        let dir = root.join(".cortex").join("backups");
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let name = db.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let to = dir.join(format!("{name}.{}.sqlite3", chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")));
        // VACUUM INTO reads through SQLite, so commits still in the -wal file
        // are in the copy (see Store::backup_before_migration).
        store
            .conn()
            .execute("VACUUM INTO ?1", rusqlite::params![to.to_string_lossy()])
            .with_context(|| format!("failed to back up {} to {}", db.display(), to.display()))?;
        for (id, _) in &seeded {
            store.delete_annotation(*id)?;
        }
        tidy.backup = Some(to.display().to_string());
    }
    tidy.seeded_copies = seeded.into_iter().map(|(_, topic)| topic).collect();
    Ok((tidy, stale))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::new(tag).expect("temp dir")
    }

    fn run(root: &Path, opts: Opts) -> Vec<Outcome> {
        sync(root, "demo", &opts).unwrap()
    }

    fn plain() -> Opts {
        Opts { check: false, force: false, adopt: false }
    }

    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn each_template_has_one_unstamped_section_with_no_placeholder_inside() {
        for doc in &DOCS {
            let f = find(doc.template).unwrap().unwrap_or_else(|| panic!("{} has no section", doc.rel));
            assert_eq!(f.stamp, None, "{}: the template's marker must not carry a hash", doc.rel);
            let (before, body, after) = template_parts(doc);
            assert!(!body.contains("<PROJECT>"), "{}: the section is the same for every project", doc.rel);
            assert!(canonical(body).lines().count() > 40, "{}", doc.rel);
            assert!(!before.contains(BEGIN) && !after.contains(END), "{}", doc.rel);
            // A fresh template must not read as a pre-marker copy of itself.
            assert_eq!(state(Some(&new_file(doc, "demo")), doc), State::Current { restamp: false }, "{}", doc.rel);
        }
    }

    #[test]
    fn a_new_workspace_gets_both_files_and_a_second_run_changes_nothing() {
        let d = tmp("instr_new");
        let first = run(d.path(), plain());
        assert!(first.iter().all(|o| o.state == "missing" && o.changed), "{first:?}");
        assert!(read(d.path(), ".github/copilot-instructions.md").starts_with("# Copilot Instructions — demo"));
        let claude = read(d.path(), "CLAUDE.md");
        assert!(claude.contains("## How to work") && claude.contains("# Compact instructions"));
        let second = run(d.path(), plain());
        assert!(second.iter().all(|o| o.state == "current" && !o.changed), "{second:?}");
        assert_eq!(read(d.path(), "CLAUDE.md"), claude);
    }

    #[test]
    fn a_users_own_file_keeps_every_line_and_gains_the_section() {
        let d = tmp("instr_own");
        let mine = "# Our rules\n\nAlways write tests first.\n";
        std::fs::write(d.path().join("CLAUDE.md"), mine).unwrap();
        let o = run(d.path(), plain());
        assert_eq!((o[0].state, o[0].changed), ("none", true));
        let now = read(d.path(), "CLAUDE.md");
        assert!(now.starts_with(mine), "{now}");
        assert!(now.contains("## Tool routing") && now.trim_end().ends_with(END));
        assert_eq!(run(d.path(), plain())[0].state, "current");
    }

    #[test]
    fn an_outdated_section_is_replaced_and_its_surroundings_are_not() {
        let d = tmp("instr_old");
        let doc = &DOCS[0];
        let text = format!("# Mine\n\nabove\n\n{}\nbelow\n", render("## Tool routing\n\nthe old text\n"));
        std::fs::write(d.path().join("CLAUDE.md"), &text).unwrap();
        assert_eq!(state(Some(&text), doc), State::Outdated);
        assert!(review_items(d.path()).contains("CLAUDE.md: a newer cortex_suite section"));
        let o = run(d.path(), plain());
        assert_eq!((o[0].state, o[0].changed, o[0].backup.is_none()), ("outdated", true, true));
        let now = read(d.path(), "CLAUDE.md");
        assert!(now.starts_with("# Mine\n\nabove\n\n") && now.ends_with(&format!("{END}\n\nbelow\n")), "{now}");
        assert!(!now.contains("the old text"));
        assert_eq!(review_items(d.path()), "");
    }

    #[test]
    fn a_hand_edited_section_is_kept_until_forced_and_then_backed_up() {
        let d = tmp("instr_edit");
        let edited = render("## Tool routing\n\nthe old text\n").replace("the old text", "the old text, and my edit");
        std::fs::write(d.path().join("CLAUDE.md"), &edited).unwrap();
        let o = run(d.path(), plain());
        assert_eq!((o[0].state, o[0].changed, o[0].attention), ("edited", false, true));
        assert_eq!(read(d.path(), "CLAUDE.md"), edited);
        assert!(review_items(d.path()).contains("--force"));
        let o = run(d.path(), Opts { force: true, ..plain() });
        let kept = o[0].backup.clone().expect("a backup of the edited file");
        assert_eq!(std::fs::read_to_string(kept).unwrap(), edited);
        assert_eq!(run(d.path(), plain())[0].state, "current");
    }

    #[test]
    fn check_reports_without_writing() {
        let d = tmp("instr_check");
        let o = run(d.path(), Opts { check: true, ..plain() });
        assert!(o.iter().all(|o| o.changed && o.action.starts_with("would change")), "{o:?}");
        assert!(!d.path().join("CLAUDE.md").exists());
    }

    #[test]
    fn crlf_and_a_bom_survive_an_update() {
        let d = tmp("instr_crlf");
        std::fs::write(d.path().join("CLAUDE.md"), "\u{feff}# Ours\r\n\r\nrule one\r\n").unwrap();
        run(d.path(), plain());
        let now = read(d.path(), "CLAUDE.md");
        assert!(now.starts_with("\u{feff}# Ours\r\n"));
        assert!(!now.replace("\r\n", "").contains('\n'), "every line ending stays CRLF");
        assert_eq!(run(d.path(), plain())[0].state, "current");
    }

    #[test]
    fn a_pre_marker_copy_is_named_and_converted_only_when_asked() {
        let d = tmp("instr_legacy");
        let old = "# Agent Operating Manual\n\nOur team note.\n\n## 0) How to work\n\n- act\n\n\
                   ## 1) Tool routing — structure vs judgment\n\nquartz-ctx owns structure.\n\n### Detail\n\nmore\n\n\
                   ## 2) The `hint` is required — say what you are doing\n\nget_anti_patterns refuses.\n\n\
                   ## 6) Editing safety\n\n- small patches\n";
        std::fs::write(d.path().join("CLAUDE.md"), old).unwrap();
        let o = run(d.path(), plain());
        assert_eq!((o[0].state, o[0].changed, o[0].attention), ("legacy", false, true));
        assert!(o[0].action.contains("## 1) Tool routing") && o[0].action.contains("--adopt"));
        assert_eq!(review_items(d.path()), "", "never raised at closeout");
        let o = run(d.path(), Opts { adopt: true, ..plain() });
        assert!(o[0].backup.is_some());
        let now = read(d.path(), "CLAUDE.md");
        for kept in ["Our team note.", "## 0) How to work", "- act", "## 6) Editing safety", "- small patches"] {
            assert!(now.contains(kept), "{kept} lost:\n{now}");
        }
        assert!(!now.contains("quartz-ctx owns structure.") && !now.contains("### Detail"), "{now}");
        assert!(now.find("## 0) How to work") < now.find(BEGIN) && now.find(END) < now.find("## 6) Editing safety"));
        assert_eq!(run(d.path(), plain())[0].state, "current");
    }

    #[test]
    fn the_old_readme_snippet_counts_as_a_copy_and_unrelated_headings_do_not() {
        let doc = &DOCS[1];
        let snippet = "# Ours\n\n## Cortex (Semantic Memory Layer)\n\ncortex holds knowledge.\n\n\
                       ### PROTOCOL - CORTEX Trigger\n\nrun get_anti_patterns\n\n## Deploying\n\nship it\n";
        let State::Legacy(hs) = state(Some(snippet), doc) else { panic!("{:?}", state(Some(snippet), doc)) };
        assert_eq!(hs, vec!["## Cortex (Semantic Memory Layer)".to_string()]);
        let converted = adopt(snippet, doc, &render(template_parts(doc).1));
        assert!(converted.contains("## Deploying") && !converted.contains("PROTOCOL - CORTEX"), "{converted}");
        // A user's own "Limits" and "API facts" are not ours.
        let own = "# Ours\n\n## Limits\n\nRate limit is 10/s.\n\n## API facts\n\nUse v2.\n";
        assert_eq!(state(Some(own), doc), State::NoSection);
    }

    #[test]
    fn an_old_copilot_copy_loses_its_opening_paragraphs_too() {
        let doc = &DOCS[1];
        let old = "# Copilot Instructions — demo\n\nTwo MCP servers back this workspace. Use them.\n\n\
                   - **quartz-ctx** — structure\n- **cortex** — judgment\n\nReading code under the indexed roots: get_source.\n\n\
                   Our own preamble.\n\n## Before writing any non-trivial code\n\n1. get_api_context\n\n\
                   ## API facts\n\n- get_item(name)\n\n## Style\n\n- ours\n";
        let now = adopt(old, doc, &render(template_parts(doc).1));
        // The section takes the place of the first paragraph it replaces.
        assert!(now.starts_with(&format!("# Copilot Instructions — demo\n\n{BEGIN}")), "{now}");
        assert!(now.contains(&format!("{END}\n\nOur own preamble.\n\n## Style\n\n- ours\n")), "{now}");
        assert!(!now.contains("Two MCP servers back this workspace. Use them."));
        assert_eq!(state(Some(&now), doc), State::Current { restamp: false });
    }

    #[test]
    fn unreadable_markers_are_left_for_a_person() {
        let d = tmp("instr_broken");
        let text = format!("# Mine\n\n{BEGIN} -->\nno end\n");
        std::fs::write(d.path().join("CLAUDE.md"), &text).unwrap();
        let o = run(d.path(), plain());
        assert_eq!((o[0].state, o[0].changed, o[0].attention), ("broken", false, true));
        assert_eq!(read(d.path(), "CLAUDE.md"), text);
    }

    #[test]
    fn a_hand_copied_section_that_matches_is_restamped_not_reported() {
        let doc = &DOCS[0];
        let (_, body, _) = template_parts(doc);
        let text = format!("{BEGIN} -->\n{body}{END}\n");
        assert_eq!(state(Some(&text), doc), State::Current { restamp: true });
        let fixed = replace(&text, &render(body));
        assert_eq!(state(Some(&fixed), doc), State::Current { restamp: false });
    }

    /// A rule that matched current text would send every update after a line
    /// that is right; one that missed its own example would never fire.
    #[test]
    fn every_older_rule_catches_its_example_and_nothing_shipped_today() {
        for r in SUPERSEDED {
            assert!(says(&r.example.to_lowercase(), r.old), "{:?} misses its own example", r.old);
        }
        for t in REMOVED_TOOLS {
            assert_eq!(older_rule(&format!("then call `{t}(hint)`")), Some(REMOVED_NOW), "{t}");
        }
        let shipped = [
            ("templates/CLAUDE.md", DOCS[0].template),
            ("templates/copilot-instructions.md", DOCS[1].template),
            ("templates/skills/frontier/SKILL.md", include_str!("../../templates/skills/frontier/SKILL.md")),
            ("templates/skills/frontier.prompt.md", include_str!("../../templates/skills/frontier.prompt.md")),
            ("the first-run prefs.toml", crate::memory::PREFS_TEMPLATE),
        ];
        for (name, text) in shipped {
            for (n, line) in text.lines().enumerate() {
                if let Some(now) = older_rule(&line.to_lowercase()) {
                    panic!("{name}:{}: {line:?} reads as an older rule (now: {now})", n + 1);
                }
            }
        }
    }

    #[test]
    fn older_guidance_is_named_wherever_agents_read_it_except_inside_the_section() {
        let d = tmp("instr_stale");
        let root = d.path();
        // The user's own stale line is reported; the managed section's is not,
        // since `instructions` owns that text and replaces it itself.
        let section = render("## Ours\n\nso omitting `markers_text` commits nothing\n");
        std::fs::write(root.join("CLAUDE.md"), format!("# Mine\n\nTo commit: reply KNOWLEDGE COMMITTED\n\n{section}"))
            .unwrap();
        std::fs::create_dir_all(root.join(".cortex")).unwrap();
        std::fs::write(
            root.join(".cortex/prefs.toml"),
            "[project]\nnotes = [\n    \"call get_anti_patterns + get_preferences + list_patterns\",\n]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".claude/skills/lookup")).unwrap();
        std::fs::write(root.join(".claude/skills/lookup/SKILL.md"), "# lookup\n\n**`markers_text` is required on Claude Code.**\n")
            .unwrap();
        std::fs::write(root.join("AGENTS.md"), "Nothing older here.\n").unwrap();
        let user = root.join("user-CLAUDE.md");
        std::fs::write(&user, "## Session end — KNOWLEDGE COMMITTED\n").unwrap();
        let found: Vec<String> = stale_guidance(root, Some(&user)).into_iter().map(|s| s.place).collect();
        assert_eq!(
            found,
            vec![
                "CLAUDE.md:3".to_string(),
                ".cortex/prefs.toml:3".to_string(),
                ".claude/skills/lookup/SKILL.md:3".to_string(),
                format!("{}:1", user.display()),
            ]
        );
    }

    #[test]
    fn seeded_tool_copies_leave_the_store_after_a_backup_and_a_persons_older_note_is_named() {
        use crate::model::Annotation;
        let store = crate::test_support::TempStore::new("instr_tidy").unwrap();
        let root = store.dir().to_path_buf();
        let db = root.join("memory.db");
        let add = |topic: &str, body: &str| {
            store
                .insert_annotation(&Annotation {
                    id: None,
                    topic: topic.to_string(),
                    body: body.to_string(),
                    tags: vec![],
                    added_at: chrono::Utc::now(),
                    hash: None,
                })
                .unwrap()
        };
        add("MCP: get_preferences", "Params: none. Returns active prefs.toml summary loaded at server startup.");
        add("MCP: recurrent_think", "Params: task str required, hypothesis str.");
        let mine = add("deploy", "Then clear the response cache.");
        add("MCP: recall", "Mine: recall is cheaper than semantic_search for exact names.");
        let topics = |s: &crate::memory::Store| {
            let mut t: Vec<String> = s.all_annotations().unwrap().into_iter().map(|a| a.topic).collect();
            t.sort();
            t
        };

        let (tidy, stale) = tidy_store(&root, &db, true).unwrap();
        assert_eq!(tidy.seeded_copies, ["MCP: get_preferences", "MCP: recurrent_think"]);
        assert!(tidy.backup.is_none());
        assert_eq!(topics(&store).len(), 4, "--check removed something");
        let places: Vec<&str> = stale.iter().map(|s| s.place.as_str()).collect();
        assert_eq!(places, [format!("store annotation {mine} (deploy)")]);
        assert!(stale[0].fix.as_deref().is_some_and(|f| f.ends_with(&format!("annotate remove {mine}"))));

        let (tidy, _) = tidy_store(&root, &db, false).unwrap();
        let backup = PathBuf::from(tidy.backup.expect("removed without a backup"));
        assert!(backup.starts_with(root.join(".cortex/backups")) && backup.is_file());
        // The person's annotations stay, including one about an MCP tool that
        // was not a seeded copy.
        assert_eq!(topics(&store), ["MCP: recall", "deploy"]);
        let copy = crate::memory::Store::open(&backup).unwrap();
        assert_eq!(topics(&copy).len(), 4, "the backup is not the store as it was");

        let none = tidy_store(&root, &root.join("absent.db"), false).unwrap();
        assert!(none.0.seeded_copies.is_empty() && !root.join("absent.db").exists());
    }
}
