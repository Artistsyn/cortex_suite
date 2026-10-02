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
}
