# Cortex Suite

Two local MCP servers that give a coding agent reliable knowledge of your Rust
codebase. No network calls, no API keys, no telemetry.

| | Owns | How it stays true |
|---|---|---|
| **quartz-ctx** | **Structure** — what the code *is*: types, signatures, enum variants, trait impls, `file:line` | parsed live from source, auto-reloads within ~5s of an edit |
| **cortex** | **Judgment** — what you *learned*: patterns, anti-patterns, decisions, corrections | SQLite, grown from your sessions |

The split is enforced, not conventional: quartz-ctx holds no hand-written
knowledge, and cortex no longer parses Rust itself — it ingests quartz-ctx's
output, so both are fed by one extractor and cannot disagree about what a type is.

## Install

```powershell
.\scripts\setup.ps1 -Workspace C:\code\my-project      # Windows
```
```bash
./scripts/setup.sh ~/code/my-project                    # macOS / Linux
```

Needs [Rust](https://rustup.rs) and a C toolchain (for the tree-sitter grammars —
MSVC Build Tools on Windows, Xcode CLI tools on macOS, `build-essential` on
Linux). Then edit `.cortex/index-sources.json`, run one `index` command, restart
your editor.

Don't want to list crates by hand? `quartz-ctx serve --discover .` finds every
crate under a directory — workspace members and standalone crates alike.

**→ Read [SETUP_HANDOFF.md](SETUP_HANDOFF.md) before you start.** It documents
the pitfalls that cost us real debugging time — stale binaries, cache replay,
config drift between editors, and the PowerShell 5.1 traps.

## What you get

- `get_api_context(hint)` — one budgeted packet of the types, variants and
  signatures relevant to a task, instead of several search round-trips
- `get_anti_patterns(hint)` / `list_patterns(hint)` — the traps and the vetted
  approaches for what you are about to write
- `recall(topic)` — have we solved this before?
- `quartz-ctx generate` — full API sheets in seconds: every type, variant and
  signature, with worked syntax mined from your `examples/` and `#[test]` bodies,
  `file:line` on every item, and a documentation-coverage report that names each
  undocumented item

## The one habit that matters

**Always pass a `hint`.** These tools list everything regardless — the hint
decides what gets expanded. It cuts a session boot from ~34k tokens to ~10k with
nothing dropped, and it is the only thing that records which knowledge actually
proved useful.

## Optional third server

[graphify](docs/GRAPHIFY.md) answers repo-wide architecture questions — module
clusters, hubs, cycles, dependency paths — across every language in the tree, not
just the crates you index.

```bash
cargo install graphify-rs
graphify-rs build --path . --code-only --format json --output .graphify-output
```

Neither cortex nor quartz-ctx requires it. Note that a graph is a **snapshot**:
unlike quartz-ctx it does not re-read source, so rebuild it after significant
changes or it answers confidently and wrongly.

## Languages

| Language | Extractor | Signal |
|---|---|---|
| Rust | `syn` | resolved — types, trait impls, cross-file `impl` blocks |
| Python | tree-sitter | AST only — no type resolution |
| TypeScript / JavaScript | tree-sitter | AST only — no type resolution |

Rust is the strong path. The others are parsed from a concrete syntax tree, so
there is no type resolution or cross-file linking — a genuinely weaker signal,
and treated as one. What it buys is that a Python or TypeScript project no longer
returns *zero items silently*, which is indistinguishable from "this project has
no API".

## Limits

AST-only languages resolve no types, so relationships between their items are
thinner than Rust's. Call edges are recorded for every call site but only become
graph edges when the callee is unambiguous — a method call carries no receiver
type, so edging it would invent ownership.

## Layout

```
cortex/            memory + project intelligence server
quartz-ctx/        API extraction server
templates/         configs and instruction files to copy into your workspace
scripts/           setup.ps1, setup.sh
docs/GRAPHIFY.md   optional third server
```
