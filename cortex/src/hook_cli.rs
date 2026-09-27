//! `cortex hook <event>`: cortex's hooks as a plain command.
//!
//! Claude Code can call an MCP tool from a hook (`mcp_tool`), and that is how
//! cortex's hooks run there. VS Code's agent hooks (Preview) cannot: they run
//! `command` hooks only, have no PostToolUseFailure event, and ignore matchers
//! in Claude-format files. This entrypoint gives any host that can run a
//! command the same pushes. It reads the hook's JSON on stdin, works out what
//! happened, runs the SAME tool functions the MCP server runs (same caps,
//! dedupe, redaction), and prints the one reply both hosts show the model:
//!
//!   {"hookSpecificOutput":{"hookEventName":"<event>","additionalContext":"..."}}
//!
//! or nothing. It always exits 0, because a hook must never block the agent,
//! and it never creates a database: a fresh store prints a first-run banner,
//! and any stdout that is not the reply breaks the host's parsing.
//!
//! Payloads, as captured from a real session on 2026-09-27 (upgrade plan,
//! tranche 2 item 12):
//!   terminal  Claude `Bash` {command} with tool_response {stdout, stderr}, or
//!             `error` on PostToolUseFailure; VS Code `run_in_terminal`
//!             {command} with tool_response as ONE string and no exit code.
//!             A failure is read from the output text, never from a flag:
//!             Copilot marks failed commands `success` too.
//!   edits     Claude Edit / Write / MultiEdit / NotebookEdit; VS Code
//!             replace_string_in_file, multi_replace_string_in_file,
//!             create_file, apply_patch, edit_notebook_file,
//!             insert_edit_into_file.
//!   prompts   UserPromptSubmit {prompt}.
//! Everything else -- read_file, grep_search and the rest, most of an agent's
//! calls -- is recognised from the payload alone, and the process exits before
//! the store is opened.
//!
//! Hooks arrive CONCURRENTLY in VS Code (one call's PostToolUse overlaps the
//! next call's PreToolUse). Each invocation is its own process on a WAL store
//! with a busy timeout, which is what makes that safe.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::memory::Store;

/// Which host sent the payload. Only used to namespace session keys, so a
/// VS Code chat and a Claude Code session can never share one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Host {
    Claude,
    VsCode,
    Unknown,
}

/// What a hook call is about, once the host's shape is set aside.
#[derive(Debug, PartialEq)]
pub enum Work {
    Terminal { command: String, output: String },
    /// (file, text the edit adds), one per file touched.
    Edits(Vec<(String, String)>),
    Prompt(String),
    Nothing,
}

const CLAUDE_TOOLS: &[&str] = &["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"];
const VSCODE_TOOLS: &[&str] = &[
    "run_in_terminal",
    "replace_string_in_file",
    "multi_replace_string_in_file",
    "create_file",
    "apply_patch",
    "edit_notebook_file",
    "insert_edit_into_file",
];

/// Parse the payload. Raw control characters inside strings (terminal output
/// can carry them) make strict JSON fail, so retry with them blanked out --
/// structure is unaffected, and the failure markers in the text survive.
pub fn parse(stdin: &str) -> Option<Value> {
    serde_json::from_str(stdin).ok().or_else(|| {
        let cleaned: String =
            stdin.chars().map(|c| if (c as u32) < 0x20 { ' ' } else { c }).collect();
        serde_json::from_str(&cleaned).ok()
    })
}

pub fn host_of(p: &Value) -> Host {
    let transcript = str_at(p, "transcript_path");
    let tool = str_at(p, "tool_name");
    if transcript.contains("copilot") || VSCODE_TOOLS.contains(&tool.as_str()) {
        Host::VsCode
    } else if transcript.contains(".claude") || CLAUDE_TOOLS.contains(&tool.as_str()) {
        Host::Claude
    } else {
        Host::Unknown
    }
}

/// `vscode:<session>`, `claude:<session>` or `hook:<session>`.
pub fn session_key(p: &Value) -> String {
    let host = match host_of(p) {
        Host::VsCode => "vscode",
        Host::Claude => "claude",
        Host::Unknown => "hook",
    };
    let id = str_at(p, "session_id");
    format!("{host}:{}", if id.is_empty() { "unknown" } else { &id })
}

/// The event, from the command line or the payload, in the PascalCase every
/// host accepts back (`postToolUse` -> `PostToolUse`).
pub fn event_name(arg: Option<&str>, p: &Value) -> String {
    let raw = arg
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| Some(str_at(p, "hook_event_name")).filter(|s| !s.is_empty()))
        .or_else(|| Some(str_at(p, "hookEventName")).filter(|s| !s.is_empty()))
        .unwrap_or_default();
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The text a terminal call produced, whichever host shape it came in.
fn terminal_output(p: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    match p.get("tool_response") {
        Some(Value::String(s)) if !s.is_empty() => parts.push(s.clone()),
        Some(Value::Object(o)) => {
            for k in ["stdout", "stderr", "output"] {
                if let Some(Value::String(s)) = o.get(k) {
                    if !s.is_empty() {
                        parts.push(s.clone());
                    }
                }
            }
        }
        _ => {}
    }
    let error = str_at(p, "error");
    if !error.is_empty() {
        parts.push(error);
    }
    parts.join("\n")
}

/// Files and added lines of a V4A patch (`*** Update File: <path>` headers,
/// `+` lines), which is what VS Code's `apply_patch` sends.
fn patch_edits(patch: &str) -> Vec<(String, String)> {
    let mut edits: Vec<(String, String)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in patch.lines() {
        let header = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "));
        if let Some(path) = header {
            edits.push((path.trim().to_string(), String::new()));
            current = Some(edits.len() - 1);
        } else if line.starts_with("*** ") {
            current = None; // Delete File, Move to, End Patch
        } else if let (Some(i), Some(added)) = (current, line.strip_prefix('+')) {
            edits[i].1.push_str(added);
            edits[i].1.push('\n');
        }
    }
    edits
}

/// What this hook call is about, from its event and payload alone.
pub fn work(event: &str, p: &Value) -> Work {
    match event {
        "UserPromptSubmit" => {
            let prompt = str_at(p, "prompt");
            return if prompt.is_empty() { Work::Nothing } else { Work::Prompt(prompt) };
        }
        "PostToolUse" | "PostToolUseFailure" => {}
        _ => return Work::Nothing,
    }
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let s = |k: &str| str_at(&input, k);
    let one = |file: String, added: String| Work::Edits(vec![(file, added)]);
    let edits = match str_at(p, "tool_name").as_str() {
        "Bash" | "run_in_terminal" => {
            let (command, output) = (s("command"), terminal_output(p));
            return if command.is_empty() && output.is_empty() {
                Work::Nothing
            } else {
                Work::Terminal { command, output }
            };
        }
        "Edit" => one(s("file_path"), s("new_string")),
        "Write" => one(s("file_path"), s("content")),
        "NotebookEdit" => one(s("notebook_path"), s("new_source")),
        "MultiEdit" => {
            let added = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|es| es.iter().map(|e| str_at(e, "new_string")).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            one(s("file_path"), added)
        }
        "replace_string_in_file" => one(s("filePath"), s("newString")),
        "create_file" => one(s("filePath"), s("content")),
        "edit_notebook_file" => one(s("filePath"), s("newCode")),
        "insert_edit_into_file" => one(s("filePath"), s("code")),
        "multi_replace_string_in_file" => {
            let mut by_file: BTreeMap<String, String> = BTreeMap::new();
            for r in input.get("replacements").and_then(Value::as_array).into_iter().flatten() {
                let text = by_file.entry(str_at(r, "filePath")).or_default();
                text.push_str(&str_at(r, "newString"));
                text.push('\n');
            }
            Work::Edits(by_file.into_iter().collect())
        }
        "apply_patch" => Work::Edits(patch_edits(&s("input"))),
        _ => Work::Nothing,
    };
    match edits {
        Work::Edits(v) => {
            let v: Vec<_> = v.into_iter().filter(|(_, added)| !added.trim().is_empty()).collect();
            if v.is_empty() { Work::Nothing } else { Work::Edits(v) }
        }
        other => other,
    }
}

/// Handle one hook call; returns exactly what to print (the reply, or "").
pub fn run(event_arg: Option<&str>, stdin: &str, db_path: &Path) -> String {
    let Some(p) = parse(stdin) else { return String::new() };
    let event = event_name(event_arg, &p);
    let work = work(&event, &p);
    // Decided from the payload alone: most calls end here, store untouched.
    if work == Work::Nothing || !db_path.exists() {
        return String::new();
    }
    let Ok(store) = Store::open(db_path) else { return String::new() };
    let session = session_key(&p);
    let repo_root = db_path.parent().and_then(Path::parent).unwrap_or(Path::new("."));
    let call = |tool: &str, args: Value| {
        crate::mcp::tools::run_hook_tool(tool, &args, &store, &session, repo_root).unwrap_or_default()
    };
    let texts: Vec<String> = match work {
        Work::Terminal { command, output } => {
            vec![call("compact_output", json!({ "command": command, "output": output, "format": "text" }))]
        }
        Work::Edits(edits) => edits
            .into_iter()
            .map(|(file, added)| {
                call("edit_guard", json!({ "file_path": file, "added": added, "format": "text" }))
            })
            .collect(),
        Work::Prompt(prompt) => vec![call("note_challenge", json!({ "prompt": prompt }))],
        Work::Nothing => Vec::new(),
    };
    let text = texts.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
    // Proof the entrypoint RAN, separate from whether it found anything.
    let _ = crate::corrections::beat_named(&store, "cli_hook", !text.is_empty());
    if text.is_empty() { String::new() } else { crate::push::hook_context(&event, &text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: Value) -> Value {
        v
    }

    #[test]
    fn vscode_terminal_payload_is_read_as_captured() {
        let v = p(json!({
            "hook_event_name": "PostToolUse", "session_id": "0a1b", "tool_name": "run_in_terminal",
            "transcript_path": "/x/GitHub.copilot-chat/transcripts/0a1b.jsonl",
            "tool_input": {"command": "cargo build", "explanation": "e", "goal": "g", "mode": "sync"},
            "tool_response": "user@mac % cargo build\nerror[E0425]: cannot find value `x`\n"
        }));
        assert_eq!(host_of(&v), Host::VsCode);
        assert_eq!(session_key(&v), "vscode:0a1b");
        match work("PostToolUse", &v) {
            Work::Terminal { command, output } => {
                assert_eq!(command, "cargo build");
                assert!(output.contains("error[E0425]"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn claude_bash_success_and_failure_shapes() {
        let ok = json!({"session_id": "c1", "tool_name": "Bash", "tool_input": {"command": "cargo test"},
                        "tool_response": {"stdout": "test result: ok", "stderr": "warning: x"}});
        assert_eq!(session_key(&ok), "claude:c1");
        assert_eq!(work("PostToolUse", &ok),
                   Work::Terminal { command: "cargo test".into(), output: "test result: ok\nwarning: x".into() });
        let failed = json!({"tool_name": "Bash", "tool_input": {"command": "cargo build"},
                            "error": "Exit code 101\nerror: could not compile"});
        assert_eq!(work("PostToolUseFailure", &failed),
                   Work::Terminal { command: "cargo build".into(), output: "Exit code 101\nerror: could not compile".into() });
    }

    #[test]
    fn every_edit_tool_yields_its_file_and_added_text() {
        let cases = [
            (json!({"tool_name": "Edit", "tool_input": {"file_path": "a.rs", "old_string": "o", "new_string": "N"}}), "a.rs", "N"),
            (json!({"tool_name": "Write", "tool_input": {"file_path": "b.rs", "content": "C"}}), "b.rs", "C"),
            (json!({"tool_name": "replace_string_in_file", "tool_input": {"filePath": "c.rs", "oldString": "o", "newString": "N2"}}), "c.rs", "N2"),
            (json!({"tool_name": "create_file", "tool_input": {"filePath": "d.rs", "content": "C2"}}), "d.rs", "C2"),
            (json!({"tool_name": "edit_notebook_file", "tool_input": {"filePath": "e.ipynb", "newCode": "x=1"}}), "e.ipynb", "x=1"),
        ];
        for (v, file, added) in cases {
            assert_eq!(work("PostToolUse", &v), Work::Edits(vec![(file.into(), added.into())]), "{v}");
        }
    }

    #[test]
    fn multi_file_edits_are_grouped_per_file() {
        let v = json!({"tool_name": "multi_replace_string_in_file", "tool_input": {"explanation": "e", "replacements": [
            {"filePath": "a.rs", "oldString": "o", "newString": "one"},
            {"filePath": "b.rs", "oldString": "o", "newString": "two"},
            {"filePath": "a.rs", "oldString": "o", "newString": "three"}]}});
        assert_eq!(work("PostToolUse", &v),
                   Work::Edits(vec![("a.rs".into(), "one\nthree\n".into()), ("b.rs".into(), "two\n".into())]));
    }

    #[test]
    fn apply_patch_reads_added_lines_per_file() {
        let patch = "\n*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old line\n+new line\n context\n+another\n\
                     *** Delete File: src/gone.rs\n*** Add File: src/b.rs\n+fresh\n*** End Patch\n";
        let v = json!({"tool_name": "apply_patch", "tool_input": {"input": patch, "explanation": "e"}});
        assert_eq!(work("PostToolUse", &v),
                   Work::Edits(vec![("src/a.rs".into(), "new line\nanother\n".into()), ("src/b.rs".into(), "fresh\n".into())]));
    }

    #[test]
    fn reads_and_searches_are_nothing_and_so_are_other_events() {
        let read = json!({"tool_name": "read_file", "tool_input": {"filePath": "a.rs"}, "tool_response": "..."});
        assert_eq!(work("PostToolUse", &read), Work::Nothing);
        let pre = json!({"tool_name": "run_in_terminal", "tool_input": {"command": "ls"}});
        assert_eq!(work("PreToolUse", &pre), Work::Nothing);
        assert_eq!(work("Stop", &json!({})), Work::Nothing);
        let prompt = json!({"prompt": "that's wrong"});
        assert_eq!(work("UserPromptSubmit", &prompt), Work::Prompt("that's wrong".into()));
    }

    #[test]
    fn event_names_come_back_in_pascal_case() {
        assert_eq!(event_name(Some("postToolUse"), &json!({})), "PostToolUse");
        assert_eq!(event_name(None, &json!({"hook_event_name": "UserPromptSubmit"})), "UserPromptSubmit");
        assert_eq!(event_name(None, &json!({})), "");
    }

    #[test]
    fn a_payload_with_raw_control_characters_still_parses() {
        let raw = "{\"tool_name\":\"run_in_terminal\",\"tool_response\":\"line1\nline2\t\u{1b}[31merror\"}";
        assert!(serde_json::from_str::<Value>(raw).is_err(), "the fixture must be invalid strict JSON");
        assert!(parse(raw).is_some());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn end_to_end_a_known_failure_is_pushed_once_and_nothing_else_speaks() {
        let d = crate::test_support::TempDir::new("hook_cli").unwrap();
        let db = d.join("memory.db");
        // An existing (empty) file is not "new": schema only, no seed, no banner.
        std::fs::File::create(&db).unwrap();
        {
            let store = Store::open(&db).unwrap();
            crate::crystallizer::add_anti_pattern(
                &store,
                "cross-compiling dies with ANDROID_NDK_ROOT unset in a non-interactive shell",
                "run the build from a shell that never sourced the env",
                "eval the toolchain from run.sh --print-env first",
                vec![],
            )
            .unwrap();
        }
        let failing = json!({
            "hook_event_name": "PostToolUse", "session_id": "s1", "tool_name": "run_in_terminal",
            "transcript_path": "/x/GitHub.copilot-chat/transcripts/s1.jsonl",
            "tool_input": {"command": "cargo build --target aarch64-linux-android"},
            "tool_response": "thread 'main' panicked at /r/physx-sys/build.rs:270:18:\n\
                              environment variable \"ANDROID_NDK_ROOT\" has not been set: NotPresent\n\
                              error: could not compile `quest_app`"
        })
        .to_string();
        let out = run(None, &failing, &db);
        let v: Value = serde_json::from_str(&out).expect("the reply must be the whole stdout, valid JSON");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("ANDROID_NDK_ROOT"));
        assert_eq!(run(None, &failing, &db), "", "once per failure per session");

        let read = json!({"hook_event_name": "PostToolUse", "tool_name": "read_file", "tool_input": {"filePath": "x"}});
        assert_eq!(run(None, &read.to_string(), &db), "");
        assert_eq!(run(None, "garbage", &db), "");

        let store = Store::open(&db).unwrap();
        let (fired, matched): (i64, i64) = store
            .conn()
            .query_row("SELECT fired, matched FROM hook_heartbeat WHERE hook = 'cli_hook'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((fired, matched), (2, 1), "two terminal calls ran; one found something");
        let runs: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM test_outcomes WHERE session_id = 'vscode:s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 2, "the verdict is recorded under the VS Code session");
    }

    #[test]
    fn a_missing_store_is_never_created() {
        let d = crate::test_support::TempDir::new("hook_cli_nodb").unwrap();
        let db = d.join("absent.db");
        let v = json!({"hook_event_name": "PostToolUse", "tool_name": "run_in_terminal",
                       "tool_input": {"command": "cargo build"}, "tool_response": "error[E0425]: x"});
        assert_eq!(run(None, &v.to_string(), &db), "");
        assert!(!db.exists(), "a hook must not create a store (its first-run banner would break stdout)");
    }
}
