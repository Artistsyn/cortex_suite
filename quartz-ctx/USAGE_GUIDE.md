# Quartz-CTX Usage Guide

## Overview

`quartz-ctx` is a dual-mode Rust API context tool for the Quartz game engine:

- **Mode 1: Static Generation** — Creates markdown reference docs from the Quartz API
- **Mode 2: Live MCP Skill** — Runs as an MCP server that Copilot can query in real-time

This guide covers both modes and best practices for using them in your workflow.

---

## Installation

### Option A: Build from Source (Recommended)

```bash
cd quartz-ctx
cargo build --release
```

The binary will be at `target/release/quartz-ctx`.

### Option B: Install Globally

```bash
cd quartz-ctx
cargo install --path .
```

Now `quartz-ctx` is available as a command from anywhere.

### Option C: Use Absolute Path in MCP Config

If you don't want to add to PATH, reference the binary directly in `.vscode/mcp.json`:

```json
"quartz-ctx": {
  "type": "stdio",
  "command": "C:/path/to/FlowMake/target/release/quartz-ctx",
  "args": ["serve", "--source", "quartz/src", "--name", "Quartz"]
}
```

---

## Mode 1: Generate Static Documentation

### When to Use

- After making changes to the Quartz API
- To create reference docs for offline browsing
- To keep local by default (generated docs now land in `docs/<scraped-directory>/` and are ignored)
- As a backup/fallback when MCP server isn't running

### Command

```bash
cd quartz-ctx
cargo run -- generate --source ../quartz/src --name Quartz
```

Or if installed globally:

```bash
quartz-ctx generate --source quartz/src --name Quartz
```

### Output

Generates these files in `docs/<scraped-directory>/`:

| File | Contents |
|------|----------|
| `INDEX.md` | Entry point with module map and statistics |
| `vocabulary.md` | All enums (primary API vocabulary) |
| `types.md` | All structs with fields and methods |
| `traits.md` | All trait definitions |
| `functions.md` | All free functions |
| `misc.md` | Type aliases and constants |
| `api-graph.json` | Machine-readable dump for tooling |

### Flags

```
--source <DIR>      Source directory to scan [default: src]
--output <DIR>      Where to write docs [default: .]
--name <NAME>       Engine name in docs [default: Quartz]
--minimal           Only generate INDEX, vocabulary, and JSON (skip detail files)
--dry-run           Print found items without writing files
```

### Example: Minimal Docs

```bash
quartz-ctx generate --source quartz/src --name Quartz --minimal
```

Only generates INDEX.md, vocabulary.md, and api-graph.json — useful for keeping docs lightweight.

### Example: Dry Run (Preview)

```bash
quartz-ctx generate --source quartz/src --name Quartz --dry-run
```

Prints all extracted items to stdout without writing files. Good for verification.

---

## Mode 2: Live MCP Skill Server

### When to Use

- While actively writing Quartz code
- When you need to look up exact signatures, variants, or method names
- To get real-time answers that reflect the current codebase
- As the primary interface during `PROTOCOL - QUARTZ -` sessions

### Setup

#### Step 1: Build or Install (if not done already)

See "Installation" section above.

#### Step 2: Add to .vscode/mcp.json

At the FlowMake root (`.vscode/mcp.json`):

```json
{
  "servers": {
    "quartz-ctx": {
      "type": "stdio",
      "command": "quartz-ctx",
      "args": ["serve", "--source", "quartz/src", "--name", "Quartz"]
    }
  }
}
```

#### Step 3: Restart VS Code

MCP servers are loaded on startup. Restart VS Code for Copilot to activate the skill.

### Verification

In VS Code, open Copilot Chat and ask:

```
What enums are available?
```

Copilot should call `list_items` and return a categorized list. If nothing appears, check:

1. Is `quartz-ctx` binary in PATH or correctly configured?
2. Is `.vscode/mcp.json` valid JSON?
3. Did you restart VS Code after adding the server config?

### Startup Diagnostics (Recommended)

Run this before wiring MCP if you suspect path or parse issues:

```bash
quartz-ctx selfcheck --source quartz/src --name Quartz
```

Machine-readable output for automation:

```bash
quartz-ctx selfcheck --source quartz/src --name Quartz --json
```

If `status` is `FAIL`, fix source path/parsing first. MCP `args` must begin with `serve`.

### Available Tools

Once the server is running, Copilot has access to 4 tools:

#### 1. `get_variants` (PRIMARY for Quartz)

**When to use:** Before writing an `Action`, `Condition`, `GameEvent`, or any enum-based code.

**Example:**

```
What are all the variants of Action?
```

Copilot calls `get_variants({"name": "Action"})` and returns every variant with:
- Full name (e.g., `Action::SetPosition`)
- Field types and names
- Documentation for each field

**Best for:** Finding the exact variant you need to express an intent.

#### 2. `search_items` (Find by keyword)

**When to use:** You don't know the exact name of what you're looking for.

**Example:**

```
Find all APIs related to gravity
```

Copilot calls `search_items({"query": "gravity"})`. Results are ranked:
1. Exact name matches (`Gravity` enum)
2. Name contains query (`GravityFalloff`, `GravityConfig`)
3. Doc contains query (anything documented as handling gravity)
4. Variant matches (enum variants with matching names/docs)

**Best for:** Exploratory queries to discover what's available.

#### 3. `get_item` (Full details)

**When to use:** You need the complete picture of a specific type.

**Example:**

```
Show me the GameObject type with all its methods
```

Copilot calls `get_item({"name": "GameObject"})` and returns:
- Full signature
- All fields with types and docs
- All methods with signatures
- All variants (if enum)
- Trait implementations

**Best for:** Deep-dive understanding of a type's API surface.

#### 4. `list_items` (Browse by category)

**When to use:** You want to see all items of a certain kind.

**Example:**

```
Show me all the enum types
```

Copilot calls `list_items({"kind": "enum"})` and returns all enums with brief docs.

**Supported kinds:** `struct`, `enum`, `trait`, `fn`, `type`, `const`

**Best for:** Inventory/discovery of the API landscape.

---

## Workflow Examples

### Example 1: Implementing a New Game Object Behavior

```
PROTOCOL - QUARTZ -
I need to make the player jump when spacebar is pressed.
```

Bot boots, loads quartz-ctx MCP.

**You:** What Actions are available for movement?

**Copilot:** Calls `search_items({"query": "jump"})` → finds `Action::Jump` and related variants.

**You:** Show me the Jump action details

**Copilot:** Calls `get_item({"name": "Action"})` → shows `Jump` variant with fields.

**You:** Okay, now write the spacebar handler

**Copilot:** Uses the verified API info to write correct code with exact variant names/fields.

---

### Example 2: Exploring Physics Configuration

```
PROTOCOL - QUARTZ -
I need to understand how gravity works in Quartz.
```

Bot boots, loads quartz-ctx MCP.

**You:** Find all APIs related to gravity

**Copilot:** Calls `search_items({"query": "gravity"})` → returns `Gravity` enum, `GravityFalloff`, gravity-related actions.

**You:** Show me all Gravity variants

**Copilot:** Calls `get_variants({"name": "Gravity"})` → lists every variant with docs.

**You:** Now implement gravity falloff for this zone

**Copilot:** Uses the variant list to pick the right one and write code.

---

### Example 3: Finding an Obscure Method

```
I need to rotate a sprite around a pivot point. What's the method?
```

**Copilot:** Calls `search_items({"query": "rotate"})` → finds `RotationOptions` struct, `SetRotation` action, etc.

**You:** Show me RotationOptions

**Copilot:** Calls `get_item({"name": "RotationOptions"})` → reveals all fields and methods.

---

## Performance & Optimization

### Static Generation

- **Time:** ~100-200ms for Quartz engine
- **Output size:** ~100KB of markdown + ~50KB JSON
- **Best for:** Offline reference, version control, fallback access

### Live Server

- **Startup:** Loads API into memory (~1-2 MB), ~50ms first query
- **Response time:** <5ms for typical queries (in-memory lookup)
- **Best for:** Real-time lookups during development

### Recommendations

- Run `generate` once after major API changes (nightly builds, version bumps)
- Keep `serve` running in `.vscode/mcp.json` for interactive sessions
- Review `docs/<scraped-directory>/` locally before sharing or unignoring it intentionally
- Use `PROTOCOL - QUARTZ -` to ensure consistency between static cache and live API

---

## Troubleshooting

### MCP Tool Not Appearing in Copilot

**Check:**
1. Is `quartz-ctx` binary in your PATH? (`which quartz-ctx` or `where quartz-ctx`)
2. If not in PATH, update `.vscode/mcp.json` with absolute path
3. Is `.vscode/mcp.json` valid JSON? (Use a JSON validator)
4. Restart VS Code after any MCP config changes

**Test:**
```bash
echo '{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}' | quartz-ctx serve --source quartz/src
```

If the server starts without error, it's working. Press Ctrl+C to exit.

### Search Results Seem Unsorted

Search results are ranked by relevance:
- Exact name match = highest
- Name contains query = high
- Doc mentions query = medium
- Variant matches = low

Use `get_item` for precise lookups (exact names).

### Static Docs Out of Date

After API changes in `quartz/`:

```bash
cd quartz-ctx
cargo run -- generate --source ../quartz/src --name Quartz
cd ..
git add -f docs/<scraped-directory>/
git commit -m "Update Quartz API docs"
```

### Binary Not Found After Install

If you ran `cargo install --path .`, try:

```bash
cargo install --force --path .
```

Or add Cargo's bin directory to PATH:

```bash
export PATH="$HOME/.cargo/bin:$PATH"  # Unix/Linux/Mac
$env:Path += ";$env:UserProfile\.cargo\bin"  # PowerShell
```

---

## Integration with PROTOCOL - QUARTZ -

When you use `PROTOCOL - QUARTZ -`:

1. **Session boot** activates quartz-ctx MCP (if configured)
2. **Static cache** (`QUARTZ_AI_CACHE.md`) is loaded as fallback reference
3. **Live tool** (`get_variants`) is available for real-time API queries
4. **Consistency check**: If you find something in the live API, verify it against the static cache

Best practice: Use `get_variants` to find exact variant names, then cross-reference with `QUARTZ_AI_CACHE.md`.

---

## FAQ

**Q: Should I use static docs or live server?**  
A: Both! Use live server for real-time lookups during coding. Commit static docs to version control as a backup and for code review context.

**Q: How often should I regenerate docs?**  
A: After any significant Quartz API changes. Can be automated in CI/CD.

**Q: Does quartz-ctx parse my game code?**  
A: No, only the Quartz engine source (`quartz/src/`). It extracts public API items only.

**Q: Can I use it for other Rust projects?**  
A: Yes! It's a generic Rust API scraper. Just point it at any `src/` directory with `generate` or `serve`.

**Q: Why does search sometimes return variant matches?**  
A: Variants are part of the API vocabulary. If an enum variant's name or docs match your search, they're included inline for discoverability.

---

## For Contributors

If you want to improve quartz-ctx:

- **Better search ranking?** See `src/mcp.rs` function `tool_search_items`
- **Different doc layout?** See `src/render/markdown.rs`
- **New tool?** Add it to `tools_list_result()` in `src/mcp.rs` and implement `tool_*()` function

The codebase is small (~1000 lines) and straightforward to extend.

---

## Quick Reference

```bash
# Build
cd quartz-ctx && cargo build --release

# Generate docs
quartz-ctx generate --source quartz/src --name Quartz

# Run live server (test)
quartz-ctx serve --source quartz/src --name Quartz

# Install globally
cargo install --path quartz-ctx

# See all flags
quartz-ctx --help
quartz-ctx generate --help
quartz-ctx serve --help
```

---

## End of Usage Guide
