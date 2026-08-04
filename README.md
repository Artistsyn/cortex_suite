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

Needs only [Rust](https://rustup.rs). Then edit
`.cortex/index-sources.json`, run one `index` command, restart your editor.

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

## Limits

Rust only — a TypeScript or Python project yields zero items, silently. You list
each crate's `src` yourself; there is no workspace auto-discovery yet.

## Layout

```
cortex/        memory + project intelligence server
quartz-ctx/    API extraction server
templates/     configs and instruction files to copy into your workspace
scripts/       setup.ps1, setup.sh
```
