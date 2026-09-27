//! Knowledge pushed to the agent at the moment it applies -- and only then.
//!
//! Pull retrieval depends on the agent thinking to ask, and it asks least when it
//! is most sure. Two moments carry their own evidence of what is relevant: an
//! edit (the text being written) and a failure (the error being printed). This
//! module decides whether a recorded trap genuinely matches either one, and wraps
//! the answer in the only form a hook can deliver.
//!
//! DELIVERY. Claude Code writes a PostToolUse hook's plain stdout to its debug
//! log; the model never sees it. Only `hookSpecificOutput.additionalContext`
//! reaches the model, and an `mcp_tool` hook's text result is parsed exactly like
//! a command hook's stdout (code.claude.com/docs/en/hooks). Until this module
//! existed, every edit-guard warning and every compacted log was computed,
//! logged, counted on the scoreboard -- and dropped. Verified 2026-09-27: a fired
//! guard (trap #344) left an `edit_guard_fires` row and nothing in the agent's
//! context; 5,810 compacted outputs sat in transcripts as hook attachments while
//! the agent read the full originals.
//!
//! PRECISION. Counting shared words is not evidence of relevance. A 200-line
//! edit shares "state", "render" and "value" with most traps in a 400-entry
//! store, which is how the guard warned about a C# grammar trap while a game's
//! difficulty table was being edited. Evidence now has to be DISTINCTIVE --
//! found in at most a few live traps -- and of the right kind: a code
//! identifier plus one more distinctive token, or three distinctive words, with
//! ordinary English, library identifiers, file locations and prose files set
//! aside. Replayed against 2,403 real edits (the `replay_*` tests), that took
//! the guard from firing on 91% of edits to 32%, and a labelled random sample
//! of its fires from about half relevant to about nine in ten.

use std::collections::BTreeMap;

use crate::memory::Store;
use crate::model::AntiPattern;

/// A word found in at most this many live traps is distinctive.
pub const MAX_DISTINCTIVE_DF: usize = 3;

/// Failure pushes per session. A build that fails ten ways in one session is
/// being worked on; past a handful, more reminders are wallpaper.
pub const FAILURE_PUSH_SESSION_CAP: i64 = 4;

/// Sessions a failure must have reached before an unrecorded one is worth a
/// nudge -- the same bar closeout uses before it proposes a trap for review.
pub const NUDGE_MIN_SESSIONS: i64 = 3;

/// Error-output words that name the KIND of failure, not the failure. They sit
/// in almost every compiler message and would otherwise dominate the overlap.
const FAILURE_STOPWORDS: &[&str] = &[
    "error", "errors", "warning", "warnings", "note", "help", "found", "expected",
    "type", "types", "mismatched", "cannot", "could", "compile", "compiling",
    "failed", "failure", "panicked", "thread", "main", "test", "tests", "result",
    "this", "that", "here", "there", "value", "field", "fields", "method", "struct",
    "function", "named", "scope", "current", "supplied", "argument", "arguments",
    "takes", "with", "from", "into", "have", "been", "left", "right", "assertion",
    "called", "unwrap", "option", "none", "some", "line", "column", "stdout",
    "stderr", "rust", "backtrace", "environment", "variable",
];

/// The best live trap for a query and why it qualified.
pub struct TrapMatch<'a> {
    pub ap: &'a AntiPattern,
    /// Distinct query tokens the trap mentions.
    pub score: usize,
    /// The rare ones among them -- the actual evidence.
    pub distinctive: Vec<String>,
}

/// Words of a text, split and lowercased the way `text_hint_score` splits them.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}

/// Word -> indices of the traps whose description, `wrong` text or tags use it.
/// The same fields the edit guard has always scored against.
fn index(aps: &[AntiPattern]) -> BTreeMap<String, Vec<usize>> {
    let mut idx: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, ap) in aps.iter().enumerate() {
        let hay = format!("{} {} {}", ap.description, ap.wrong, ap.tags.join(" "));
        for w in words(&hay) {
            let ids = idx.entry(w).or_default();
            if ids.last() != Some(&i) {
                ids.push(i);
            }
        }
    }
    idx
}

/// Traps a token hits: an exact word, or -- for tokens of six or more
/// characters -- a longer word it prefixes, so "migration" finds "migrations"
/// but "size" never finds "resize". Identical to `text_hint_score`.
fn hits(idx: &BTreeMap<String, Vec<usize>>, token: &str) -> Vec<usize> {
    let mut v: Vec<usize> = idx.get(token).cloned().unwrap_or_default();
    if token.len() >= 6 {
        for (w, ids) in idx.range(token.to_string()..) {
            if !w.starts_with(token) {
                break;
            }
            if w.len() > token.len() {
                v.extend(ids);
            }
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// Identifier-shaped words of a text, lowercased: `snake_case`/`SCREAMING_CASE`
/// (an underscore between letters) and `CamelCase` (a capital after a
/// lowercase letter). Taken from the ORIGINAL text, because lowercasing is what
/// erases CamelCase.
///
/// Why identifiers get their own standing: replaying all 115 historical guard
/// fires (2026-09-27) showed that a rare word is not the same thing as a
/// meaningful one. Fires carried by `ignore_zoom`, `lose_heart`, `recv_timeout`
/// or `test_outcomes` were almost all on point; fires carried by "sees",
/// "checked", "anyway" or "keep" -- rare in the store, meaningless as evidence --
/// were almost all noise.
pub fn identifier_tokens(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| {
            let letters = w.chars().filter(|c| c.is_alphabetic()).count();
            let snake = w.len() >= 5
                && letters >= 2
                && w.trim_matches('_').contains('_');
            let camel = w.len() >= 6
                && w.chars().zip(w.chars().skip(1)).any(|(a, b)| a.is_lowercase() && b.is_uppercase());
            snake || camel
        })
        .map(|w| w.trim_matches('_').to_lowercase())
        .filter(|w| !GENERIC_IDENTIFIERS.contains(&w.as_str()))
        .collect()
}

/// Identifier-shaped, but in nearly every file of their language, so they are
/// never evidence. Found by the same replay: `to_string`, `assert_eq`,
/// `is_none` and `ends_with` each carried an unrelated trap, because a trap
/// that happens to mention one looked "distinctive" to a store that has only a
/// few. (The durable fix is frequency across observed edits -- see
/// docs/cortex-upgrade-plan-2026-09-27.md -- this list is the floor.)
const GENERIC_IDENTIFIERS: &[&str] = &[
    // Rust std and the crates every file here uses
    "to_string", "to_owned", "to_vec", "to_lowercase", "to_uppercase", "to_string_lossy",
    "as_str", "as_ref", "as_mut", "as_ptr", "as_bytes", "as_slice", "into_iter", "iter_mut",
    "is_none", "is_some", "is_empty", "is_ok", "is_err", "is_some_and", "unwrap_or",
    "unwrap_or_else", "unwrap_or_default", "ok_or", "ok_or_else", "and_then", "map_err",
    "map_or", "map_or_else", "starts_with", "ends_with", "strip_prefix", "strip_suffix",
    "split_once", "split_whitespace", "trim_start", "trim_end", "trim_matches", "push_str",
    "with_capacity", "sort_by", "sort_by_key", "sort_unstable", "max_by_key", "min_by_key",
    "filter_map", "flat_map", "for_each", "take_while", "skip_while", "get_mut", "or_insert",
    "or_default", "or_insert_with", "from_str", "from_utf8", "read_to_string",
    "create_dir_all", "remove_file", "set_var", "var_os", "file_name", "file_stem",
    "to_path_buf", "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq", "cfg_attr",
    "serde_json", "partialeq", "partialord", "hashmap", "hashset", "btreemap", "btreeset",
    "vecdeque", "phantomdata", "tostring", "fromstr", "pathbuf", "bufreader", "bufwriter",
    "rwlock", "oncelock", "systemtime",
    // JS / TS
    "foreach", "indexof", "lastindexof", "addeventlistener", "removeeventlistener",
    "queryselector", "queryselectorall", "getelementbyid", "settimeout", "setinterval",
    "cleartimeout", "requestanimationframe", "usestate", "useeffect", "useref", "usememo",
    "usecallback", "tofixed", "parseint", "parsefloat", "hasownproperty", "preventdefault",
    "stoppropagation", "classname", "innerhtml", "textcontent", "appendchild", "tolowercase",
    "touppercase", "startswith", "endswith", "tojson", "getattribute", "setattribute",
    "getcontext", "getboundingclientrect", "as_deref", "as_deref_mut",
];

/// How much evidence a trap needs before it is pushed unasked.
#[derive(Clone, Copy)]
pub struct Bar {
    /// Distinct shared tokens, when the evidence is distinctive WORDS.
    pub min_score: usize,
    /// Distinct shared tokens, when a distinctive IDENTIFIER is among them.
    pub id_min_score: usize,
    /// Distinctive words needed when no identifier is shared.
    pub min_rare_words: usize,
    /// May one distinctive identifier (9+ characters) qualify on its own?
    pub lone_identifier_ok: bool,
}

/// The edit guard's bar: an identifier plus one more distinctive token, or
/// three distinctive words, plus overlap. A lone identifier is not enough in an
/// edit: a large edit MENTIONS many names (a crate path in a config file, a
/// field in passing), and in a 2,403-edit replay those passing mentions were
/// the guard's largest remaining source of noise.
pub const EDIT_BAR: Bar =
    Bar { min_score: 3, id_min_score: 3, min_rare_words: 3, lone_identifier_ok: false };
/// A failure's bar: an error line is short, and the identifier it names is
/// evidence on its own. Two distinctive WORDS are not: replaying 424 recorded
/// failures, pairs like "dark, lamp" and "mean, ratio" carried half the
/// matches and almost none of the right ones.
pub const FAILURE_BAR: Bar =
    Bar { min_score: 2, id_min_score: 1, min_rare_words: 3, lone_identifier_ok: true };

/// Prose the edit guard does not read. A plan, report or note names many topics
/// at once and so resembles many traps, and a doc edit cannot break a build --
/// the guard exists for code. Configuration (json, toml, yaml) is still read:
/// several traps are about exactly those files.
pub fn is_prose_file(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    path.contains('.')
        && matches!(
            ext.as_str(),
            "md" | "markdown" | "txt" | "html" | "htm" | "rst" | "adoc" | "csv" | "jsonl" | "log"
        )
}

/// The best qualifying trap for `tokens`, skipping ids in `exclude`.
///
/// Only DISTINCTIVE shared tokens (found in at most `MAX_DISTINCTIVE_DF` live
/// traps) count as evidence. A trap qualifies with a distinctive identifier
/// (from `identifiers`) and `bar.id_min_score` overlap, or with
/// `bar.min_rare_words` distinctive words and `bar.min_score` overlap. Ranked by
/// identifiers, then distinctive words, then overlap.
pub fn best_trap<'a>(
    aps: &'a [AntiPattern],
    tokens: &[String],
    identifiers: &std::collections::HashSet<String>,
    exclude: &[i64],
    bar: Bar,
) -> Option<TrapMatch<'a>> {
    if tokens.is_empty() || aps.is_empty() {
        return None;
    }
    let idx = index(aps);
    let mut score = vec![0usize; aps.len()];
    let mut rare: Vec<Vec<String>> = vec![Vec::new(); aps.len()];
    for t in tokens
        .iter()
        .filter(|t| !COMMON_WORDS.contains(&t.as_str()) && !GENERIC_IDENTIFIERS.contains(&t.as_str()))
    {
        let h = hits(&idx, t);
        let distinctive = !h.is_empty() && h.len() <= MAX_DISTINCTIVE_DF;
        for &i in &h {
            score[i] += 1;
            if distinctive {
                rare[i].push(t.clone());
            }
        }
    }
    let ids_of = |i: usize| rare[i].iter().filter(|t| identifiers.contains(*t)).count();
    // An identifier is evidence when another distinctive token agrees with it.
    // Alone it is enough only where the bar allows it (a failure's error line),
    // and only when it is long: a short one (`as_deref`, `setSize`) is usually
    // library vocabulary, while every relevant lone-identifier match in the
    // replay was 9+ characters (`set_checkpoint`, `pool_acquire`, `CuboidDef`).
    let id_evidence = |i: usize| {
        let lone = bar.lone_identifier_ok
            && rare[i].iter().any(|t| identifiers.contains(t) && t.len() >= 9);
        lone || (ids_of(i) >= 1 && rare[i].len() >= 2)
    };
    (0..aps.len())
        .filter(|&i| aps[i].id.is_some_and(|id| !exclude.contains(&id)))
        .filter(|&i| {
            (id_evidence(i) && score[i] >= bar.id_min_score)
                || (rare[i].len() >= bar.min_rare_words && score[i] >= bar.min_score)
        })
        .max_by_key(|&i| (ids_of(i), rare[i].len(), score[i]))
        .map(|i| TrapMatch { ap: &aps[i], score: score[i], distinctive: rare[i].clone() })
}

/// Ordinary English that turns up rare in a trap store and means nothing as
/// evidence -- the words that carried the replay's noise ("keep", "note",
/// "property", "header", "sees", "checked", "anyway"), plus the general
/// vocabulary of prose. Domain nouns that look ordinary but matter here
/// (plane, slot, layer, head, cast, dark) are deliberately NOT on it.
const COMMON_WORDS: &[&str] = &[
    "keep", "keeps", "kept", "note", "notes", "along", "someone", "something", "property",
    "properties", "override", "header", "identity", "opens", "sides", "inputs", "rejects",
    "sees", "checked", "anyway", "older", "newer", "describes", "behaviour", "behavior",
    "enough", "directions", "thing", "things", "many", "walking", "give", "gives", "given",
    "reuse", "fraction", "intend", "intended", "finished", "compared", "genuinely", "roughly",
    "reasonable", "problem", "problems", "whole", "every", "never", "always", "because",
    "should", "would", "could", "which", "where", "while", "there", "their", "these", "those",
    "about", "after", "before", "other", "another", "still", "only", "just", "more", "most",
    "much", "same", "different", "each", "both", "either", "without", "within", "between",
    "through", "during", "until", "since", "again", "already", "also", "even", "ever", "what",
    "does", "doing", "done", "made", "make", "makes", "making", "used", "uses", "using",
    "need", "needs", "needed", "take", "takes", "taken", "come", "comes", "said", "says",
    "look", "looks", "seen", "find", "finds", "found", "know", "known", "think", "work",
    "works", "worked", "right", "wrong", "true", "false", "good", "well", "real", "really",
    "actually", "exactly", "simply", "rather", "instead", "however", "otherwise", "whether",
    "first", "second", "third", "once", "twice", "here", "then", "than", "them", "they",
    "have", "been", "were", "will", "with", "from", "into", "onto", "over", "under", "above",
    "below", "like", "such", "some", "none", "able", "unable", "want", "wants", "means",
    "meant", "matter", "matters", "reason", "reasons", "happen", "happens", "happened",
    "point", "points", "case", "cases", "part", "parts", "kind", "kinds", "sort", "whatever",
    "whichever", "anything", "everything", "nothing", "people", "person", "today", "later",
    "earlier", "ahead", "behind", "around", "across", "toward", "towards", "whose",
];

/// Tokens of a failure's excerpt: identifiers and content words, not the
/// vocabulary every compiler message shares.
pub fn failure_tokens(excerpt: &str) -> Vec<String> {
    let mut v: Vec<String> = words(excerpt)
        .filter(|w| w.len() >= 4 && !FAILURE_STOPWORDS.contains(&w.as_str()))
        // A bare error code (e0308) says which rule broke, not what broke it.
        .filter(|w| !(w.len() == 5 && w.starts_with('e') && w[1..].chars().all(|c| c.is_ascii_digit())))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The words of a failure that say WHAT went wrong -- the assertion or error
/// message -- without its location, the harness's banner, or any path.
///
/// `None` for a compiler error with an E-code. rustc's own diagnostic already
/// names the exact item and usually the fix, and matching trap text against it
/// was measured as mostly noise: replaying 424 recorded failures, the matches
/// were carried by file names (`sim_tests`, `terrain_pipeline`) and path pieces
/// (`users`, `arty`), not by what failed. Those failures are still answered
/// when a trap is LINKED to their signature, and nudged when they recur.
pub fn failure_message(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let message: Vec<&str> = if let Some(i) = lines.iter().position(|l| l.contains("panicked at ")) {
        // `panicked at <path>:<line>:<col>:` then the message on the next
        // lines; older toolchains printed `panicked at '<msg>', <path>` on one
        // line. Keeping the whole remainder serves both: the path filter below
        // drops the location, whichever side of the message it is on.
        let same_line = lines[i].split_once("panicked at ").map(|(_, rest)| rest);
        same_line
            .into_iter()
            .chain(
                lines[i + 1..]
                    .iter()
                    .copied()
                    .take_while(|l| {
                        let t = l.trim_start();
                        !(t.starts_with("note:") || t.starts_with("stack backtrace")
                            || t.starts_with("----") || t.starts_with("failures:"))
                    })
                    .filter(|l| !l.trim().is_empty())
                    .take(2),
            )
            .collect()
    } else if output.contains("error[E") {
        return None;
    } else {
        // Non-rustc failures: the lines that carry an error's own words.
        lines
            .iter()
            .copied()
            .filter(|l| {
                let t = l.trim_start();
                (t.contains("Error:") || t.starts_with("error:") || t.starts_with("Error "))
                    && !t.contains("could not compile")
                    && !t.contains("test failed, to rerun")
            })
            .take(2)
            .collect()
    };
    let cleaned: Vec<String> = message
        .iter()
        .map(|l| {
            l.split_whitespace()
                .filter(|w| !is_path_like(w))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|l| !l.is_empty())
        .collect();
    (!cleaned.is_empty()).then(|| cleaned.join("\n"))
}

/// A chunk of text that is a location rather than words: a path, a module path
/// (a panicking test's thread name), or a file name with a source extension,
/// optionally with `:line:col`.
fn is_path_like(w: &str) -> bool {
    let w = w.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ';'));
    if w.contains('/') || w.contains('\\') || w.contains("::") {
        return true;
    }
    let base = w.split(':').next().unwrap_or(w);
    matches!(
        base.rsplit('.').next().unwrap_or(""),
        "rs" | "js" | "jsx" | "ts" | "tsx" | "py" | "json" | "toml" | "wgsl" | "cpp" | "h" | "c" | "kt" | "java"
    ) && base.contains('.')
}

/// What to tell the agent about a failing build or test, if anything.
pub struct Push {
    /// Per-session de-duplication key.
    pub key: String,
    pub text: String,
    pub anti_pattern_id: Option<i64>,
}

/// Is a failure signature specific enough to be a trap worth recording?
///
/// `rust:E0599:set_glow` names what broke. `rust:E0308:mismatched types@a.rs` is
/// the fallback form for a message with no identifier -- ordinary compile churn
/// that is fixed in seconds and would only teach the store noise.
fn is_specific(sig: &str) -> bool {
    if sig.starts_with("assert:") {
        return true;
    }
    match sig.strip_prefix("rust:").and_then(|r| r.split_once(':')) {
        Some((_code, sym)) => !sym.is_empty() && !sym.contains(' ') && !sym.contains('@'),
        None => false,
    }
}

/// Cut to at most `n` characters on a char boundary.
pub(crate) fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// A recorded trap, phrased for the moment of failure.
fn trap_text(ap: &AntiPattern, how: &str, sig: &str, matched: Option<&[String]>) -> String {
    let id = ap.id.unwrap_or_default();
    let mut s = format!(
        "[cortex] Recorded trap #{id} {how}:\n  {}\n  → {}",
        clip(&ap.description, 300),
        clip(&ap.correct, 420)
    );
    match matched {
        Some(m) => s.push_str(&format!(
            "\n  (matched on: {}; failure `{sig}`. If it does not apply, ignore it.)",
            m.join(", ")
        )),
        None => s.push_str(&format!("\n  (failure `{sig}`)")),
    }
    s
}

/// The trap recorded for a failing build or test, a trap that names what the
/// failure names, or -- for a failure that keeps coming back with nothing
/// recorded -- a nudge to record it. `None` when the store has nothing to add.
pub fn failure_recall(store: &Store, output: &str) -> Option<Push> {
    let sig = crate::test_signal::error_signature(output)?;
    let row: Option<(i64, i64, Option<i64>)> = store
        .conn()
        .query_row(
            "SELECT seen_count, proposed, anti_pattern_id FROM recurring_errors WHERE signature = ?1",
            rusqlite::params![sig],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let aps = store.all_anti_patterns().ok()?;

    // 1. A trap recorded for this exact failure.
    if let Some(id) = row.and_then(|r| r.2) {
        if let Some(ap) = aps.iter().find(|a| a.id == Some(id)) {
            return Some(Push {
                key: sig.clone(),
                text: trap_text(ap, "was recorded for this exact failure", &sig, None),
                anti_pattern_id: Some(id),
            });
        }
    }

    // 2. A trap that names what this failure's MESSAGE names.
    if let Some(message) = failure_message(output) {
        let tokens = failure_tokens(&message);
        if let Some(m) = best_trap(&aps, &tokens, &identifier_tokens(&message), &[], FAILURE_BAR) {
            return Some(Push {
                key: sig.clone(),
                text: trap_text(m.ap, "matches this failure", &sig, Some(&m.distinctive)),
                anti_pattern_id: m.ap.id,
            });
        }
    }

    // 3. Nothing recorded, and it keeps happening.
    if let Some((seen, proposed, _)) = row {
        if seen >= NUDGE_MIN_SESSIONS && proposed == 0 && is_specific(&sig) {
            let quoted = shell_quote(&sig);
            return Some(Push {
                key: sig.clone(),
                text: format!(
                    "[cortex] This failure has now occurred in {seen} separate sessions and no \
                     recorded trap covers it (`{sig}`). Once you know the cause, record it so the \
                     next occurrence is answered with the fix automatically:\n  {}\n  If it is not \
                     a trap: {}",
                    crate::cache::launcher_command(&format!(
                        "anti-pattern add --description \"...\" --wrong \"...\" --correct \"...\" --resolves {quoted}"
                    )),
                    crate::cache::launcher_command(&format!("recurring-dismiss {quoted}")),
                ),
                anti_pattern_id: None,
            });
        }
    }
    None
}

/// Quote a value for the shell the launcher runs in.
fn shell_quote(s: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", s.replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Wrap text in the only form a hook can deliver to the model.
///
/// `event` must name the hook event that runs the tool, or Claude Code rejects
/// the output as a non-blocking error.
pub fn hook_context(event: &str, text: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    })
    .to_string()
}

/// Deliver a hook tool's message: hook JSON by default, plain text when the
/// caller asked for `format: "text"` (a direct MCP call, not a hook).
pub fn deliver(args: &serde_json::Value, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if args.get("format").and_then(|v| v.as_str()) == Some("text") {
        return text.to_string();
    }
    let event = args
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("PostToolUse");
    hook_context(event, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ap(id: i64, description: &str, wrong: &str, correct: &str) -> AntiPattern {
        AntiPattern {
            id: Some(id),
            description: description.into(),
            wrong: wrong.into(),
            correct: correct.into(),
            tags: vec![],
            added_at: chrono::Utc::now(),
            hash: None,
            superseded_by: None,
        }
    }

    /// Five traps that all talk about render state, one that names ignore_zoom.
    fn store_of_traps() -> Vec<AntiPattern> {
        let mut v: Vec<AntiPattern> = (1..=5)
            .map(|i| ap(i, "render state value shader update", "render the state value", "fix it"))
            .collect();
        v.push(ap(
            6,
            "A HUD button does nothing while zoomed because ignore_zoom objects use the base scale",
            "hit-test an ignore_zoom object against the raw pointer position",
            "convert the pointer with screen_to_virtual before testing ignore_zoom objects",
        ));
        v
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn common_words_alone_never_qualify_a_trap() {
        let aps = store_of_traps();
        let tokens = strs(&["render", "state", "value", "shader", "update"]);
        // Five shared words, but every one of them is in five traps: no evidence.
        assert!(best_trap(&aps, &tokens, &HashSet::new(), &[], EDIT_BAR).is_none());
    }

    #[test]
    fn rare_but_ordinary_words_are_not_evidence_either() {
        // The replay's noise: "sees" and "checked" were each in one trap and
        // carried unrelated warnings. Two rare WORDS are below the bar; an
        // identifier or three distinctive words are not.
        let aps = vec![
            ap(1, "the copilot host sees no skill path", "assume it sees skills", "publish twice"),
            ap(2, "a server that exits when nothing is checked", "exit if unchecked", "degrade"),
            ap(3, "render state", "render state", "x"),
        ];
        let tokens = strs(&["sees", "checked", "render", "state"]);
        assert!(best_trap(&aps, &tokens, &HashSet::new(), &[], EDIT_BAR).is_none());
    }

    #[test]
    fn a_distinctive_identifier_plus_overlap_qualifies_and_names_its_evidence() {
        let aps = store_of_traps();
        let text = "fn press() { if hit(ignore_zoom) { /* button while zoomed */ } render(); }";
        let tokens = strs(&["ignore_zoom", "button", "zoomed", "render"]);
        let m = best_trap(&aps, &tokens, &identifier_tokens(text), &[], EDIT_BAR)
            .expect("the zoom trap should match");
        assert_eq!(m.ap.id, Some(6));
        assert!(m.distinctive.contains(&"ignore_zoom".to_string()));
    }

    #[test]
    fn an_excluded_trap_is_never_returned() {
        let aps = store_of_traps();
        let tokens = strs(&["ignore_zoom", "button", "zoomed"]);
        let ids = identifier_tokens("ignore_zoom");
        assert!(best_trap(&aps, &tokens, &ids, &[6], EDIT_BAR).is_none());
    }

    #[test]
    fn one_distinctive_identifier_is_enough_for_a_failure_not_for_an_edit() {
        let aps = vec![
            ap(1, "cross-compiling dies with ANDROID_NDK_ROOT unset in a non-interactive shell", "run the build", "source the env file"),
            ap(2, "unrelated", "unrelated", "unrelated"),
        ];
        let excerpt = r#"environment variable "ANDROID_NDK_ROOT" has not been set: NotPresent"#;
        let tokens = failure_tokens(excerpt);
        let ids = identifier_tokens(excerpt);
        assert!(tokens.contains(&"android_ndk_root".to_string()), "{tokens:?}");
        assert!(best_trap(&aps, &tokens, &ids, &[], EDIT_BAR).is_none(), "one token is below an edit's bar");
        let m = best_trap(&aps, &tokens, &ids, &[], FAILURE_BAR).expect("an identifier is evidence for a failure");
        assert_eq!(m.ap.id, Some(1));
    }

    #[test]
    fn identifiers_are_read_from_the_original_case() {
        let ids = identifier_tokens("CanvasLayout lays out ignore_zoom; ANDROID_NDK_ROOT unset; plain words; __init__ x_y");
        for want in ["canvaslayout", "ignore_zoom", "android_ndk_root"] {
            assert!(ids.contains(want), "{want} missing from {ids:?}");
        }
        for not in ["plain", "words", "init", "x_y"] {
            assert!(!ids.contains(not), "{not} is not an identifier: {ids:?}");
        }
        // Identifier-shaped but in every file of the language: never evidence.
        let generic = identifier_tokens("x.to_string(); assert_eq!(a, b); if v.is_none() {} HashMap::new()");
        assert!(generic.is_empty(), "{generic:?}");
    }

    /// Replay every recorded failure through the trap matcher, on a COPY of a
    /// store. Evaluation, not a regression test:
    ///
    ///   CORTEX_REPLAY_DB=copy.db cargo test replay_failure_recall -- --ignored --nocapture
    #[test]
    #[ignore]
    fn replay_failure_recall_against_recorded_failures() {
        let Ok(db) = std::env::var("CORTEX_REPLAY_DB") else {
            eprintln!("set CORTEX_REPLAY_DB");
            return;
        };
        let store = Store::open(std::path::Path::new(&db)).unwrap();
        let aps = store.all_anti_patterns().unwrap();
        let rows: Vec<(String, String, i64)> = store
            .conn()
            .prepare("SELECT signature, sample, json_array_length(sessions) FROM recurring_errors")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut matched = 0;
        for (sig, sample, sessions) in &rows {
            let Some(message) = failure_message(sample) else { continue };
            let tokens = failure_tokens(&message);
            if let Some(m) = best_trap(&aps, &tokens, &identifier_tokens(&message), &[], FAILURE_BAR) {
                matched += 1;
                let first = sample.lines().next().unwrap_or("").chars().take(110).collect::<String>();
                println!(
                    "{sessions}s\t{sig}\n\t{first}\n\t-> #{} [{}] {}",
                    m.ap.id.unwrap_or(0),
                    m.distinctive.join(", "),
                    m.ap.description.chars().take(100).collect::<String>()
                );
            }
        }
        println!("{matched} of {} recorded failures matched a trap", rows.len());
    }

    #[test]
    fn a_failure_is_read_by_its_message_not_its_location() {
        let panic = "thread 'sim_tests::hooks_stay_in_reach' panicked at src/sim_tests.rs:63:13:\n\
                     hooks too far apart: 812 px\n\
                     note: run with `RUST_BACKTRACE=1` environment variable";
        let m = failure_message(panic).unwrap();
        assert!(m.contains("hooks too far apart"), "{m}");
        assert!(!m.contains("sim_tests"), "the location is not evidence: {m}");

        let build = "thread 'main' panicked at /Users/u/.cargo/registry/physx-sys-0.11/build.rs:40:9:\n\
                     environment variable \"ANDROID_NDK_ROOT\" has not been set: NotPresent";
        let m = failure_message(build).unwrap();
        assert!(m.contains("ANDROID_NDK_ROOT") && !m.contains("build.rs") && !m.contains("/Users"), "{m}");

        let old = "thread 'main' panicked at 'shield band leaves no way in', src/boss.rs:10:5";
        let m = failure_message(old).unwrap();
        assert!(m.contains("shield band leaves no way in") && !m.contains("boss.rs"), "{m}");

        let rustc = "error[E0425]: cannot find value `FLARE_INTERVAL` in this scope\n --> src/solar.rs:12:5";
        assert!(failure_message(rustc).is_none(), "rustc already names the item");

        let vitest = " FAIL  src/brush.test.js > winding\nAssertionError: expected 'cw' to be 'ccw'";
        let m = failure_message(vitest).unwrap();
        assert!(m.contains("expected 'cw' to be 'ccw'") && !m.contains("brush.test.js"), "{m}");
    }

    #[test]
    fn prose_is_skipped_code_and_config_are_read() {
        for p in ["docs/plan.md", "REPORT.HTML", "notes.txt", "trace.jsonl"] {
            assert!(is_prose_file(p), "{p}");
        }
        for p in ["src/main.rs", "app.jsx", "index-sources.json", "Cargo.toml", "shader.wgsl", "Makefile"] {
            assert!(!is_prose_file(p), "{p}");
        }
    }

    #[test]
    fn compiler_vocabulary_is_not_a_token() {
        let t = failure_tokens("error[E0308]: mismatched types expected `f32`, found `i32` in this function");
        assert!(t.iter().all(|w| !["error", "mismatched", "types", "expected", "found", "e0308"].contains(&w.as_str())), "{t:?}");
    }

    #[test]
    fn only_specific_signatures_earn_a_nudge() {
        assert!(is_specific("rust:E0599:set_glow"));
        assert!(is_specific("assert:environment variable ANDROID_NDK_ROOT has not been set@build.rs"));
        assert!(!is_specific("rust:E0308:mismatched types@lightmap.rs"));
        assert!(!is_specific("rust:E0308"));
    }

    #[test]
    fn hook_json_is_the_documented_shape_and_text_is_opt_in() {
        let out = deliver(&serde_json::json!({}), "careful");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "careful");
        assert!(out.starts_with('{') && out.ends_with('}'), "Claude Code parses JSON only when it is the whole output");
        assert_eq!(deliver(&serde_json::json!({"format": "text"}), "careful"), "careful");
        assert_eq!(deliver(&serde_json::json!({}), ""), "", "silence stays silent");
        let pre = deliver(&serde_json::json!({"hook_event_name": "PreToolUse"}), "x");
        assert!(pre.contains("\"PreToolUse\""));
    }

    #[test]
    fn clip_never_splits_a_character() {
        let s = "é".repeat(500);
        let c = clip(&s, 300);
        assert_eq!(c.chars().count(), 301);
    }

    fn temp(name: &str) -> crate::test_support::TempStore {
        crate::test_support::TempStore::new(name).unwrap()
    }

    #[test]
    fn a_linked_trap_answers_its_own_failure_exactly() {
        let store = temp("push_linked");
        let (id, _) = store
            .insert_anti_pattern_checked(&ap(0, "set_glow allocates a fresh image per call", "call set_glow every frame", "cache the glow image"))
            .unwrap();
        let out = "error[E0599]: no method named `set_glow` found for struct `Thing`\n --> src/a.rs:3:9";
        let sig = crate::test_signal::error_signature(out).unwrap();
        crate::test_signal::note_failure(&store, "s1", "cargo build", out).unwrap();
        assert!(crate::test_signal::mark_recurring_handled(&store, &sig, Some(id)).unwrap());
        let p = failure_recall(&store, out).expect("a linked trap must be recalled");
        assert_eq!(p.anti_pattern_id, Some(id));
        assert!(p.text.contains("exact failure"), "{}", p.text);
    }

    #[test]
    fn an_unrecorded_repeat_is_nudged_only_at_the_bar() {
        let store = temp("push_nudge");
        let out = "error[E0599]: no method named `frobnicate_widget` found for struct `Thing`\n --> src/a.rs:3:9";
        for s in ["s1", "s2"] {
            crate::test_signal::note_failure(&store, s, "cargo build", out).unwrap();
        }
        assert!(failure_recall(&store, out).is_none(), "two sessions is below the bar");
        crate::test_signal::note_failure(&store, "s3", "cargo build", out).unwrap();
        let p = failure_recall(&store, out).expect("three sessions and nothing recorded");
        assert!(p.anti_pattern_id.is_none());
        assert!(p.text.contains("--resolves"), "{}", p.text);
    }
}
