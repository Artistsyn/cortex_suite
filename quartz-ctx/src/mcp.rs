/// MCP (Model Context Protocol) server over stdio.
///
/// Implements just enough of the JSON-RPC MCP spec to register tools that
/// Copilot (or any MCP-capable host) can call during chat.
///
/// Tools exposed:
///   get_item        — full details on a named item
///   list_items      — list all items, optionally filtered by kind
///   search_items    — substring search across names and doc comments
///   get_variants    — all variants for a named enum (the key vocabulary tool)
///
/// Configure in .vscode/mcp.json:
///   {
///     "servers": {
///       "quartz-ctx": {
///         "type": "stdio",
///         "command": "quartz-ctx",
///         "args": ["serve", "--source", "src"]
///       }
///     }
///   }
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::model::{ApiItem, ItemKind, Visibility};
use crate::{helpers, parser};

// ── Source auto-reload ────────────────────────────────────────────────────────

/// How often (at most) we stat-scan the source trees for changes.
const RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Cheap change fingerprint: FNV over every .rs path + mtime in all sources.
fn source_fingerprint(sources: &[(PathBuf, String, bool)]) -> u64 {
    let mut h: u64 = 14695981039346656037;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
    };
    for (path, _, _) in sources {
        for entry in WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "rs"))
        {
            mix(entry.path().to_string_lossy().as_bytes());
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        mix(&d.as_secs().to_le_bytes());
                    }
                }
            }
        }
    }
    h
}

/// Re-parse the sources if anything changed since the last check.
/// The API served is therefore always ground truth — no server restarts needed
/// after engine edits.
fn maybe_reload(
    items: &mut Vec<ApiItem>,
    sources: &[(PathBuf, String, bool)],
    last_check: &mut Instant,
    fingerprint: &mut u64,
) {
    if sources.is_empty() || last_check.elapsed() < RELOAD_CHECK_INTERVAL {
        return;
    }
    *last_check = Instant::now();

    let fp = source_fingerprint(sources);
    if fp == *fingerprint {
        return;
    }
    *fingerprint = fp;

    match parser::load_sources_with(sources) {
        Ok(new_items) if !new_items.is_empty() => {
            eprintln!(
                "quartz-ctx: source change detected — reloaded {} API items (was {})",
                new_items.len(), items.len()
            );
            *items = new_items;
        }
        Ok(_) => eprintln!("quartz-ctx: reload produced 0 items — keeping previous data"),
        Err(e) => eprintln!("quartz-ctx: reload failed ({e}) — keeping previous data"),
    }
}

// ── Curated-knowledge gating ──────────────────────────────────────────────────

// ── Public entry point ────────────────────────────────────────────────────────

pub fn serve(items: Vec<ApiItem>, engine_name: &str, sources: Vec<(PathBuf, String, bool)>) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    let mut items = items;
    let mut last_check = Instant::now();
    let mut fingerprint = source_fingerprint(&sources);

    eprintln!("quartz-ctx MCP server ready ({} items loaded)", items.len());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warn: could not parse request: {e}");
                continue;
            }
        };

        // Notifications have no id and need no response.
        let is_notification = req.get("id").is_none();

        let method = req["method"].as_str().unwrap_or("");

        if is_notification {
            // e.g. "notifications/initialized" — just swallow it
            continue;
        }

        let id = req["id"].clone();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Keep served data in sync with the source tree (throttled stat scan).
        if method == "tools/call" {
            maybe_reload(&mut items, &sources, &mut last_check, &mut fingerprint);
        }

        let result = match method {
            "initialize"  => Ok(initialize_result(engine_name)),
            "tools/list"  => Ok(tools_list_result()),
            "tools/call"  => tools_call(&params, &items),
            other         => Err(format!("unknown method: {other}")),
        };

        let response = match result {
            Ok(r)    => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(msg) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": msg }
            }),
        };

        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }

    Ok(())
}

// ── MCP protocol handlers ─────────────────────────────────────────────────────

fn initialize_result(engine_name: &str) -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "quartz-ctx",
            "version": env!("CARGO_PKG_VERSION"),
            "description": format!("{engine_name} API reference tool")
        }
    })
}

fn tools_list_result() -> Value {
    let full = json!({
        "tools": [
            // ── Core Lookup Tools (Original 4) ────────────────────────────────────
            {
                "name": "get_item",
                "description": "Get complete details on a specific API item by exact name. \
                                Returns kind, full signature, doc comment, fields with types, \
                                all methods, enum variants, and trait implementations. \
                                Use this when you need the full picture of a type.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact name of the type, enum, trait, or function (case-sensitive)."
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "list_items",
                "description": "List all public API items, optionally filtered by kind. \
                                Results grouped by type (Enums, Structs, Traits, Functions). \
                                Use this to discover what APIs are available, or get a quick reference of a category.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "description": "Filter by kind: struct, enum, trait, fn, type, const. \
                                           Leave blank to list all items grouped by category.",
                            "enum": ["struct", "enum", "trait", "fn", "type", "const"]
                        }
                    }
                }
            },
            {
                "name": "search_items",
                "description": "Search for API items by keyword, ranked by relevance. \
                                Searches item names (prioritized) and doc comments. \
                                Surfaces matching enum variants inline. \
                                Use this to find things when you don't know the exact name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keyword or substring to search for (case-insensitive). \
                                           E.g., 'position', 'gravity', 'camera'."
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "get_variants",
                "description": "Get every variant of a named enum with full details. \
                                Returns all variants with their field types and documentation. \
                                **Primary use case for Quartz workflows**: call this before writing an Action, \
                                Condition, or GameEvent to find the exact variant you need. \
                                E.g., get_variants({\"name\": \"Action\"}) to see all available actions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact name of the enum (case-sensitive)."
                        }
                    },
                    "required": ["name"]
                }
            },
            // ── Tier 1: CRITICAL (Hallucination Prevention) ─────────────────────
            {
                "name": "get_trait_implementations",
                "description": "Check what traits a type implements or doesn't implement. \
                                Critical for generic code and understanding type compatibility. \
                                E.g., can you use this in a where T: Clone? Does it implement Copy?",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "type_name": {
                            "type": "string",
                            "description": "Type name to check (e.g., 'GameObject', 'Action', 'GameEvent')."
                        }
                    },
                    "required": ["type_name"]
                }
            },
            // ── Tier 2: HIGH-VALUE (Reliability Improvements) ───────────────────
            {
                "name": "get_builder_methods",
                "description": "Get all builder methods for a type and their correct sequence. \
                                Ensures builder chains are correct and complete. \
                                Use this when building complex objects like GameObject or Scene.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "base_type": {
                            "type": "string",
                            "description": "Base type that has a builder (e.g., 'GameObject', 'Scene', 'Camera')."
                        }
                    },
                    "required": ["base_type"]
                }
            },
            {
                "name": "get_return_type_usage",
                "description": "Find out what you can do with the return value of a method. \
                                Shows methods available on the return type and common usage patterns. \
                                E.g., you called get_velocity(), what methods does Velocity have?",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "method": {
                            "type": "string",
                            "description": "Full method path (e.g., 'GameObject::get_velocity', 'Canvas::camera')."
                        }
                    },
                    "required": ["method"]
                }
            },
            {
                "name": "find_related_types",
                "description": "Discover related APIs and types for a concept. \
                                Helps find the right API when you don't know exact names. \
                                E.g., 'collision detection' → CollisionMode, GameEvent::Collision, etc.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Concept or keyword to find related APIs for."
                        },
                        "related_to": {
                            "type": "string",
                            "description": "Optional: relate it to a specific type or context."
                        }
                    },
                    "required": ["query"]
                }
            },
            // ── Tier 3: ADVANCED (Safety & Performance) ──────────────────────────
            // ── Phase 1 Additions: Behavioral & Semantic Knowledge ──────────────────
            // ── Compact context injection ────────────────────────────────────────
            {
                "name": "get_api_context",
                "description": "Get a compact, budgeted API context packet for a task. Pass a task \
                                description or keywords (e.g. 'spawn pooled bullets with collision and sound') \
                                and receive the most relevant types, enum variant names, and method signatures \
                                in minimal form — one call instead of several get_item/search_items round trips. \
                                Use this FIRST when starting a coding task; drill into specifics with \
                                get_variants/get_item afterwards.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hint": {
                            "type": "string",
                            "description": "Task description or keywords driving relevance ranking."
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "Output budget in characters (default 4000)."
                        },
                        "origin": {
                            "type": "string",
                            "description": "Optional origin filter: e.g. 'quartz', 'synful-quartz', 'path-forge'."
                        }
                    },
                    "required": ["hint"]
                }
            }
        ]
    });

    // Every remaining tool is computed from parsed source, so the whole surface
    // is valid on any Rust project. There is nothing left to gate on engine name:
    // the hand-curated Quartz knowledge that used to be served here now lives in
    // cortex, where it is queryable and can be updated without a recompile.
    full
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

fn tools_call(params: &Value, items: &[ApiItem]) -> Result<Value, String> {
    let tool_name = params["name"]
        .as_str()
        .ok_or("missing tool name")?;

    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let text = match tool_name {
        "get_item"                    => tool_get_item(&args, items),
        "list_items"                  => tool_list_items(&args, items),
        "search_items"                => tool_search_items(&args, items),
        "get_variants"                => tool_get_variants(&args, items),
        "get_api_context"             => tool_get_api_context(&args, items),
        "get_trait_implementations"   => tool_get_trait_implementations(&args, items),
        "get_builder_methods"         => tool_get_builder_methods(&args, items),
        "get_return_type_usage"       => tool_get_return_type_usage(&args, items),
        "find_related_types"          => tool_find_related_types(&args, items),
        // ── Phase 1 additions ──
        other                         => Err(format!("unknown tool: {other}")),
    }?;

    Ok(json!({
        "content": [{ "type": "text", "text": text }]
    }))
}

// ── Tool implementations ──────────────────────────────────────────────────────

fn tool_get_item(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let name = args["name"].as_str().ok_or("missing `name`")?;

    // Items keep source order: the first match is from the primary engine.
    let matches: Vec<&ApiItem> = items.iter().filter(|i| i.name == name).collect();
    let item = *matches.first()
        .ok_or_else(|| format!("no item named `{name}` found"))?;

    let mut out = format!("# `{}` ({})\n\n", item.name, item.kind.label());

    if !item.origin.is_empty() {
        out.push_str(&format!("origin: `{}`", item.origin));
        let others: Vec<&str> = matches.iter().skip(1)
            .map(|i| i.origin.as_str())
            .filter(|o| !o.is_empty())
            .collect();
        if !others.is_empty() {
            out.push_str(&format!("  (also defined in: {})", others.join(", ")));
        }
        out.push_str("\n\n");
    }

    if !item.module_str().is_empty() {
        out.push_str(&format!("module: `{}`\n\n", item.module_str()));
    }
    // A citable location, so an agent can open the declaration rather than
    // search for it.
    if let Some(span) = &item.span {
        out.push_str(&format!("defined at: `{span}`\n\n"));
    }
    // Only worth stating when it is not the default public API surface.
    if item.visibility != Visibility::Public {
        out.push_str(&format!("visibility: `{}`\n\n", item.visibility.label()));
    }
    if !item.doc.is_empty() {
        out.push_str(&format!("{}\n\n", item.doc));
    }

    out.push_str(&format!("```rust\n{}\n```\n\n", item.signature));

    if !item.fields.is_empty() {
        out.push_str("## Fields\n\n");
        for f in &item.fields {
            let doc = if f.doc.is_empty() { String::new() } else { format!(" — {}", f.doc) };
            out.push_str(&format!("- `{}: {}`{}\n", f.name, f.ty, doc));
        }
        out.push('\n');
    }

    if !item.variants.is_empty() {
        out.push_str("## Variants\n\n");
        for v in &item.variants {
            let fields = v.fields_inline();
            let shape = if fields.is_empty() {
                format!("`{}`", v.name)
            } else {
                format!("`{}` `{}`", v.name, fields)
            };
            let doc = if v.doc.is_empty() { String::new() } else { format!(" — {}", v.doc_summary()) };
            out.push_str(&format!("- {}{}\n", shape, doc));
        }
        out.push('\n');
    }

    if !item.methods.is_empty() {
        out.push_str("## Methods\n\n");
        for m in &item.methods {
            let doc = if m.doc.is_empty() { String::new() } else { format!("\n  {}", m.doc_summary()) };
            out.push_str(&format!("- `{}`{}\n", m.signature, doc));
        }
        out.push('\n');
    }

    if !item.traits_impl.is_empty() {
        out.push_str(&format!("**Implements:** {}\n", item.traits_impl.join(", ")));
    }

    Ok(out)
}

fn tool_list_items(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let kind_filter: Option<ItemKind> = match args["kind"].as_str() {
        Some("struct") => Some(ItemKind::Struct),
        Some("enum")   => Some(ItemKind::Enum),
        Some("trait")  => Some(ItemKind::Trait),
        Some("fn")     => Some(ItemKind::Function),
        Some("type")   => Some(ItemKind::TypeAlias),
        Some("const")  => Some(ItemKind::Const),
        Some(other)    => return Err(format!("unknown kind `{other}`")),
        None           => None,
    };

    let filtered: Vec<_> = items
        .iter()
        .filter(|i| kind_filter.as_ref().map_or(true, |k| &i.kind == k))
        .collect();

    if filtered.is_empty() {
        return Ok("No items found.".into());
    }

    let mut out = String::new();

    // Group by kind for readability when listing everything
    if kind_filter.is_none() {
        for (label, kind) in &[
            ("Enums",      ItemKind::Enum),
            ("Structs",    ItemKind::Struct),
            ("Traits",     ItemKind::Trait),
            ("Functions",  ItemKind::Function),
            ("Type Aliases / Constants", ItemKind::TypeAlias),
        ] {
            let group: Vec<_> = filtered.iter().filter(|i| &i.kind == kind).collect();
            if group.is_empty() { continue; }
            out.push_str(&format!("## {}\n", label));
            for item in group {
                let doc = if item.doc_summary().is_empty() { String::new() } else { format!(" — {}", item.doc_summary()) };
                out.push_str(&format!("- `{}`{}\n", item.name, doc));
            }
            out.push('\n');
        }
    } else {
        for item in filtered {
            let doc = if item.doc_summary().is_empty() { String::new() } else { format!(" — {}", item.doc_summary()) };
            out.push_str(&format!("- `{}` ({}){}\n", item.name, item.kind.label(), doc));
        }
    }

    Ok(out)
}

fn tool_search_items(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let query = args["query"]
        .as_str()
        .ok_or("missing `query`")?
        .to_lowercase();

    // Score each item for ranking: exact name matches first, then name contains, then doc matches
    let mut scored: Vec<(i32, &ApiItem)> = items
        .iter()
        .filter_map(|i| {
            let name_lower = i.name.to_lowercase();
            let doc_lower = i.doc.to_lowercase();
            let has_variant_match = i.variants.iter().any(|v| {
                v.name.to_lowercase().contains(&query)
                    || v.doc.to_lowercase().contains(&query)
            });

            let score = if name_lower == query {
                3000 // exact name match
            } else if name_lower.starts_with(&query) {
                2000 // name starts with query
            } else if name_lower.contains(&query) {
                1000 // name contains query
            } else if doc_lower.contains(&query) {
                100  // doc contains query
            } else if has_variant_match {
                50   // variant match
            } else {
                return None;
            };

            Some((score, i))
        })
        .collect();

    // Sort by score descending, then by name for stability
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    if scored.is_empty() {
        return Ok(format!("No items matching `{query}`."));
    }

    let mut out = format!("{} result(s) for `{query}` (sorted by relevance):\n\n", scored.len());
    for (_score, item) in scored {
        out.push_str(&format!("- `{}` ({}", item.name, item.kind.label()));
        if !item.module_str().is_empty() {
            out.push_str(&format!(", module: `{}`", item.module_str()));
        }
        if !item.origin.is_empty() {
            out.push_str(&format!(", origin: `{}`", item.origin));
        }
        out.push(')');
        if !item.doc_summary().is_empty() {
            out.push_str(&format!("\n  {}", item.doc_summary()));
        }
        out.push('\n');

        // Surface matching variants inline
        let matching_variants: Vec<_> = item
            .variants
            .iter()
            .filter(|v| {
                v.name.to_lowercase().contains(&query)
                    || v.doc.to_lowercase().contains(&query)
            })
            .collect();

        for v in matching_variants {
            let fields = v.fields_inline();
            let shape = if fields.is_empty() { v.name.clone() } else { format!("{} {}", v.name, fields) };
            out.push_str(&format!("  ├─ variant `{}`", shape));
            if !v.doc_summary().is_empty() {
                out.push_str(&format!(" — {}", v.doc_summary()));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    Ok(out)
}

fn tool_get_variants(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let name = args["name"].as_str().ok_or("missing `name`")?;

    let item = items
        .iter()
        .find(|i| i.name == name && i.kind == ItemKind::Enum)
        .ok_or_else(|| format!("no enum named `{name}` found"))?;

    if item.variants.is_empty() {
        return Ok(format!("`{name}` has no variants."));
    }

    let mut out = format!("# `{}` variants\n\n", item.name);
    if !item.doc.is_empty() {
        out.push_str(&format!("{}\n\n", item.doc));
    }

    for v in &item.variants {
        let fields = v.fields_inline();
        if fields.is_empty() {
            out.push_str(&format!("## `{}::{}`\n", item.name, v.name));
        } else {
            out.push_str(&format!("## `{}::{}` `{}`\n", item.name, v.name, fields));
        }

        if !v.doc.is_empty() {
            out.push_str(&format!("{}\n\n", v.doc));
        }

        if v.fields.len() > 1 {
            for f in &v.fields {
                let name = if f.name.starts_with('_') { "(positional)".into() } else { format!("`{}`", f.name) };
                let doc = if f.doc.is_empty() { String::new() } else { format!(" — {}", f.doc) };
                out.push_str(&format!("- {}: `{}`{}\n", name, f.ty, doc));
            }
            out.push('\n');
        }
    }

    Ok(out)
}

// ── New 12 Tools (Tier 1–3) ──────────────────────────────────────────────────

fn tool_get_trait_implementations(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let type_name = args["type_name"].as_str().ok_or("missing `type_name`")?;

    let matches: Vec<&ApiItem> = items.iter().filter(|i| i.name == type_name).collect();
    if matches.is_empty() {
        return Err(format!("no item named `{type_name}` found"));
    }

    let mut out = format!("# Trait Implementations for `{type_name}`

");
    for item in &matches {
        if matches.len() > 1 && !item.origin.is_empty() {
            out.push_str(&format!("## origin: `{}`

", item.origin));
        }
        if item.traits_impl.is_empty() {
            out.push_str("No `impl Trait for` blocks found for this type.

");
            continue;
        }
        for t in &item.traits_impl {
            out.push_str(&format!("- `{t}`
"));
        }
        out.push('\n');
    }

    // Which traits exist in the index but this type does NOT implement — useful
    // for "can I call .clone() on this?" style questions. Computed, not curated.
    let all_traits: std::collections::BTreeSet<&str> = items
        .iter()
        .filter(|i| i.kind == ItemKind::Trait)
        .map(|i| i.name.as_str())
        .collect();
    let implemented: std::collections::BTreeSet<&str> = matches
        .iter()
        .flat_map(|i| i.traits_impl.iter().map(|s| s.as_str()))
        .collect();
    let missing: Vec<&str> = all_traits.difference(&implemented).copied().collect();
    if !missing.is_empty() {
        out.push_str(&format!(
            "## Indexed traits NOT implemented by `{type_name}`

{}
",
            missing.iter().map(|t| format!("- `{t}`")).collect::<Vec<_>>().join("
")
        ));
    }

    Ok(out)
}

fn tool_get_builder_methods(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let base_type = args["base_type"].as_str().ok_or("missing `base_type`")?;

    // A builder is either `<Base>Builder` or the base type itself exposing
    // chainable methods. Chainable == returns Self or the builder type.
    let builder_name = format!("{base_type}Builder");
    let candidates: Vec<&ApiItem> = items
        .iter()
        .filter(|i| i.name == builder_name || i.name == base_type)
        .collect();

    if candidates.is_empty() {
        return Err(format!("no item named `{base_type}` or `{builder_name}` found"));
    }

    let mut out = format!("# Builder methods for `{base_type}`

");
    let mut found_any = false;

    for item in candidates {
        let chainable: Vec<&crate::model::ApiMethod> = item
            .methods
            .iter()
            .filter(|m| {
                let sig = m.signature.replace(' ', "");
                sig.contains("->Self")
                    || sig.contains(&format!("->{}", item.name))
                    || sig.contains(&format!("->{builder_name}"))
            })
            .collect();
        let terminal: Vec<&crate::model::ApiMethod> = item
            .methods
            .iter()
            .filter(|m| matches!(m.name.as_str(), "finish" | "build" | "done" | "into_inner"))
            .collect();

        if chainable.is_empty() && terminal.is_empty() {
            continue;
        }
        found_any = true;
        out.push_str(&format!("## `{}`

", item.name));

        if !chainable.is_empty() {
            out.push_str("**Chainable** (return Self, so they compose):

");
            for m in &chainable {
                let doc = if m.doc.is_empty() { String::new() } else { format!(" — {}", m.doc_summary()) };
                out.push_str(&format!("- `{}`{doc}
", m.signature));
            }
            out.push('\n');
        }
        if !terminal.is_empty() {
            out.push_str("**Terminal** (end the chain):

");
            for m in &terminal {
                out.push_str(&format!("- `{}`
", m.signature));
            }
            out.push('\n');
        }
    }

    if !found_any {
        return Ok(format!(
            "`{base_type}` exposes no chainable (-> Self) methods, so it is not a builder.
             Use get_item(\"{base_type}\") for its full method list."
        ));
    }
    Ok(out)
}

fn tool_get_api_context(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let hint = args["hint"].as_str().ok_or("missing `hint`")?;
    let budget = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(4000) as usize;
    let budget = budget.clamp(500, 20_000);
    let origin_filter = args.get("origin").and_then(|v| v.as_str()).unwrap_or("");

    // Tokenize the hint: lowercase words of length >= 3, deduped.
    let mut words: Vec<String> = hint
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect();
    words.dedup();
    if words.is_empty() {
        return Err("hint contains no usable keywords (need words of 3+ characters)".to_string());
    }

    // Score every item as the sum of per-word signals.
    let mut scored: Vec<(i64, &ApiItem)> = items
        .iter()
        .filter(|i| origin_filter.is_empty() || i.origin == origin_filter)
        .filter_map(|item| {
            let name = item.name.to_lowercase();
            let module = item.module_str().to_lowercase();
            let doc = item.doc.to_lowercase();
            let mut score: i64 = 0;
            for w in &words {
                if &name == w                 { score += 300; }
                else if name.contains(w)      { score += 100; }
                if module.contains(w)         { score += 40; }
                if doc.contains(w)            { score += 30; }
                if item.variants.iter().any(|v| v.name.to_lowercase().contains(w)) { score += 25; }
                if item.methods.iter().any(|m| m.name.to_lowercase().contains(w))  { score += 25; }
            }
            if score > 0 { Some((score, item)) } else { None }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    if scored.is_empty() {
        return Ok(format!("No API context found for `{hint}`. Try broader keywords or `list_items`."));
    }

    let mut out = format!("# API context for: {hint}\n\n");
    let mut included = 0usize;
    for (_score, item) in &scored {
        let mut block = String::new();
        let origin = if item.origin.is_empty() { String::new() } else { format!(", {}", item.origin) };
        block.push_str(&format!("### `{}` ({}{})", item.name, item.kind.label(), origin));
        if !item.doc_summary().is_empty() {
            block.push_str(&format!(" — {}", item.doc_summary()));
        }
        block.push('\n');

        match item.kind {
            ItemKind::Enum => {
                // Variant NAMES only — the vocabulary; get_variants gives fields.
                let names: Vec<&str> = item.variants.iter().map(|v| v.name.as_str()).collect();
                if !names.is_empty() {
                    block.push_str(&format!("variants ({}): {}\n", names.len(), names.join(", ")));
                }
            }
            _ => {
                // Up to 8 method signatures, truncated — the callable surface.
                for m in item.methods.iter().take(8) {
                    let mut sig = m.signature.clone();
                    if sig.len() > 100 { sig.truncate(97); sig.push_str("..."); }
                    block.push_str(&format!("  - `{sig}`\n"));
                }
                if item.methods.len() > 8 {
                    block.push_str(&format!("  - ... {} more (use get_item)\n", item.methods.len() - 8));
                }
                if !item.fields.is_empty() && item.methods.is_empty() {
                    let fields: Vec<String> = item.fields.iter().take(10)
                        .map(|f| format!("{}: {}", f.name, f.ty)).collect();
                    block.push_str(&format!("fields: {}\n", fields.join(", ")));
                }
            }
        }
        block.push('\n');

        if out.len() + block.len() > budget {
            let remaining = scored.len() - included;
            out.push_str(&format!("*(budget reached — {remaining} more relevant item(s); refine the hint or raise max_chars)*\n"));
            break;
        }
        out.push_str(&block);
        included += 1;
        if included >= 15 { break; }
    }

    out.push_str("\nDrill down with get_variants(<enum>) or get_item(<type>) for full field/doc detail.\n");
    Ok(out)
}

fn tool_get_return_type_usage(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let query = args["method"].as_str().ok_or("missing `method`")?;
    // Accept "Type::method", "type.method" or a bare method name.
    let bare = query.rsplit(|c| c == '.' || c == ':').next().unwrap_or(query);

    let mut hits: Vec<(&ApiItem, &crate::model::ApiMethod)> = Vec::new();
    for item in items {
        for m in &item.methods {
            if m.name == bare {
                hits.push((item, m));
            }
        }
    }

    if hits.is_empty() {
        return Err(format!(
            "no method named `{bare}` in the index. Try search_items(\"{bare}\")."
        ));
    }

    let mut out = format!("# Return type and borrowing for `{bare}`\n");
    for (item, m) in hits.iter().take(8) {
        let sig = m.signature.replace(' ', "");
        let returns = m
            .signature
            .split_once("->")
            .map(|(_, r)| r.trim().to_string())
            .unwrap_or_else(|| "()".to_string());

        // Borrow kind is readable straight off the receiver and return type.
        let receiver = if sig.contains("(&mutself") {
            "takes `&mut self` — exclusive borrow; cannot overlap another borrow of the same value"
        } else if sig.contains("(&self") {
            "takes `&self` — shared borrow; several may be held at once"
        } else if sig.contains("(self") {
            "takes `self` by value — consumes the receiver"
        } else {
            "associated function — no receiver"
        };

        // `quote!` renders types with spaces (`Option <& mut GameObject>`), so
        // every shape test must run against a despaced copy. Testing the raw
        // string reported `Option <& mut T>` as an owned value — the exact
        // borrow advice that leads to double-borrow panics.
        let r = returns.replace(' ', "");
        let lifetime = if r.starts_with("&mut") {
            "returns a MUTABLE reference: it keeps the exclusive borrow alive, so drop it before borrowing the owner again"
        } else if r.starts_with('&') {
            "returns a shared reference: it borrows the owner for as long as the value is held"
        } else if r.contains("Option<&mut") || r.contains("Result<&mut") {
            "returns an optional MUTABLE reference: holding it blocks any other borrow of the owner"
        } else if r.contains("Option<&") || r.contains("Result<&") {
            "returns an optional shared reference: it borrows the owner while held"
        } else {
            "returns an owned value: no borrow outlives the call"
        };

        out.push_str(&format!("## `{}::{}`\n", item.name, m.name));
        out.push_str(&format!("```rust\n{}\n```\n", m.signature));
        out.push_str(&format!("- **Returns:** `{returns}`\n"));
        out.push_str(&format!("- **Receiver:** {receiver}\n"));
        out.push_str(&format!("- **Borrowing:** {lifetime}\n"));
        if !m.doc.is_empty() {
            out.push_str(&format!("- {}\n", m.doc_summary()));
        }
        out.push_str("\n");
    }

    if hits.len() > 8 {
        out.push_str(&format!("*({} more definitions of this name)*\n", hits.len() - 8));
    }
    Ok(out)
}

fn tool_find_related_types(args: &Value, items: &[ApiItem]) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("missing `query`")?;

    let related = helpers::find_related_apis(query, items);
    if related.is_empty() {
        return Ok(format!("No related types found for `{query}`."));
    }

    let mut out = format!("# Related Types for `{query}`\n\n");
    for item in related.iter().take(10) {
        out.push_str(&format!("- `{}` ({})", item.name, item.kind.label()));
        if !item.doc_summary().is_empty() {
            out.push_str(&format!(" — {}", item.doc_summary()));
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod borrow_tests {
    /// Mirrors the classification in `tool_get_return_type_usage`.
    /// `quote!` renders types with spaces, so every shape test runs despaced.
    fn classify(returns: &str) -> &'static str {
        let r = returns.replace(' ', "");
        if r.starts_with("&mut") {
            "mut-ref"
        } else if r.starts_with('&') {
            "shared-ref"
        } else if r.contains("Option<&mut") || r.contains("Result<&mut") {
            "opt-mut-ref"
        } else if r.contains("Option<&") || r.contains("Result<&") {
            "opt-shared-ref"
        } else {
            "owned"
        }
    }

    /// The bug this guards: `Option <& mut GameObject>` (as `quote!` renders it)
    /// was classified "owned", telling agents no borrow outlives the call — the
    /// precise misinformation that produces double-borrow panics.
    #[test]
    fn spaced_option_mut_reference_is_not_mistaken_for_owned() {
        assert_eq!(classify("Option <& mut GameObject>"), "opt-mut-ref");
        assert_eq!(classify("Option<&mut GameObject>"), "opt-mut-ref");
    }

    #[test]
    fn spaced_shapes_classify_the_same_as_unspaced() {
        for (spaced, tight) in [
            ("& mut Canvas", "&mut Canvas"),
            ("& GameObject", "&GameObject"),
            ("Option <& GameObject>", "Option<&GameObject>"),
            ("Result <& mut T , E>", "Result<&mut T,E>"),
        ] {
            assert_eq!(classify(spaced), classify(tight), "mismatch for {spaced}");
        }
    }

    #[test]
    fn owned_values_stay_owned() {
        for owned in ["Self", "String", "Option <String>", "Vec <GameObject>", "f32"] {
            assert_eq!(classify(owned), "owned", "{owned} misclassified");
        }
    }
}
