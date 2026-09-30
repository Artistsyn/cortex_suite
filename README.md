# Cortex Suite

Two local MCP servers that give a coding agent reliable knowledge of your
codebase — in **ten languages**, including the calls that cross between them.
No network calls, no API keys, no telemetry.

| | Owns | How it stays true |
|---|---|---|
| **quartz-ctx** | **Structure** — what the code *is*: types, signatures, enum variants, interfaces, `file:line` | checked against the disk before every answer; only changed files are re-parsed |
| **cortex** | **Judgment** — what you *learned*: patterns, anti-patterns, decisions, corrections | SQLite, grown from your sessions; its code index refreshes itself before every code-backed answer |

**An edit is visible to the very next call, on both servers.** Nothing waits on a
timer, a `reindex`, a restart or a git commit. Each server decides freshness when
it answers, by the cheapest check that can settle it — file metadata, then a
content hash, then a re-parse, then an API comparison — so an unchanged tree
costs a few milliseconds and an edit costs its own files. What changed is kept
in a change journal: `get_delta` reports it per API item (no git needed), and a
stored pattern that names code changed since it was written says so when it is
served.

The split is enforced, not conventional: quartz-ctx holds no hand-written
knowledge, and cortex no longer parses code itself — it ingests quartz-ctx's
output, so both are fed by one extractor and cannot disagree about what a type is.

**Knowledge also arrives without being asked for.** In Claude Code, and in
VS Code Copilot through `hooks-init --vscode`, cortex installs hooks that watch
the work itself. When an edit touches a recorded trap,
or a build or test fails in a way the store already knows, the agent is told at
that moment, in the one hook-output form the model is actually shown. Every
build/test verdict is recorded as it happens, so "is this getting better?" is
answered from what was observed, not from what anyone reported. See
[Memory that arrives on its own](#memory-that-arrives-on-its-own).

## Languages

Rust through `syn`; Python, TypeScript, JavaScript, Go, Java, C#, C/C++, Ruby and
PHP through tree-sitter. Every front end feeds the **same** project-wide
resolution pass, so a Go method whose receiver type is three files away, a C++
member defined out of line in a `.cpp`, and a C# `partial` half all reach their
type. Interfaces and base classes populate trait-implementation lookups, and call
edges are extracted for every language.

Each item carries what it is **and where it came from** — language, source root,
`file:line` — so `Canvas` the Rust struct and `Canvas` the TypeScript class are
never confused. When several declarations share a name, you get all of them with
their provenance rather than whichever happened to be first.

Every item carries a confidence tag. Read it as a statement about the SOURCE,
not the parser: no front end here infers types — `syn` parses Rust exactly as
tree-sitter parses the rest, and every language is linked across files by name in
the same pass. `resolved` (Rust) means the language requires declared types, so
signatures and fields are complete as written; `name_resolved` means the language
may leave them out (a JavaScript parameter has no type unless JSDoc gives one),
so an empty type is "not declared", not "unknown to us"; `ast_only` means one file,
no cross-file linking.

### Calls that cross the language boundary

A call graph that stops at the language boundary stops where the interesting
questions start. `trace_across_languages` joins HTTP routes to the
`fetch`/`axios`/`requests` calls that hit them, and wasm/FFI exports to the code
that imports them, naming the item at each end.

Path parameters normalise across every syntax, so FastAPI's
`/api/models/{model_path:path}/animation` matches a JavaScript template literal
`` `/api/models/${modelPath}/animation` ``. FFI keys fold case and underscores,
because wasm-bindgen renames `auto_detect_chains` to `autoDetectChains`.

**The unmatched halves are reported too**, and they are usually the finding: a
call with no route behind it (a rename applied on one side only), a route nothing
calls, a caller using a verb the route does not declare. No single-language tool
can see those — the compiler included — because neither side is wrong on its own.

```
quartz-ctx boundaries --source .
```

**This depends entirely on how far up you point each root.** For a Rust library,
point at its `src` — that is the project. For anything with more than one
language in it, point at the **application directory**: a web app whose backend
is `server.py` and whose frontend is `frontend/src` is **one** root.

Rooting at `app/frontend/src` indexes the callers and not the routes they call,
so every call reports as *no matching route* — which is indistinguishable from a
genuinely broken call. The reference workspace shipped exactly this mistake and
reported five real, working endpoints as orphaned. The same applies across the
FFI boundary: a wasm crate and the JavaScript importing it must both be listed,
or you get two lists of false findings describing one working boundary.

Pointing at an app root is safe. Build output is rejected by **shape** — a
content-hash filename, or any line over 5,000 characters — not by folder name,
so bundles are skipped even when they sit somewhere no blocklist would look. The
skip count is always printed.

`.cortex/index-sources.json` carries all of this in its own comment block, and
`boundaries` is the check: a long *calls with no matching route* list usually
means a root is missing from the manifest, not that the code is broken.

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

Setup installs a launcher into `.cortex/` for both platforms — `cortex.sh` and
`cortex.ps1`, same commands on each:

```bash
./.cortex/cortex.sh reindex      # full rebuild of every configured source (first run)
./.cortex/cortex.sh refresh      # re-index only what changed (the servers do this themselves)
./.cortex/cortex.sh check-mcp    # confirm both MCP configs agree and use relative paths
./.cortex/cortex.sh deploy       # rebuild without stopping the running server
```

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
- `set_checkpoint` / `get_checkpoint` — save where a long task stands and pick it
  up again after a context reset
- **Pushed traps** — at an edit and at a failure, with no call needed (below)
- `cortex scoreboard` — observed outcomes, repeat failures, what reached the
  agent, and the token bill actually paid; `cortex fired` — which mechanisms have
  really run
- `quartz-ctx generate` — full API sheets in seconds: every type, variant and
  signature, with worked syntax mined from your `examples/` and `#[test]` bodies,
  `file:line` on every item, and a documentation-coverage report that names each
  undocumented item

## The one habit that matters

**Pass a `hint`.** `get_anti_patterns`, `list_patterns` and `get_preferences`
require one. These tools list everything regardless: the hint decides what gets
expanded, entries sharing no word with it are cut to one line, and `since`
makes a repeat call count unchanged entries instead of re-listing them. Measured
on a ~400-entry store, a first hinted `get_anti_patterns` returns about 10k tokens
and a `since` repeat about 2–3k. A hinted call is also the only thing that
records which knowledge actually proved useful. The one-line index grows with the
store, which is why the next step is a small session-start brief with everything
else pulled on demand — see [the plan](docs/cortex-upgrade-plan-2026-09-27.md).

## Memory that arrives on its own

A trap in the store only helps if the agent thinks to ask, and it asks least when
it is most sure. So in Claude Code, cortex installs hooks into
`.claude/settings.local.json` the first time it serves a project. They are
versioned, so upgrades re-install themselves; `cortex hooks-init` installs or
upgrades them by hand.

| Hook | When | What the agent gets |
|---|---|---|
| `edit_guard` | after an Edit or Write | one short warning when the edit shares **distinctive** evidence with a recorded trap: a code identifier plus one more rare token, or three rare words. Ordinary English, library identifiers and prose files never count. At most one per file and four per session. |
| Bash observer | after every command, **including failed ones** (`PostToolUseFailure`) | nothing, usually; every build/test verdict is recorded. When a failure matches a trap — the one linked to it by `anti-pattern add --resolves`, or one its error message names — the agent gets the trap and its fix. A specific failure seen in three sessions with nothing recorded gets a nudge with the exact command to record it. |
| `note_challenge` | on each user message | records a disputed claim as an open question, to be settled by checking |

Two facts shaped all of this, and both were learned the hard way. Claude Code
shows the model **only** `hookSpecificOutput.additionalContext` from these hooks:
plain hook output goes to a debug log, and for months cortex's warnings went
there. And a command that exits non-zero never reaches `PostToolUse`, so an
observer that listens only there sees nothing but the failures a pipe happened
to hide. No hook can shrink a Bash result, so cortex does not claim to save
tokens that way.

The matching was chosen by replaying real history: over 2,403 recorded edits,
the guard now speaks on 32% of them instead of 91%, and a labelled sample of its
warnings went from about half relevant to about nine in ten.

**VS Code Copilot gets the same pushes.** VS Code's agent hooks run commands,
not MCP tools, so `cortex hook <event>` is the same logic as a plain command. It
reads the hook's JSON on stdin (Claude Code's or VS Code's shape), and prints
the reply or nothing. It always exits 0 and never creates a store.

```bash
./.cortex/cortex.sh hooks-init --vscode     # writes .github/hooks/cortex.json
```

It reads VS Code's terminal tool (`run_in_terminal`) and its edit tools
(`replace_string_in_file`, `multi_replace_string_in_file`, `create_file`,
`apply_patch`, `edit_notebook_file`), plus user prompts. VS Code reports no
exit codes and marks failed commands successful, so failures are read from the
output text. Everything else, such as reads and searches, exits before the store
is opened: about 3 ms of its own, 6–30 ms as VS Code's hook log reports it. A
call that does work takes about 35 ms. It is safe under VS Code's concurrent
hooks: 90 simultaneous calls all answered correctly.

The timing differs from Claude Code, which waits for every hook. VS Code waits
for PreToolUse but starts PostToolUse and moves on, so a PostToolUse reply is
attached to the tool's result after the next request has already gone out: the
model sees it one request late, and never if that request ends the turn. The
edit guard therefore answers at PreToolUse, since the edit's text is all it
reads, and counts a warning as delivered only when the edit's PostToolUse shows
the edit went through, so a failed edit cannot silence its retry. A failing
command's output exists only afterwards, so in VS Code that note arrives one
request later than it does in Claude Code.

### Limits on record: the walls ledger

An agent's most expensive mistake is not a bug. It accepts a limit that
isn't one, then writes it down, and every later session reads it as settled.
Measured on the author's workspace: every limit that was later tested did not
hold as stated. wgpu's 256-layer default had been reported as a hardware
limit, and a multiview restriction disappeared in a newer release. The check
that moved each one was cheap.
(`docs/frontier-plan-2026-09-29.md`.)

cortex keeps limits as **walls**. Each wall carries:

- **whose limit it is:** physics, hardware, platform, library default, library
  version, our design, existing implementations, authority or budget (the last
  six are movable);
- **its evidence**, each item dated;
- **while open, the cheapest test** that would decide it.

Two rules are enforced, not suggested:

- **Evidence.** A wall can't be recorded without a provenance and evidence.
  Evidence that is only inferred, only an authority, or only someone else's
  implementation leaves it open.
- **Verdicts.** A verdict changes only with a new fact, a measurement or a dated
  source, in either direction. Challenged models flip about half their answers
  either way.

Walls reach agents at the moments they decide:

- **In retrieval.** `get_context` and `get_anti_patterns` serve the walls a task
  touches, each one line, together with how often limits here moved when
  checked. `get_walls(hint)` lists them; `record_wall` and `update_wall` change
  them.
- **When the user pushes back.** When you dispute a limit ("more doable than you
  wrote off", "was 30 even the latest version?"), the challenge hook injects the
  wall audit with the walls on record, instead of a generic reminder. The cues
  were chosen by replaying 2,195 real messages: 26 fire, and 24 of them were real
  limit pushbacks.
- **When the dispute is settled.** It must name the wall it was about.
- **In the review block.** Walls whose revisit date has come are listed there.
- **When a dependency moves.** An edit to `Cargo.toml`, `Cargo.lock` or
  `package.json`, or a `cargo update` whose output reports a new version,
  pushes every open or held wall bound to that package back to the agent, once
  per session. "Bound" means the package is named in its revisit condition, or
  in the claim or topic of a version-bound wall.
- **Cheap tests are flagged.** An open wall whose cheapest test takes 30 minutes
  or less is called out ("Cheap to settle now: #2 (~20 min)").

`setup.sh` / `setup.ps1` also install the `frontier` skill, which walks the audit:
`.claude/skills/frontier/SKILL.md` for Claude Code and
`.github/prompts/frontier.prompt.md` for Copilot.

To record a limit from a session, use a `[CORTEX-WALL: ...]` marker. From the
command line: `cortex walls list | show <id> | import <file.json>`.

### Knowledge that commits itself

Knowledge used to wait for you to reply `KNOWLEDGE COMMITTED` at the end of a task.
Measured on 2026-09-30, that approval passed 99.3-100% of what reached it. The
protocol around it lost 123 of 411 markers across context compactions, before any
closeout could include them. So the safety moved from approving each entry to
things the loop cannot talk its way past:

- **Captured when written.** On Claude Code, the Stop and PreCompact hooks read
  the transcript from where they last stopped and commit each `[CORTEX-*]` marker
  through the closeout gates. Fenced code and placeholder examples are skipped.
- **Every automatic change is a row** in a ledger (`cortex knowledge changes`),
  with what it replaced and why. Nothing is deleted: a retracted entry keeps its
  row and leaves every serving path, including the response cache.
- **Restatements merge, older drafts don't win.** A marker that restates a live
  entry (the same pattern name with new text, or cosine >= 0.9) replaces it only
  if it was written later. Catching up on a transcript meets drafts that a later
  version had already replaced.
- **You audit a sample.** `cortex knowledge audit` shows 5 random automatic
  commits; wrong or useless retracts on the spot, and more than 3 bad in the
  last 20 switch automatic commit off.
- **Look-alikes are reconciled, not alarmed.** A new entry that reads close to an
  older one (cosine 0.25-0.9) opens a pair: its author hears on the next prompt, and
  anyone served either entry sees one line until `resolve_pair` says duplicate,
  refinement, conflict or compatible. Entries are *disputed* only by events: a
  challenge that proved one wrong, a wall it cites that moved, or its failure
  coming back after its fix was delivered. A conflict is settled only by a fact.
- **Backfill.** `cortex knowledge backfill` finds markers written in past
  transcripts that never reached the store; `--write` commits them tagged
  `backfill`, after a backup, and `undo --class backfill` takes them all back.
  Coverage on this workspace went from 65% to 99%.

Skill drafts are triaged the same way (`cortex knowledge skills`). A detector
template with its placeholders is rejected. An authored, concrete draft goes out
as a trial, and is kept only if something invokes it within 60 days.

Once a week a scheduled session (`cortex-weekly-maintenance`) judges look-alike
pairs and writes the digest (`cortex knowledge digest`). Its verdicts count only
after it agrees with hand labels it cannot see, at 90% or better. Cue changes are
still made by hand; `cortex knowledge cues` replays any candidate over every real
prompt first. An automated miner was tested and found nothing it could promote.

The design, the evidence behind it and the later phases are in
[docs/self-learning-loop-2026-09-30.md](docs/self-learning-loop-2026-09-30.md).

## Is it working?

```bash
./.cortex/cortex.sh scoreboard     # 14-day window vs the previous one
./.cortex/cortex.sh fired          # has each mechanism actually run?
```

Every scoreboard number says whether it is **observed** (recorded by hooks),
**delivered** (reached the agent) or **self-reported** (closeouts, shown for
reference only). It covers:

- build/test runs that went green;
- how many of this window's failures had happened before the window, which is
  what memory should prevent;
- lookups, with identical repeats flagged as a loop;
- pushes delivered, confirmed from the host's own transcript records;
- the token bill read from Claude Code's transcripts, including where the cost
  comes from: context size × calls.

## What you have to review

Knowledge entries commit automatically; your part is the weekly sample audit
(`cortex knowledge audit`, above). The consolidation pipeline runs itself at
closeout when it has gone stale, and nothing it produces is committed without you. Drafted skills and pending
proposals are listed under **AWAITING YOUR REVIEW** in the closeout report and in
`get_session_health`, each with the command that resolves it. Read a draft before
approving it — see [SETUP_HANDOFF.md](SETUP_HANDOFF.md#3-daily-use).

## Optional third server

[graphify](docs/GRAPHIFY.md) answers repo-wide architecture questions — module
clusters, hubs, cycles, dependency paths — across every language in the tree, not
just the crates you index.

```bash
cargo install graphify-rs
graphify-rs build --path . --code-only --format json --output .graphify-output
```

Neither cortex nor quartz-ctx requires it. `graphify-rs serve` loads its graph
once and never re-reads it, so serve it through `cortex graphify-serve`, which
rebuilds and reloads the graph when the source has moved past it — see
[docs/GRAPHIFY.md](docs/GRAPHIFY.md).

## Languages

| Language | Extractor | Tag | What comes out |
|---|---|---|---|
| Rust | `syn` | `resolved` | types, fields, variants, methods (cross-file `impl` blocks attached by name + module proximity), trait impls, calls |
| TypeScript | tree-sitter | `name_resolved` | classes, interfaces (fields), enums (variants), type aliases, functions incl. `const f = () =>`, typed signatures, JSDoc |
| JavaScript / JSX | tree-sitter | `name_resolved` | classes with class-body fields AND `this.x = ...` fields, functions incl. arrow consts and components, JSDoc (docs above `export` included) |
| Python | tree-sitter | `name_resolved` | classes, fields from the class body and from `self.x = ...` (typed from `__init__` annotations), `Enum` subclasses as enums, typed signatures, docstrings |
| Go, Java, C#, C / C++, Ruby, PHP | tree-sitter | `name_resolved` | declarations, members, bases and interfaces; Go receivers, C++ out-of-line members and C# `partial` halves attached across files |

Measured on a real FastAPI + React level editor (the reference workspace) after
the 2026-09-23 parity pass:
JavaScript types with fields 0% → 100%, JavaScript items with docs 11% → 52%.
The remaining gap is what the source declares — plain JavaScript has no types to
extract except what JSDoc states (and JSDoc lines arrive in the item's doc) — not
what the extractor reads. Two same-named types in one project are listed together
with their provenance rather than guessed between, in every language.

Visibility follows each language's own convention rather than Rust's — a leading
underscore in Python and JS/TS, `#field` in modern JS, `private` / `protected`
where they are spelled, capitalisation in Go, and package-private / implicit
private in Java and C# — and it filters **methods and fields alike**. Interface
members are read as public even though they carry no modifier, because they
cannot carry one. Point `include_private` at applications and at every non-Rust
root: a language with no `pub` returns almost nothing under a library view.

## Limits

No language's types are inferred, Rust's included: relationships are linked by
name, so where the source leaves a type out (untyped JavaScript, un-annotated
Python) there is less to link. Call edges are recorded for every call site but
only become graph edges when the callee is unambiguous — a method call carries no
receiver type, so edging it would invent ownership.

## Layout

```
cortex/            memory + project intelligence server
quartz-ctx/        API extraction server
templates/         configs and instruction files to copy into your workspace
scripts/           setup.ps1, setup.sh
docs/GRAPHIFY.md   optional third server
docs/cortex-upgrade-plan-2026-09-27.md   audit, research and roadmap (the tracker)
```
