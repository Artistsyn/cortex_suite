# quartz-ctx — Quick-Start Usage Guide

For the complete setup walkthrough, MCP config details, per-language notes, and troubleshooting, see **[SETUP_HANDOFF.md](../SETUP_HANDOFF.md)**.

---

## Installation

**Build from source:**

```bash
cd quartz-ctx
cargo build --release
# binary: target/release/quartz-ctx
```

**Install globally:**

```bash
cargo install --path quartz-ctx
```

**Use an absolute path in MCP config** (when not in PATH):

```json
"quartz-ctx": {
  "type": "stdio",
  "command": "/path/to/your/project/target/release/quartz-ctx",
  "args": ["serve", "--source", "src", "--name", "MyProject"]
}
```

---

## Mode 1: Generate Static Documentation

```bash
quartz-ctx generate --source src --name MyProject
```

Writes `docs/<name>-ctx/`: `INDEX.md`, `vocabulary.md`, `types.md`, `traits.md`, `functions.md`, `misc.md`, `api-graph.json`.

Useful flags:

```
--output <DIR>    Output root [default: .]
--minimal         Only INDEX, vocabulary, and JSON
--dry-run         Print extracted items, write nothing
```

---

## Mode 2: Live MCP Skill Server

### Configure `.vscode/mcp.json`

```json
{
  "servers": {
    "quartz-ctx": {
      "type": "stdio",
      "command": "quartz-ctx",
      "args": ["serve", "--source", "src", "--name", "MyProject"]
    }
  }
}
```

Restart VS Code after editing MCP config. quartz-ctx is served jointly with **cortex** — both servers are loaded from the same config file (see `SETUP_HANDOFF.md §1`).

### Available tools

| Tool | What it does |
|------|-------------|
| `get_api_context` | Hint-matched summary of types, signatures, and enum variants |
| `get_anti_patterns` | Known mistakes to avoid for the current task (hint required) |
| `list_patterns` | Vetted approaches stored from prior sessions (hint required) |
| `get_preferences` | Recorded preferences relevant to the current task (hint required) |
| `recall` | Free-form lookup across all stored knowledge |
| `trace_across_languages` | Cross-language call/data flow tracing |
| `list_items` | List all public items, optionally filtered by kind |
| `get_item` | Full details for a named item |
| `get_variants` | All variants for a named enum |
| `search_items` | Substring search across names and doc comments |

### Verify it's working

```bash
quartz-ctx selfcheck --source src --name MyProject --json
```

Then ask Copilot: `get_api_context(hint: "...")`. If it returns your types, you're done.

---

## Supported languages

Rust, Python, TypeScript, JavaScript, Go, Java, C#, C/C++, Ruby, PHP.
Rust: full type resolution (`syn`). All others: tree-sitter, `name_resolved` confidence. See `SETUP_HANDOFF.md §7`.

---

## Troubleshooting

**Tool not appearing in Copilot:**
1. Is the binary in PATH? Run `which quartz-ctx` (or `where quartz-ctx` on Windows).
2. Is `.vscode/mcp.json` valid JSON?
3. Did you restart VS Code after changing MCP config?

**Test the server directly:**
```bash
echo '{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}' | quartz-ctx serve --source src
```

See `SETUP_HANDOFF.md §2` for the full troubleshooting guide.
