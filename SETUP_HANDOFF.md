# Cortex Suite — Setup and Handoff

Two MCP servers that give a coding agent reliable knowledge of your codebase.

- **quartz-ctx** — *structure*: what the code **is**. Parsed live from source, so it
  is never stale. Types, signatures, enum variants, trait impls, `file:line`.
- **cortex** — *judgment*: what you have **learned**. Patterns, anti-patterns,
  decisions, corrections. Stored in SQLite, grown from your sessions.

They are deliberately split that way. quartz-ctx holds no hand-written knowledge;
cortex no longer parses Rust itself. `reindex` runs quartz-ctx and ingests its
output, so both are fed by one extractor and cannot disagree about what a type is.

Works on **Rust, Python, TypeScript and JavaScript** projects, on Windows, macOS
and Linux, with Claude Code, VS Code + Copilot, or any MCP-capable host. Rust gets
the strong path (`syn`, fully resolved); the rest are AST-only — see §7.

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

**Requirements:** [Rust](https://rustup.rs), plus a C toolchain for the
tree-sitter grammars used by the non-Rust extractors:

| Platform | What you need |
|---|---|
| Windows | Visual Studio Build Tools with the C++ workload |
| macOS | `xcode-select --install` |
| Linux | `build-essential` (or your distro's equivalent) |

Nothing else. No Python, no Node, no network calls at runtime, no API keys.
Everything is local.

**Tip:** don't want to list crates by hand? `quartz-ctx serve --discover .`
finds every crate under a directory — a Cargo workspace's members and a folder of
standalone crates look identical to it.

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

### 2.7 PowerShell: `$ErrorActionPreference = 'Stop'` kills native commands

This one bit our own setup script, and it is worth knowing before you write any
automation around these tools.

With `$ErrorActionPreference = 'Stop'`, **any** stderr output from a native
executable is raised as a terminating `NativeCommandError`. `cargo` writes every
`Compiling ...` line to stderr, so a completely successful build aborts the
script on the first crate.

```powershell
# WRONG - dies on cargo's normal progress output
$ErrorActionPreference = 'Stop'
cargo build

# RIGHT - relax the preference around the native call, judge it by exit code
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
cargo build
$ErrorActionPreference = $prev
if ($LASTEXITCODE -ne 0) { throw "build failed" }
```

Exit code is the only trustworthy signal for a native process.

### 2.8 PowerShell 5.1 is .NET Framework, not .NET Core

`[System.IO.Path]::GetRelativePath` does not exist there. Neither do a number of
other modern BCL conveniences. Windows PowerShell 5.1 runs on .NET Framework 4.x;
`pwsh` 7+ runs on .NET Core. Code that works in `pwsh` can fail on the shell your
colleagues actually have.

Use `Uri.MakeRelativeUri` for relative paths — available on both.

### 2.9 Never write escape sequences through a shell heredoc

Writing `'\n'` inside a `bash <<EOF` heredoc produces a **real newline** in the
file, not the two-character escape. In Rust that is
`error: character constant must be escaped`; in other languages it silently
breaks a string literal.

This has recurred across three different file types. Use your editor's edit tool,
or build the escape as `chr(92) + 'n'` inside a script file.

### 2.10 Point it at a repo root, not just `src`

quartz-ctx excludes `target`, `node_modules`, `vendor`, `dist`, `.venv`,
`__pycache__` and friends. Before those exclusions existed, scanning a project
root reported **1,239 items against a real API surface of 99** — it was reading
generated bindings out of `target/`.

You can now point it at either the repo root or its `src`; both give the same
answer.

### 2.11 Libraries vs applications — `include_private`

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

> **`hint` is required.** `get_anti_patterns`, `list_patterns` and
> `get_preferences` reject a call without one, and there is no escape hatch.

`get_anti_patterns(hint=...)`, `list_patterns(hint=...)`,
`get_api_context(hint=...)`, `get_preferences(hint=...)`.

Two reasons:

1. **Tokens.** These tools list every entry regardless, but expand only what
   matches the hint. On the reference DB that is ~34k tokens down to ~10k per
   session boot, with nothing dropped.
2. **Learning.** Only hint-matched retrievals count as *targeted*, and only
   targeted retrievals feed pattern survival scoring.

It is required rather than merely documented because documenting it did not work.
Across 823 calls, compliance on the optional hints was 2–5%; on parameters the
schema marked `required` it was 100%. The instruction appeared twice in the
operating manual and was ignored either way. Compliance tracked the schema and
not the prose — so when a rule matters, put it in the schema.

To review everything deliberately, say so: `list_patterns(hint: "auditing all
patterns", detail: "full")`.

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

### What the system asks of you, and when

Closeout runs the consolidation pipeline itself when the last run is more than 8h
old — roughly 4 seconds on a 109-session database. Clustering, skill detection,
gap and survival proposals, drift analysis and trial promotion all happen there,
so the loop does not depend on anyone remembering a command.

**Nothing reaches your instruction files or preferences without a person.**
Automatic promotion only moves a proposal from `trial` to `pending`: into the
review queue, not past it.

Anything waiting on you is printed under **AWAITING YOUR REVIEW** at the end of
closeout and in `get_session_health`, with the command beside it:

```bash
cortex skill-status                      # drafted skills, with their draft paths
```

```bash
cortex skill-approve <name>              # publish one to your skills dir
```

```bash
cortex review-proposals                  # pending proposals; then proposal-approve / proposal-reject <id>
```

The block prints nothing when the queue is empty, so its absence means the queue
is empty — not that the surface is missing.

**Read a draft before approving it.** Skill detection fires on repeated tool
sequences, and a thin signal produces a draft full of placeholder text like
`[describe when to use this]`. Approving that publishes a skill that teaches
future sessions nothing and costs tokens forever. Reject those; keep the drafts
that contain a real procedure. `skill-approve` refuses a draft that still
contains `[Edit: ...]` unless you pass `--force`.

**Where approved skills land — both editors.** The two hosts have no path in
common, so approval writes twice:

| Host | File | How it is used |
|---|---|---|
| Claude Code | `<repo>/.claude/skills/<name>/SKILL.md` | auto-discovered; appears in the agent's skill list |
| VS Code Copilot | `<repo>/.github/prompts/<name>.prompt.md` | invoked as `/<name>` in Copilot Chat |

The prompt file is written only when the repo already has a `.github/`
directory — the suite will not create one for a repo that has none. Copilot has
no skills mechanism and reads nothing under `.claude/`, so publishing to only one
path leaves the skill live for half the team.

Change `skills_dir` under `[skills]` in `.cortex/prefs.toml` if your Claude host
looks elsewhere. Publishing to a directory nothing reads gives you a skill that is
approved, on disk, and invisible to every session — which is what the old default
(`agent_customization/skills`) did.

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
| Empty index for an app | `pub`-only extraction | `include_private: true` (2.11) |
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

## 7. Languages, and what each is worth

| Language | Extractor | Signal |
|---|---|---|
| Rust | `syn` | **resolved** — types, trait impls, cross-file `impl` blocks, full signatures |
| Python | tree-sitter | AST only |
| TypeScript / JavaScript | tree-sitter | AST only |

The distinction is real and worth respecting. `syn` understands Rust; tree-sitter
produces a concrete syntax tree with **no type resolution and no cross-file
linking**. An AST-only item tells you a class exists, its methods, and where —
not what any of it resolves to.

That is still a large improvement on the previous behaviour, which was to return
**zero items silently** for a non-Rust project — an answer indistinguishable from
"this project has no API". Measured on a real FastAPI + React tree: 0 → 192 items.

Visibility follows each language's own convention rather than Rust's: a leading
underscore is internal in Python and JS/TS, `#field` is genuinely private in
modern JS, and `private` / `protected` are honoured in TypeScript.

### Known limits

- **AST-only languages resolve no types**, so relationships between their items
  are thinner than Rust's, and `include_private` matters more for them.
- **Call edges are conservative.** Every call site is recorded in `call_graph`
  with its `file:line`, but a call only becomes a graph edge when the callee
  resolves unambiguously. A method call carries no receiver type, so edging it
  would invent ownership — which is why ~5,500 recorded calls yield ~330 edges.
- `content_store` exists in the schema but is not populated, so the
  compact/expand handle contract is not yet active.
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
