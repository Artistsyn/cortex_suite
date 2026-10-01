//! What a compaction must not lose, put back from the transcript.
//!
//! A compaction replaces the conversation with a summary the model writes, and
//! a summary blurs exactly the things work resumes from: which files were being
//! edited, the error still open (verbatim, not paraphrased), what already went
//! green, and what the agent said it would do next. Re-acquiring them is the
//! cost that grows when compaction comes earlier: measured here, the 30 calls
//! after a compaction grew 2.58k tokens each against 1.59k elsewhere, and a
//! study of compressed agents found retrieval calls tripling (21 -> 64).
//!
//! The transcript keeps everything from before the compaction boundary, so a
//! SessionStart hook with the `compact` matcher can read that state back
//! mechanically - no model in the loop - and hand it to the agent in one
//! bounded block. The same state is stored as a checkpoint, so
//! `get_checkpoint(scope="any")` finds it after a restart.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

/// Most characters handed back: ~1.5k tokens, under the host's 10,000-char cap
/// on a hook's additionalContext.
pub const BUDGET: usize = 6_000;

/// The state of one segment of a session - the stretch a compaction just
/// summarised.
#[derive(Debug, Default, PartialEq)]
pub struct State {
    /// The user's requests, oldest first (the last few).
    pub requests: Vec<String>,
    /// Files edited, most recent first, with how many edits.
    pub edited: Vec<(String, usize)>,
    /// Files read and not edited, most recent first.
    pub read: Vec<String>,
    /// Build/test/run commands whose last run succeeded, most recent first.
    pub green: Vec<String>,
    /// Commands whose last run failed: (command, the end of its output).
    pub failing: Vec<(String, String)>,
    /// The agent's last words before the compaction.
    pub last_note: String,
}

impl State {
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty() && self.edited.is_empty() && self.failing.is_empty() && self.last_note.is_empty()
    }
}

fn is_build_command(cmd: &str) -> bool {
    const PAIRS: &[(&str, &[&str])] = &[
        ("cargo", &["build", "test", "check", "clippy", "run", "nextest"]),
        ("npm", &["test", "run"]),
        ("pnpm", &["test", "run"]),
        ("yarn", &["test", "build"]),
        ("go", &["test", "build"]),
        ("swift", &["build", "test"]),
        ("deno", &["test"]),
    ];
    const SINGLE: &[&str] = &["pytest", "make", "ninja", "xcodebuild", "gradle", "gradlew", "./gradlew", "tsc"];
    let toks: Vec<&str> = cmd
        .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
        .filter(|t| !t.is_empty())
        .collect();
    toks.windows(2).any(|w| PAIRS.iter().any(|(a, bs)| w[0] == *a && bs.contains(&w[1])))
        || toks.iter().any(|t| SINGLE.contains(t) || t.ends_with("run.sh") || t.ends_with("bench.py"))
}

/// The command without a leading `cd somewhere &&`, which varies between runs
/// of the same command.
fn normalise_command(cmd: &str) -> String {
    // The first line is the command; what follows a heredoc is its script,
    // whose text (`cargo build` inside a Python string) is not a run of it.
    let mut c = cmd.trim().lines().next().unwrap_or("").trim();
    while let Some(rest) = c.strip_prefix("cd ") {
        match rest.find("&&") {
            Some(i) => c = rest[i + 2..].trim_start(),
            None => break,
        }
    }
    c.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A user message as typed, or None for tool results, meta messages, the
/// compaction summary itself, and text that is only system reminders.
fn typed_request(entry: &Value) -> Option<String> {
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        || entry.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let content = entry.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            if parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result")) {
                return None;
            }
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    // Drop injected blocks; keep what the person wrote.
    let mut kept = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("<system-reminder>") || t.starts_with("<command-") || t.starts_with("<local-command") {
            skipping = !(t.contains("</system-reminder>") || t.contains("</command-") || t.contains("</local-command"));
            continue;
        }
        if skipping {
            if t.contains("</system-reminder>") || t.contains("</command-") || t.contains("</local-command") {
                skipping = false;
            }
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    let kept = kept.trim().to_string();
    (!kept.is_empty()).then_some(kept)
}

/// The end of a command's output: its last `lines` non-empty lines, ANSI codes
/// removed. Errors are reported at the end.
fn tail(output: &str, lines: usize, chars: usize) -> String {
    // Strip ANSI escapes: ESC [ ... final byte in '@'..='~'.
    let mut clean = String::with_capacity(output.len());
    let mut it = output.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if ('@'..='~').contains(&d) {
                    break;
                }
            }
            continue;
        }
        clean.push(c);
    }
    let kept: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = kept.len().saturating_sub(lines);
    let mut s = kept[start..].join("\n");
    if s.len() > chars {
        let mut cut = s.len() - chars;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("…{}", &s[cut..]);
    }
    s
}

/// Read a transcript and return the state of the segment the latest
/// compaction summarised - between the boundary before it and its own - or,
/// when the newest boundary has not been written yet, of everything after the
/// last one.
pub fn from_transcript(path: &Path) -> Result<State> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut entries: Vec<Value> = Vec::new();
    let mut boundaries: Vec<usize> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        // Cheap filter: only entries this module reads are parsed.
        if !(line.contains("\"compact_boundary\"")
            || line.contains("\"type\":\"user\"")
            || line.contains("\"type\":\"assistant\""))
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            boundaries.push(entries.len());
        }
        entries.push(v);
    }
    // When the hook runs after the boundary is written, the newest boundary is
    // followed only by its summary; the segment is the stretch before it.
    let (from, to) = match boundaries.as_slice() {
        [] => (0, entries.len()),
        [.., last] if entries.len() - last <= 4 => {
            let prev = boundaries.iter().rev().nth(1).copied().unwrap_or(0);
            (prev, *last)
        }
        [.., last] => (*last, entries.len()),
    };
    Ok(state_of(&entries[from..to]))
}

pub fn state_of(entries: &[Value]) -> State {
    let mut st = State::default();
    let mut commands: HashMap<String, String> = HashMap::new(); // tool_use id -> command
    let mut last_run: Vec<(String, bool, String)> = Vec::new(); // (normalised command, ok, output), in order
    let mut edits: Vec<String> = Vec::new();
    let mut reads: Vec<String> = Vec::new();
    for e in entries {
        let role = e.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(content) = e.get("message").and_then(|m| m.get("content")) else { continue };
        if role == "user" {
            if let Some(req) = typed_request(e) {
                st.requests.push(req);
            }
            if let Value::Array(parts) = content {
                for p in parts {
                    if p.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = p.get("tool_use_id").and_then(Value::as_str) else { continue };
                    let Some(cmd) = commands.remove(id) else { continue };
                    let failed = p.get("is_error").and_then(Value::as_bool) == Some(true);
                    let out = text_of(p.get("content").unwrap_or(&Value::Null));
                    last_run.push((cmd, !failed, out));
                }
            }
        } else if role == "assistant" {
            if let Value::Array(parts) = content {
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = p.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !t.is_empty() {
                                st.last_note = t.to_string();
                            }
                        }
                        Some("tool_use") => {
                            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                            let input = p.get("input").cloned().unwrap_or(Value::Null);
                            let file = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(Value::as_str);
                            match (name, file) {
                                ("Edit" | "Write" | "MultiEdit" | "NotebookEdit", Some(f)) => edits.push(f.to_string()),
                                ("Read", Some(f)) => reads.push(f.to_string()),
                                ("Bash", _) => {
                                    if let (Some(id), Some(cmd)) =
                                        (p.get("id").and_then(Value::as_str), input.get("command").and_then(Value::as_str))
                                    {
                                        commands.insert(id.to_string(), normalise_command(cmd));
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Edited files, most recent first, with counts.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for f in &edits {
        *counts.entry(f.as_str()).or_default() += 1;
    }
    for f in edits.iter().rev() {
        if !st.edited.iter().any(|(g, _)| g == f) {
            st.edited.push((f.clone(), counts[f.as_str()]));
        }
    }
    for f in reads.iter().rev() {
        if !st.edited.iter().any(|(g, _)| g == f) && !st.read.contains(f) {
            st.read.push(f.clone());
        }
    }

    // Each command's LAST run decides: green, or still failing.
    let mut seen: Vec<&str> = Vec::new();
    let recent_window = last_run.len().saturating_sub(12);
    for (i, (cmd, ok, out)) in last_run.iter().enumerate().rev() {
        if seen.contains(&cmd.as_str()) {
            continue;
        }
        seen.push(cmd);
        let build = is_build_command(cmd);
        if *ok && build {
            st.green.push(cmd.clone());
        } else if !*ok && (build || i >= recent_window) {
            st.failing.push((cmd.clone(), tail(out, 14, 1_200)));
        }
    }
    st.green.truncate(5);
    st.failing.truncate(2);
    let keep = st.requests.len().saturating_sub(3);
    st.requests.drain(..keep);
    st
}

fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() <= n {
        return s.to_string();
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{} …", &s[..i])
}

fn short_path(p: &str, root: Option<&Path>) -> String {
    match root.and_then(|r| Path::new(p).strip_prefix(r).ok()) {
        Some(rel) => rel.display().to_string(),
        None => p.to_string(),
    }
}

/// The block handed back after a compaction, within `budget` characters.
pub fn render(st: &State, root: Option<&Path>, budget: usize) -> String {
    let mut out = String::from(
        "[cortex] State before the compaction, read from the transcript (quoted text is verbatim). \
         It supplements the summary; trust the files on disk over it.\n",
    );
    if !st.requests.is_empty() {
        out.push_str("Latest requests (oldest first):\n");
        for r in &st.requests {
            out.push_str(&format!("  > {}\n", cut(&r.replace('\n', " "), 700)));
        }
    }
    if !st.edited.is_empty() {
        let files: Vec<String> = st
            .edited
            .iter()
            .take(15)
            .map(|(f, n)| if *n > 1 { format!("{} (×{n})", short_path(f, root)) } else { short_path(f, root) })
            .collect();
        out.push_str(&format!("Files edited, most recent first: {}\n", files.join(", ")));
    }
    if !st.read.is_empty() {
        let files: Vec<String> = st.read.iter().take(8).map(|f| short_path(f, root)).collect();
        out.push_str(&format!("Also read: {}\n", files.join(", ")));
    }
    if !st.green.is_empty() {
        let cmds: Vec<String> = st.green.iter().map(|c| format!("`{}`", cut(c, 140))).collect();
        out.push_str(&format!("Last run green: {}\n", cmds.join(", ")));
    }
    for (cmd, err) in &st.failing {
        out.push_str(&format!("Still failing at the compaction: `{}`\n", cut(cmd, 200)));
        for l in err.lines() {
            out.push_str(&format!("    {l}\n"));
        }
    }
    if !st.last_note.is_empty() {
        let room = budget.saturating_sub(out.len() + 60).min(900);
        if room > 120 {
            out.push_str(&format!("Your last words before it: {}\n", cut(&st.last_note.replace('\n', " "), room)));
        }
    }
    if out.len() > budget {
        let mut i = budget;
        while !out.is_char_boundary(i) {
            i -= 1;
        }
        out.truncate(i);
        out.push_str(" …\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": text}})
    }
    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
    }
    fn result(id: &str, text: &str, error: bool) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": text, "is_error": error}]}})
    }
    fn say(text: &str) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
    }

    #[test]
    fn the_last_run_of_each_command_decides_green_or_failing() {
        let st = state_of(&[
            user("make the probe bake handle pillars"),
            tool_use("1", "Bash", json!({"command": "cd /w && cargo test -p bake"})),
            result("1", "error[E0425]: cannot find value `x`\n  --> src/a.rs:3:5\nerror: could not compile", true),
            tool_use("2", "Edit", json!({"file_path": "/w/src/a.rs"})),
            result("2", "ok", false),
            tool_use("3", "Bash", json!({"command": "cargo test -p bake"})),
            result("3", "test result: ok. 4 passed", false),
            tool_use("4", "Bash", json!({"command": "cargo build --release"})),
            result("4", "error: linking with `cc` failed", true),
            say("Next I will fix the linker flags."),
        ]);
        assert_eq!(st.green, vec!["cargo test -p bake".to_string()], "a failure followed by a pass is green");
        assert_eq!(st.failing.len(), 1);
        assert_eq!(st.failing[0].0, "cargo build --release");
        assert!(st.failing[0].1.contains("linking with `cc` failed"));
        assert_eq!(st.edited, vec![("/w/src/a.rs".to_string(), 1)]);
        assert_eq!(st.requests, vec!["make the probe bake handle pillars".to_string()]);
        assert_eq!(st.last_note, "Next I will fix the linker flags.");
    }

    #[test]
    fn build_commands_are_recognised_and_ansi_is_stripped() {
        assert!(is_build_command("cargo test -p bake --release"));
        assert!(is_build_command("WANT_DEPLOY=1 ./run.sh"));
        assert!(!is_build_command("grep -rn cargo src"));
        assert!(!is_build_command("git commit -m 'make it work'"));
        assert!(!is_build_command(&normalise_command("python3 - <<'EOF'\nrun(['cargo', 'build'])\nprint('cargo build')\nEOF")));
        assert_eq!(normalise_command("cd /w && cd sub &&  cargo   test -p x"), "cargo test -p x");
        assert_eq!(tail("\x1b[31merror\x1b[0m: boom\n\nnext", 5, 100), "error: boom\nnext");
    }

    #[test]
    fn injected_blocks_are_not_requests() {
        let st = state_of(&[
            user("<system-reminder>\nsomething injected\n</system-reminder>"),
            json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "This session is being continued..."}}),
            user("<system-reminder>x</system-reminder>\nkeep the doorway portals"),
        ]);
        assert_eq!(st.requests, vec!["keep the doorway portals".to_string()]);
    }

    #[test]
    fn the_segment_is_the_stretch_the_newest_boundary_summarised() {
        let dir = std::env::temp_dir().join(format!("cortex-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            user("old request, summarised by the first compaction"),
            json!({"type": "system", "subtype": "compact_boundary"}),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "summary 1"}}),
            user("the current task"),
            tool_use("9", "Write", json!({"file_path": "/w/new.rs"})),
            json!({"type": "system", "subtype": "compact_boundary"}),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "summary 2"}}),
        ];
        std::fs::write(&p, lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let st = from_transcript(&p).unwrap();
        assert_eq!(st.requests, vec!["the current task".to_string()]);
        assert_eq!(st.edited, vec![("/w/new.rs".to_string(), 1)]);
        let text = render(&st, Some(Path::new("/w")), BUDGET);
        assert!(text.contains("Files edited, most recent first: new.rs"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_block_stays_within_its_budget() {
        let long = "x".repeat(5_000);
        let mut entries = vec![user(&long), user(&long), user(&long)];
        for i in 0..40 {
            entries.push(tool_use(&i.to_string(), "Edit", json!({"file_path": format!("/w/f{i}.rs")})));
        }
        entries.push(say(&long));
        let text = render(&state_of(&entries), None, BUDGET);
        assert!(text.len() <= BUDGET + 4, "{} chars", text.len());
    }
}
