# Cortex Suite — Setup and Handoff

Two MCP servers that give a coding agent reliable knowledge of your codebase.

- **quartz-ctx** — *structure*: what the code **is**. Parsed live from source, so it
  is never stale. Types, signatures, enum variants, trait impls, `file:line`.
- **cortex** — *judgment*: what you have **learned**. Patterns, anti-patterns,
  decisions, corrections. Stored in SQLite, grown from your sessions.

They are deliberately split that way. quartz-ctx holds no hand-written knowledge;
cortex no longer parses Rust itself. `reindex` runs quartz-ctx and ingests its
output, so both are fed by one extractor and cannot disagree about what a type is.

Works on **any Rust project**, on Windows, macOS and Linux, with Claude Code,
VS Code + Copilot, or any MCP-capable host.

---

## 1. Quickstart

```bash
git clone <this-repo> cortex_suite
cd cortex_suite
```

**Windows (PowerShell):**
```powershell
.\scripts\setup.ps1 -Workspace C:\code\my-project
```

**macOS / Linux:**
```bash
chmod +x scripts/setup.sh
./scripts/setup.sh ~/code/my-project
```

The script builds both binaries, writes `.mcp.json`, `.vscode/mcp.json`,
`.cortex/index-sources.json`, `CLAUDE.md` and `.github/copilot-instructions.md`
into your workspace, and never overwrites an existing file unless you pass
`-Force` / `--force`.

Then:

1. **Edit `.cortex/index-sources.json`** — list your crates. This is the only file
   you must edit by hand.
2. **Index once:**
   ```bash
   cortex --db .cortex/memory.db index --source my_crate/src --name MyCrate
   ```
3. **Restart your editor** so it re-reads the MCP config.
4. **Verify:** ask the agent `get_api_context(hint: "...")`. If it returns your
   types, you are done.

**Requirements:** Rust (https://rustup.rs) and nothing else. No Python, no Node,
no network calls at runtime, no API keys. Everything is local.

---

## 2. The pitfalls

Every one of these cost real debugging time. They are ordered by how likely you
are to hit them.

### 2.1 A running server holds its own binary — rebuilds silently do nothing

**Symptom:** `cargo build` fails with `failed to remove ... Access is denied
(os error 5)`, or appears to succeed while behaviour never changes.

The MCP server process keeps an open handle on the executable, so the linker
cannot replace it. On Windows the build errors; the dangerous part is that the
**old binary is still there**, so if you ignore the error the next run uses stale
code.

**Fix — the deploy sequence that works:**

```powershell
Get-Process cortex, quartz-ctx -ErrorAction SilentlyContinue | Stop-Process -Force
cargo build                                  # cortex
cargo build --release                        # quartz-ctx
```
```bash
pkill -x cortex; pkill -x quartz-ctx
cargo build && cargo build --release
```

Then **reindex**, then **clear the response cache** (see 2.2).

> `cargo test` builds a *separate* test binary. Passing tests do **not** mean the
> CLI or the MCP server changed. Verifying behaviour through the executable
> always needs an explicit `cargo build`.

Always confirm the artifact rather than the exit code:
```bash
cortex --version
```

### 2.2 The response cache replays pre-rebuild output

**Symptom:** you fix a tool, rebuild, and the tool behaves exactly as before.

cortex caches tool responses keyed on an index version. Historically that key
described only the *data*, so changing how a tool *renders* data left every
cached answer "valid" and the rebuilt binary replayed the old output. This cost a
full false-negative debug cycle on a fix that was already correct.

Current builds mix the binary's identity into the key, so this should not recur.
If you ever suspect it:

```bash
sqlite3 .cortex/memory.db "DELETE FROM response_cache;"
```

Nothing durable lives in that table.

### 2.3 MCP config: the subcommand must come first

**Symptom:** the server "fails to start" with no useful message.

```jsonc
// WRONG - clap exits 2, no MCP handshake ever happens
"args": ["--source", "src", "--name", "X"]

// RIGHT
"args": ["serve", "--source", "src", "--name", "X"]
```

Related: **do not mix command families.** Either point `command` at the binary and
pass *its* flags, or point it at `powershell`/`bash` and pass *script* flags.
Mixing them (`command: cortex.exe`, `args: ["-File", "script.ps1"]`) starts a
process that dies immediately.

### 2.4 Two config files that silently drift

Claude Code reads `.mcp.json`. VS Code reads `.vscode/mcp.json`. They have
*different shapes* — `mcpServers` vs `servers`, and VS Code needs
`"type": "stdio"`.

On the reference workspace these drifted for weeks: quartz-ctx was given three
source roots in VS Code and one in Claude Code. Same tool, same question,
different answers depending on the editor, and **no error anywhere**.

Both templates in `templates/` are pre-aligned. If you edit one, edit the other.
The `--sources-from` flag exists exactly so the *root list* only lives in one
place.

Never bake live counts ("10,529 nodes") into a `description` string. They are
correct for one day.

### 2.5 Paths

- Relative to the **workspace root** (where you launch the editor), not to the
  config file.
- Forward slashes work on Windows and survive JSON escaping. Use them.
- Windows binaries need `.exe`; macOS/Linux do not.
- If your workspace uses **junctions or symlinks**, tools traverse them normally,
  but a plain directory listing can miss the contents. Verify with a direct `ls`
  of the link target before concluding a source is missing.

### 2.6 PowerShell 5.1 specifics

The default Windows PowerShell (not `pwsh`) has traps that produce silent
corruption rather than errors:

| Trap | Effect | Fix |
|---|---|---|
| `&&` chaining | parse error | use `;` or `if ($?) { ... }` |
| `2>&1` on a native exe | wraps stderr in ErrorRecords, sets `$?` false on a clean exit | don't redirect; stderr is captured already |
| Multi-line string as a CLI arg | each newline becomes a **separate argument** | keep CLI flag values single-line |
| Em-dash in an arg value | some parsers read it as a flag separator | use ASCII `-` |
| `.ps1` saved UTF-8 **without** BOM | PS 5.1 reads it as Windows-1252; a UTF-8 em-dash contains byte `0x94` = `"`, silently terminating a string mid-content | save with BOM, or avoid non-ASCII in double-quoted strings |
| `$PID`, `$Error`, `$Host` as variable names | collide with automatic variables | pick other names |

You will see `NativeCommandError` noise when a script pipes a native exe's
output. If each step still reports `done`, that is cosmetic.

### 2.7 Never write escape sequences through a shell heredoc

Writing `'\n'` inside a `bash <<EOF` heredoc produces a **real newline** in the
file, not the two-character escape. In Rust that is
`error: character constant must be escaped`; in other languages it silently
breaks a string literal.

This has recurred across three different file types. Use your editor's edit tool,
or build the escape as `chr(92) + 'n'` inside a script file.

### 2.8 Point it at a repo root, not just `src`

quartz-ctx excludes `target`, `node_modules`, `vendor`, `dist`, `.venv`,
`__pycache__` and friends. Before those exclusions existed, scanning a project
root reported **1,239 items against a real API surface of 99** — it was reading
generated bindings out of `target/`.

You can now point it at either the repo root or its `src`; both give the same
answer.

### 2.9 Libraries vs applications — `include_private`

A library publishes its API through `pub`. An **application does not**, so
`pub`-only extraction returns a nearly empty index for one. Measured:

| Project type | `pub`-only shows |
|---|---|
| Engine / library | 55% |
| CLI application | 33% |
| GUI application | **19%** |

Set `"include_private": true` per target in `index-sources.json` for apps and
binaries; leave it off for libraries so their indexed surface stays the API they
actually promise. The setting is **per root**, so one server can serve a library
at its API surface and an app at full structure simultaneously.

---

## 3. Daily use

### The rule that matters most

> **Always pass a `hint`.**

`get_anti_patterns(hint=...)`, `list_patterns(hint=...)`,
`get_api_context(hint=...)`, `get_preferences(hint=...)`.

Two reasons:

1. **Tokens.** These tools list every entry regardless, but expand only what
   matches the hint. On the reference DB that is ~34k tokens down to ~10k per
   session boot, with nothing dropped.
2. **Learning.** Only hint-matched retrievals count as *targeted*, and only
   targeted retrievals feed pattern survival scoring. Measured hint compliance was
   **3%**, which is why only 10 of 160 patterns had any usage data. A hintless
   `list_patterns` now tells you it recorded no signal.

### Typical session

```
1. get_api_context(hint: "what you are about to build")   # structure
2. get_anti_patterns(hint: same)                          # known traps
3. list_patterns(hint: same)                              # vetted approaches
4. ... write code ...
5. when stuck: recall(topic) BEFORE trying a second approach
```

### Capturing knowledge

Embed markers in your responses as you discover things:

```
[CORTEX-AP: description="..." tags="..."]wrong: ...\ncorrect: ...[/CORTEX-AP]
[CORTEX-PATTERN: name="..." intent="..." trust="verified"]body[/CORTEX-PATTERN]
[CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
```

Then at the end of a verified task, the agent presents a summary and you reply
`KNOWLEDGE COMMITTED` to commit them.

> On Claude Code the agent **must** pass its markers as `markers_text` to
> `closeout_session`. There is no chat store to scrape outside VS Code, so
> omitting it commits nothing and reports success.

### Regenerating API sheets

```bash
quartz-ctx generate --source my_crate/src --name MyCrate
```

Writes `docs/<name>/` — INDEX, vocabulary (enums), types, traits, functions, and
`api-graph.json`. Each item carries `file:line`; examples are mined from
`examples/`, `tests/` and `#[test]` bodies; `INDEX.md` ends with a documentation
coverage report naming every undocumented item, which doubles as a worklist.

---

## 4. Writing the instruction files

`CLAUDE.md` and `.github/copilot-instructions.md` are what make this reliable
rather than merely available. Templates are in `templates/`. What matters:

**Do:**
- State the **routing rule** in one place: structure → quartz-ctx, judgment →
  cortex.
- Make the pre-code check explicit and unconditional — check anti-patterns
  *before* writing a factory/tick/spawn/physics function, not after it fails.
- Say **when to re-check mid-task**: after the first failed approach, before the
  third. The most valuable lookup is the one you skip because you are confident.
- Keep it short. A long manual is skimmed.

**Don't:**
- **Don't list tool names you have not verified exist.** The single worst failure
  we had: after removing nine tools, `CLAUDE.md` still routed agents to them for
  an hour. A manual describing a vanished tool surface misdirects every session,
  and nothing errors.
- Don't duplicate the same rule in three places — they drift.
- Don't write live counts into prose.

After changing any tool surface, grep your instruction files for the removed
names.

---

## 5. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Server "not connected" | binary missing, or `serve` not first in args | check the path exists; see 2.3 |
| Tool behaves as before a fix | stale binary or cached response | 2.1 then 2.2 |
| `Access is denied (os error 5)` | server holds the binary | stop processes first (2.1) |
| Editor sees different data than Claude Code | config drift | 2.4 |
| Type returns 0 methods | index predates the cross-file impl fix | rebuild and reindex |
| A name returns the wrong type | several indexed types share it | pass `scope=`, or the full unit id |
| Index has units from deleted sources | indexing never prunes | `cortex prune-index --keep <root> ... --apply` |
| Empty index for an app | `pub`-only extraction | `include_private: true` (2.9) |
| 0 items on a non-Rust project | Rust only, today | not yet supported |

**Health checks:**
```bash
cortex --db .cortex/memory.db doctor
cortex --db .cortex/memory.db status --full
quartz-ctx selfcheck --source my_crate/src --json
```

---

## 6. Optional: graphify

A third server for cross-crate architecture questions — module clusters, hubs,
cycles, dependency paths — across the whole repository rather than the crates you
list in `index-sources.json`.

```bash
cargo install graphify-rs
graphify-rs build --path . --code-only --format json --output .graphify-output
```

Then add it to both MCP configs. Neither cortex nor quartz-ctx requires it.

**`--output` is not optional.** By default graphify writes to a hashed global
cache (`~/.graphify-rs/<project>-<hash>/`), not into your project — so an MCP
config pointing at `.graphify-output/graph.json` would never find a file. Check
`ls .graphify-output/graph.json` before wiring it up.

**→ [docs/GRAPHIFY.md](docs/GRAPHIFY.md)** for the config snippets, the tools
worth knowing, and its pitfalls — chiefly that a graph is a **snapshot**: unlike
quartz-ctx it does not re-read source, so a stale graph answers confidently and
wrongly until you rebuild.

## 7. Known limits

- **Rust only.** A TypeScript or Python project yields **zero items, silently**.
  Multi-language extraction is designed but not built.
- **No workspace auto-discovery.** List each crate's `src` in
  `index-sources.json` yourself.
- `call_graph` and `content_store` exist in the schema but are not populated, so
  `explain_dependency_path` and `simulate_change` answer from type-reference
  edges only.
- Indexing is full, not incremental. Fine at a few thousand items; a very large
  monorepo will be slower.

---

## 8. What is in this repo

```
cortex_suite/
├── SETUP_HANDOFF.md      this file
├── README.md             one-page orientation
├── cortex/               memory + project intelligence server (Rust)
├── quartz-ctx/           API extraction server (Rust)
├── templates/            configs and instruction files to copy
├── scripts/              setup.ps1 (Windows), setup.sh (macOS/Linux)
└── docs/GRAPHIFY.md      optional third server: repo-wide structural graph
```

Both servers are plain Rust binaries with no runtime dependencies. Everything is
local: no network calls, no telemetry, no API keys. `.cortex/memory.db` is yours
and is not shared by anything here.
