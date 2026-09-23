# quartz-ctx

Extracts the public API surface of a codebase and serves it as a live MCP skill server that Copilot can query during chat.

Supported languages: **Rust, Python, TypeScript, JavaScript, Go, Java, C#, C/C++, Ruby, PHP**.
Rust gets full type resolution via `syn`; the rest are parsed with tree-sitter and linked across files by name (`name_resolved` confidence). See [SETUP_HANDOFF.md §7](../SETUP_HANDOFF.md) for per-language detail.

## Two modes

### `generate` — write a static reference tree

```sh
quartz-ctx generate --source src --output . --name MyProject
```

Writes `docs/<name>-ctx/`: `INDEX.md`, `vocabulary.md`, `types.md`, `traits.md`, `functions.md`, `misc.md`, `api-graph.json`.

### `serve` — live MCP skill server

```sh
quartz-ctx serve --source src --name MyProject
```

Runs a JSON-RPC MCP server over stdio. Used jointly with **cortex** to give the agent both structure (quartz-ctx) and judgment (cortex).

Before every tool call the server re-stats its roots (the same pruned walk the
parser does) and re-parses only files whose size or nanosecond mtime moved, then
re-runs cross-file resolution for the roots that changed. An edit is visible to
the very next call; nothing is served from a timer. A file written within ~3 s of
being read is content-hashed rather than trusted, so two same-size writes inside
one timestamp tick are still caught. Files that fail to parse are named on
not-found answers, since their items are missing.

The extraction core (`parser`, `incremental`, `lang`, `model`) is also a library,
`quartz_ctx`, which cortex links — so both servers read code through one parser.

#### MCP tools

| Tool | What it does |
|------|-------------|
| `get_api_context` | Hint-matched summary of types, signatures, and enum variants |
| `trace_across_languages` | Cross-language call/data flow tracing |
| `list_items` | List all public items, optionally filtered by kind |
| `get_item` | Full details for a named item |
| `get_variants` | All variants for a named enum |
| `search_items` | Substring search across names and doc comments |

## Install

```sh
cargo install --path /path/to/quartz-ctx
```

## Setup

See **[SETUP_HANDOFF.md](../SETUP_HANDOFF.md)** for the authoritative setup walkthrough, MCP config examples, per-language notes, and troubleshooting.
