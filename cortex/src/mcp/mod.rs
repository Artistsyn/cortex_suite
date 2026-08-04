pub mod tools;

use std::path::PathBuf;
use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use crate::cache::{
    cache_stats, compute_index_version, get_cached_response, cache_response,
    invalidate_stale, SessionRegistry,
};
use crate::memory::Store;
use crate::model::CodeUnit;

use std::sync::atomic::{AtomicBool, Ordering};

/// In-memory cache of protocol state — avoids DB queries on every tool call.
/// Refreshed only when a bootstrap tool fires or begin_protocol_session is called.
static PROTOCOL_MODE: AtomicBool = AtomicBool::new(false);
static BOOTSTRAP_DONE: AtomicBool = AtomicBool::new(false);

/// Max response cache entries before LRU eviction kicks in.
const CACHE_MAX_ENTRIES: usize = 256;

/// Tools that are never cached (always need live data).
const UNCACHEABLE: &[&str] = &[
    "suggest_pattern",
    "list_patterns",
    "get_anti_patterns",
    "get_delta",
    "recurrent_think",
    "simulate_change",
    "explain_dependency_path",
    "begin_protocol_session",
    "get_session_health",
    "flush_knowledge_markers",
    "closeout_session",
    "propose_skill",
    // Output varies per call (live command output) — never cache.
    "compact_output",
];

pub fn serve(
    store: Store,
    units: Vec<CodeUnit>,
    engine_name: &str,
    repo_root: PathBuf,
    prefs_summary: String,
) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let engine_name = engine_name.to_string();
    let sessions = SessionRegistry::new();

    // Compute index version once at startup; changes only after `cortex index`.
    let index_version = compute_index_version(store.conn())
        .unwrap_or_else(|_| "unknown".to_string());

    // Flush stale cache entries from previous index versions.
    let flushed = invalidate_stale(store.conn(), &index_version).unwrap_or(0);
    if flushed > 0 {
        eprintln!("  cache: flushed {} stale entries (index version changed)", flushed);
    }

    let stats = cache_stats(store.conn()).ok();
    if let Some(s) = &stats {
        eprintln!(
            "  cache: {} entries, {} content blobs, {} total hits",
            s.entries, s.content_blobs, s.total_hits
        );
    }

    eprintln!("cortex MCP server ready ({} units, {} patterns, {} anti-patterns)",
        units.len(),
        store.all_patterns().map(|p| p.len()).unwrap_or(0),
        store.all_anti_patterns().map(|p| p.len()).unwrap_or(0),
    );

    // Phase 0B: derive logical session key from mcp_calls timing window.
    // Falls back to process-id-based key if DB not yet populated.
    // Pass repo_root to disambiguate projects within the same 2-hour window.
    let repo_root_str = repo_root.to_string_lossy();
    let session_id = crate::protocol::current_session_key(store.conn(), Some(&repo_root_str))
        .unwrap_or_else(|_| format!("session_{}", std::process::id()));

    // Initialize protocol state cache (avoids DB queries on every tool call).
    PROTOCOL_MODE.store(
        crate::protocol::is_protocol_mode(store.conn(), &session_id).unwrap_or(false),
        Ordering::Relaxed,
    );
    BOOTSTRAP_DONE.store(
        crate::protocol::is_bootstrap_complete(store.conn(), &session_id).unwrap_or(false),
        Ordering::Relaxed,
    );

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => { eprintln!("warn: bad request: {e}"); continue; }
        };

        if req.get("id").is_none() { continue; } // notification

        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize"  => Ok(initialize_result(&engine_name)),
            "tools/list"  => Ok(tools_list()),
            "tools/call"  => {
                let tool = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let args_str = args.to_string();

                // Log the call regardless of cache hit.
                if let Ok(call_id) = store.log_mcp_call(tool, &args_str) {
                    let _ = store.log_session_retrieval(
                        &session_id,
                        "mcp_calls",
                        call_id,
                        tool,
                    );
                }

                // Phase 0B: auto-record bootstrap steps when completed.
                // Also gate work tools if this is a PROTOCOL session and Phase 0 is incomplete.
                // Protocol state is cached in-memory to avoid DB queries on every tool call.
                {
                    use crate::protocol::ProtocolStep;
                    // Update cached protocol state when bootstrap tools fire.
                    let mut protocol_state_changed = false;
                    match tool {
                        "get_delta"         => { let _ = crate::protocol::record_step(store.conn(), &session_id, ProtocolStep::GetDelta); protocol_state_changed = true; }
                        "get_preferences"   => { let _ = crate::protocol::record_step(store.conn(), &session_id, ProtocolStep::GetPreferences); protocol_state_changed = true; }
                        "get_anti_patterns" => { let _ = crate::protocol::record_step(store.conn(), &session_id, ProtocolStep::GetAntiPatterns); protocol_state_changed = true; }
                        "get_context" | "list_patterns" => { let _ = crate::protocol::record_step(store.conn(), &session_id, ProtocolStep::GetContext); protocol_state_changed = true; }
                        "begin_protocol_session" => { protocol_state_changed = true; }
                        // Work tools: blocked in PROTOCOL mode until Phase 0 complete.
                        // Uses cached protocol state — refreshed only when state changes.
                        "semantic_search" | "get_item" | "get_syntax" | "get_usage_examples"
                        | "get_helper" | "recall" | "query_graph" | "simulate_change"
                        | "suggest_pattern" => {
                            let is_protocol = PROTOCOL_MODE.load(std::sync::atomic::Ordering::Relaxed);
                            if is_protocol {
                                let bootstrap_done = BOOTSTRAP_DONE.load(std::sync::atomic::Ordering::Relaxed);
                                if !bootstrap_done {
                                    let msg = crate::protocol::gate_error_message(&session_id, store.conn())
                                        .unwrap_or_else(|_| "PROTOCOL_PHASE_0_INCOMPLETE".to_string());
                                    let resp = json!({
                                        "content": [{ "type": "text", "text": msg }]
                                    });
                                    let response = json!({ "jsonrpc": "2.0", "id": id, "result": resp });
                                    writeln!(out, "{}", serde_json::to_string(&response)?)?;
                                    out.flush()?;
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                    // Refresh cache if protocol state may have changed.
                    if protocol_state_changed {
                        let is_p = crate::protocol::is_protocol_mode(store.conn(), &session_id).unwrap_or(false);
                        let is_b = crate::protocol::is_bootstrap_complete(store.conn(), &session_id).unwrap_or(false);
                        PROTOCOL_MODE.store(is_p, std::sync::atomic::Ordering::Relaxed);
                        BOOTSTRAP_DONE.store(is_b, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Check response cache (skip for volatile tools).
                let cached = if UNCACHEABLE.contains(&tool) {
                    None
                } else {
                    get_cached_response(store.conn(), tool, &args_str, &index_version)
                        .unwrap_or(None)
                };

                if let Some(cached_text) = cached {
                    eprintln!("  [cache hit] {}", tool);
                    Ok(json!({
                        "content": [{ "type": "text", "text": cached_text }]
                    }))
                } else {
                    let result = tools::dispatch(
                        tool,
                        &args,
                        &store,
                        &units,
                        &sessions,
                        &session_id,
                        &repo_root,
                        &prefs_summary,
                    );

                    // Cache the result if it was successful and tool is cacheable.
                    if let Ok(ref res) = result {
                        if !UNCACHEABLE.contains(&tool) {
                            if let Some(text) = res["content"][0]["text"].as_str() {
                                let _ = cache_response(
                                    store.conn(), tool, &args_str,
                                    &index_version, text, CACHE_MAX_ENTRIES,
                                );
                            }
                        }
                    }

                    result
                }
            }
            other => Err(format!("unknown method: {other}")),
        };

        let response = match result {
            Ok(r)    => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(msg) => json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": msg } }),
        };

        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }

    sessions.clear_session(&session_id);
    Ok(())
}

fn initialize_result(engine_name: &str) -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "cortex",
            "version": env!("CARGO_PKG_VERSION"),
            "description": format!("{engine_name} semantic memory layer")
        }
    })
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "semantic_search",
                "description": "Search the codebase by intent or concept. \
                                Returns the most semantically relevant API items. \
                                Use this FIRST before writing any engine code.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Intent or concept to search for." },
                        "limit": { "type": "integer", "description": "Max results (default: 5)." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "get_item",
                "description": "Get full compressed details on a named API item — \
                                signature, fields, variants, methods. The index spans \
                                several projects and both engine forks, so a bare name \
                                can be ambiguous; alternatives are listed when it is. \
                                Pass a full unit id as `name`, or set `scope`, to pin \
                                one exactly.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact item name, or a full unit id such as \
                                            `canvas::core::Canvas` to resolve unambiguously."
                        },
                        "scope": {
                            "type": "string",
                            "description": "Optional scope prefix to restrict the search, \
                                            e.g. `synful`, `path_forge`, `ss_engine`. \
                                            Omit for the primary unscoped engine."
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "get_syntax",
                "description": "Get a concise syntax sheet for a symbol: signature, fields, methods, and enum variants when available.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol_name": { "type": "string", "description": "Symbol or unit id to inspect." }
                    },
                    "required": ["symbol_name"]
                }
            },
            {
                "name": "get_usage_examples",
                "description": "Return ranked usage examples for a symbol from indexed callsites and symbol example cache.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol_name": { "type": "string", "description": "Symbol or unit id to inspect." },
                        "tier": { "type": "string", "description": "Optional source tier filter (for example: index_unit, production_fn)." },
                        "limit": { "type": "integer", "description": "Max examples to return (default: 5)." }
                    },
                    "required": ["symbol_name"]
                }
            },
            {
                "name": "get_helper",
                "description": "Get symbol-specific helper guidance and safe usage patterns for an implementation intent.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol_name": { "type": "string", "description": "Symbol or unit id to inspect." },
                        "intent": { "type": "string", "description": "Optional intent context (for example: refactor, safe usage, examples)." }
                    },
                    "required": ["symbol_name"]
                }
            },
            {
                "name": "get_context",
                "description": "Get a pre-compiled, token-efficient context packet \
                                for the current task. Pass open file paths or a task \
                                description. Returns relevant API, patterns, anti-patterns, \
                                and notes in minimal form.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hint": { "type": "string", "description": "Task description or open file paths." },
                        "token_budget": { "type": "integer", "description": "Max tokens to use (default: 2000)." },
                        "delta_include": { "type": "string", "description": "Optional substring include filter for changed paths." },
                        "delta_exclude": { "type": "string", "description": "Optional substring exclude filter for changed paths." },
                        "delta_max_files": { "type": "integer", "description": "Max changed files in context delta section (default: 8)." },
                        "delta_max_patch_lines": { "type": "integer", "description": "Max patch lines captured per changed file (default: 40)." }
                    },
                    "required": ["hint"]
                }
            },
            {
                "name": "get_delta",
                "description": "Get compressed git delta entries for working tree or from a commit range.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since": { "type": "string", "description": "Optional start ref. Uses HEAD working tree diff when omitted." },
                        "include": { "type": "string", "description": "Optional substring include filter for changed paths." },
                        "exclude": { "type": "string", "description": "Optional substring exclude filter for changed paths." },
                        "max_files": { "type": "integer", "description": "Max changed files to return (default: 128)." },
                        "max_patch_lines": { "type": "integer", "description": "Max patch lines inspected per changed file (default: 40)." }
                    }
                }
            },
            {
                "name": "query_graph",
                "description": "Query the knowledge graph around an indexed item by name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Indexed item name." },
                        "depth": { "type": "integer", "description": "Neighbor depth (default: 1)." }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "explain_dependency_path",
                "description": "Explain one dependency path between two symbols using graph edges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "Start symbol name or graph node id." },
                        "to": { "type": "string", "description": "Target symbol name or graph node id." },
                        "depth": { "type": "integer", "description": "Maximum path depth (default: 4)." }
                    },
                    "required": ["from", "to"]
                }
            },
            {
                "name": "get_preferences",
                "description": "Return the active coding preferences loaded by cortex. Style, API \
                                and project fields always come in full. The `notes` list is the bulk \
                                of the payload, so every note is listed but only hint-matching ones \
                                are expanded — pass `hint` describing your task to get the relevant \
                                ones complete on the first call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hint": {
                            "type": "string",
                            "description": "What you are about to work on. Notes matching this are \
                                            expanded to full text; the rest are listed as their \
                                            opening clause."
                        },
                        "detail": {
                            "type": "string",
                            "enum": ["index", "full"],
                            "description": "index (default): hint-matched notes in full, rest as \
                                            opening clause. full: every note complete."
                        }
                    }
                }
            },
            {
                "name": "recurrent_think",
                "description": "Iterative hypothesis refinement loop for complex tasks. \
                                Propose → Critique → Refine → Assess → Halt or Continue. \
                                Max 6 loops by default. Use for deep design decisions or multi-step problems.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Problem or feature description." },
                        "hypothesis": { "type": "string", "description": "Optional: current hypothesis to critique." },
                        "loop": { "type": "integer", "description": "Current loop index (default: 0)." },
                        "max_loops": { "type": "integer", "description": "Max iterations (default: 6, capped at 16)." },
                        "depth_mode": {
                            "type": "string",
                            "description": "Iteration preset: auto (default), shallow (2), deep (up to 16).",
                            "enum": ["auto", "shallow", "deep"]
                        }
                    },
                    "required": ["task"]
                }
            },
            {
                "name": "simulate_change",
                "description": "Dry-run impact predictor. Simulates what breaks if you change an item. \
                                Returns risk level (Low/Medium/High) and list of affected downstream items. \
                                Call before modifying widely-used types.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "item": { "type": "string", "description": "Name of the item to change." },
                        "change": { "type": "string", "description": "Description of the change (e.g., 'add new variant')." },
                        "depth": { "type": "integer", "description": "Transitive depth (default: 1)." },
                        "relation_filter": {
                            "description": "Optional relation filter (string or string array), for example uses/calls/implements.",
                            "oneOf": [
                                { "type": "string" },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        }
                    },
                    "required": ["item"]
                }
            },
            {
                "name": "recall",
                "description": "Retrieve everything cortex knows about a topic — \
                                matching patterns, anti-patterns, annotations, and API items. \
                                Use when you need to know if we've solved this before.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "Topic, name, or concept to recall." }
                    },
                    "required": ["topic"]
                }
            },
            {
                "name": "list_patterns",
                "description": "List all approved code patterns with their intents. \
                                Includes use/revert/survival metrics and flags patterns below 40% survival. \
                                Check this before implementing any non-trivial logic. Every pattern is \
                                always listed; pass `hint` describing the task to get body text for the \
                                ones that apply.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "detail": {
                            "type": "string",
                            "description": "Output detail tier (default: summary). Every pattern is \
                                            always listed with its intent and survival rate; the tier \
                                            controls how much body text comes with it.",
                            "enum": ["summary", "standard", "full"]
                        },
                        "hint": {
                            "type": "string",
                            "description": "What you are about to write. Patterns matching this are \
                                            expanded to include their body preview, so the ones \
                                            relevant to the task arrive complete."
                        }
                    }
                }
            },
            {
                "name": "get_anti_patterns",
                "description": "Get all known anti-patterns — things Copilot must NOT do. \
                                Always check this before generating code. Every anti-pattern \
                                is always listed; `detail` controls whether the remedy text \
                                comes with it. Pass `hint` describing the task to get the \
                                full wrong/correct text for the ones that apply.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "detail": {
                            "type": "string",
                            "enum": ["index", "full"],
                            "description": "index (default): every anti-pattern's description, \
                                            plus full remedy text for any matching `hint`. \
                                            full: complete wrong/correct text for all of them."
                        },
                        "hint": {
                            "type": "string",
                            "description": "What you are about to write, e.g. 'spawn pooled \
                                            enemy with gravity'. Anti-patterns matching this \
                                            are expanded to full remedy text."
                        }
                    }
                }
            },
            {
                "name": "suggest_pattern",
                "description": "Suggest a pattern for Syn's review. Does NOT save it — \
                                queues it as a pending observation for manual approval only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":   { "type": "string" },
                        "intent": { "type": "string" },
                        "body":   { "type": "string", "description": "The code pattern." },
                        "uses":   { "type": "array", "items": { "type": "string" }, "description": "API item names used." }
                    },
                    "required": ["name", "intent", "body"]
                }
            },
            {
                "name": "list_all",
                "description": "List all indexed API items, optionally filtered by kind.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "description": "Filter: struct, enum, trait, fn. Omit for all.",
                            "enum": ["struct", "enum", "trait", "fn"]
                        },
                        "detail": {
                            "type": "string",
                            "description": "Output detail tier (default: standard).",
                            "enum": ["summary", "standard", "full"]
                        }
                    }
                }
            },
            {
                "name": "begin_protocol_session",
                "description": "Start a PROTOCOL-mode session. Enables Phase 0 enforcement and session tracking. \
                                Call this immediately when the user's message contains PROTOCOL. \
                                Returns current session health. Work tools (semantic_search, recall, get_item, etc.) \
                                are gated until Phase 0 is complete.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Brief task description for session tracking." },
                        "mode": {
                            "type": "string",
                            "enum": ["cortex", "quartz", "graphify", "full"],
                            "description": "Which subsystems are active (informational only)."
                        }
                    },
                    "required": ["task"]
                }
            },
            {
                "name": "get_session_health",
                "description": "One-call session status: Phase 0 completion, knowledge markers written, \
                                pending observations, closeout status, top query gaps, pattern health, \
                                pending proposals. Run after bootstrap to confirm readiness and \
                                before ending a session to confirm closeout.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "flush_knowledge_markers",
                "description": "Stage CORTEX-* knowledge markers into the Cortex DB. \
                                Pass `text` containing your markers (required on Claude Code / Continue / CLI; \
                                on VS Code it falls back to scraping the Copilot session store if `text` is omitted). \
                                Markers are STAGED but not committed until closeout_session is called with \
                                inline_approve=true. Call this any time you've written markers you want captured.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text containing your [CORTEX-*] markers. On non-VS-Code hosts this is required."
                        }
                    }
                }
            },
            {
                "name": "closeout_session",
                "description": "Complete session closeout. Set inline_approve=true ONLY when the user has \
                                typed 'KNOWLEDGE COMMITTED' — this immediately commits all session markers to \
                                the Cortex DB without deferred review. Stages markers when inline_approve=false (default).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "outcome_type": {
                            "type": "string",
                            "enum": ["build_pass", "build_fail", "test_fail", "review_findings", "research_only"],
                            "description": "What happened this session."
                        },
                        "inline_approve": {
                            "type": "boolean",
                            "description": "true = immediately commit all session knowledge. Use ONLY when user typed KNOWLEDGE COMMITTED."
                        },
                        "error_text":   { "type": "string", "description": "Optional error context if failure." },
                        "diff_symbols": { "type": "string", "description": "Optional comma-separated symbols changed." },
                        "markers_text": { "type": "string", "description": "Text containing your [CORTEX-*] markers to commit/stage. Required on Claude Code / Continue / CLI (VS Code falls back to session-store scraping if omitted)." }
                    },
                    "required": ["outcome_type"]
                }
            },
            {
                "name": "propose_skill",
                "description": "Author a new skill from a workflow you've been executing. YOU write the \
                                content: pass a complete, concrete `procedure` (markdown allowed — numbered \
                                steps, exact tool calls, API names, pitfalls you hit) drawn from your actual \
                                session experience. Your text is preserved verbatim in the draft. Use when \
                                closeout reports a skill-authoring opportunity, or whenever you notice a \
                                repeatable multi-step workflow. Writes a draft to .cortex/proposals/ for \
                                human approval via skill-approve.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":      { "type": "string", "description": "Short kebab-case skill name." },
                        "trigger":   { "type": "string", "description": "When to invoke this skill (one bullet per line)." },
                        "procedure": { "type": "string", "description": "The full procedure in markdown — real steps from your experience, not placeholders. Include exact tool calls, argument shapes, and known pitfalls." },
                        "when_not_to_use": { "type": "string", "description": "When NOT to use this skill (one bullet per line)." },
                        "tools":     { "type": "string", "description": "Comma-separated tool names used." }
                    },
                    "required": ["name", "procedure"]
                }
            },
            {
                "name": "compact_output",
                "description": "Losslessly compact command output. Pass the command plus its stdout \
                                and stderr; returns the same output with only provably-redundant lines \
                                removed (build/download progress, per-test `... ok` lines == cargo -q, \
                                duplicate lines). Every error, warning, note, panic, and failure block is \
                                kept verbatim with its file:line. The full original is saved to .cortex/tee/ \
                                whenever anything is dropped. Does not execute anything.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command that produced the output (used to pick the filter)." },
                        "stdout":  { "type": "string", "description": "The command's stdout stream." },
                        "stderr":  { "type": "string", "description": "The command's stderr stream (cargo/rustc write diagnostics here)." }
                    },
                    "required": ["command"]
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::{tools_list, UNCACHEABLE};
    use serde_json::Value;

    fn find_tool<'a>(tools: &'a [Value], name: &str) -> Option<&'a Value> {
        tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
    }

    #[test]
    fn tools_list_includes_extended_tooling_entries() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");

        for name in ["get_usage_examples", "get_helper", "explain_dependency_path",
                     "begin_protocol_session", "get_session_health",
                     "flush_knowledge_markers", "closeout_session", "propose_skill"] {
            assert!(find_tool(tools, name).is_some(), "missing tool in tools/list: {name}");
        }
    }

    #[test]
    fn simulate_change_schema_exposes_relation_filter() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");
        let simulate_change = find_tool(tools, "simulate_change").expect("simulate_change tool present");

        let properties = simulate_change["inputSchema"]["properties"]
            .as_object()
            .expect("simulate_change inputSchema.properties object");
        assert!(
            properties.contains_key("relation_filter"),
            "simulate_change schema missing relation_filter"
        );
    }

    #[test]
    fn explain_dependency_path_is_not_cached() {
        assert!(
            UNCACHEABLE.contains(&"explain_dependency_path"),
            "explain_dependency_path should be uncacheable"
        );
    }

    #[test]
    fn list_tools_expose_detail_tiers() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");

        for tool_name in ["list_patterns", "list_all"] {
            let tool = find_tool(tools, tool_name).expect("list tool present");
            let detail = tool["inputSchema"]["properties"]["detail"]
                .as_object()
                .expect("detail schema object");
            let enum_values = detail
                .get("enum")
                .and_then(Value::as_array)
                .expect("detail enum");
            assert!(
                enum_values.iter().any(|v| v.as_str() == Some("summary")),
                "{tool_name} missing summary detail tier"
            );
            assert!(
                enum_values.iter().any(|v| v.as_str() == Some("standard")),
                "{tool_name} missing standard detail tier"
            );
            assert!(
                enum_values.iter().any(|v| v.as_str() == Some("full")),
                "{tool_name} missing full detail tier"
            );
        }
    }
}
