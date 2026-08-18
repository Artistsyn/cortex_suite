# Cortex Suite

Two local MCP servers that give a coding agent reliable knowledge of your
codebase — in **ten languages**, including the calls that cross between them.
No network calls, no API keys, no telemetry.

| | Owns | How it stays true |
|---|---|---|
| **quartz-ctx** | **Structure** — what the code *is*: types, signatures, enum variants, interfaces, `file:line` | parsed live from source, auto-reloads within ~5s of an edit |
| **cortex** | **Judgment** — what you *learned*: patterns, anti-patterns, decisions, corrections | SQLite, grown from your sessions |

The split is enforced, not conventional: quartz-ctx holds no hand-written
knowledge, and cortex no longer parses code itself — it ingests quartz-ctx's
output, so both are fed by one extractor and cannot disagree about what a type is.

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

Confidence is a three-way tag, not a boolean: `resolved` (a real front end agreed
the types are these types) → `name_resolved` (cross-file linking by name, no type
inference) → `ast_only`. An agent cannot calibrate what it is told if everything
arrives with the same authority.

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
./.cortex/cortex.sh reindex      # re-extract and re-index every configured source
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
- `quartz-ctx generate` — full API sheets in seconds: every type, variant and
  signature, with worked syntax mined from your `examples/` and `#[test]` bodies,
  `file:line` on every item, and a documentation-coverage report that names each
  undocumented item

## The one habit that matters

**Pass a `hint`.** `get_anti_patterns`, `list_patterns` and `get_preferences`
require one. These tools list everything regardless — the hint decides what gets
expanded. It cuts a session boot from ~34k tokens to ~10k with nothing dropped,
and it is the only thing that records which knowledge actually proved useful.

## What you have to review

The consolidation pipeline runs itself at closeout when it has gone stale, but
nothing it produces is committed without you. Drafted skills and pending
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

Neither cortex nor quartz-ctx requires it. Note that a graph is a **snapshot**:
unlike quartz-ctx it does not re-read source, so rebuild it after significant
changes or it answers confidently and wrongly.

## Languages

| Language | Extractor | Signal |
|---|---|---|
| Rust | `syn` | `resolved` — types, trait impls, cross-file `impl` blocks |
| Python, TypeScript / JavaScript, Go, Java, C#, C / C++, Ruby, PHP | tree-sitter | `name_resolved` — declarations, members, bases and interfaces, linked across files by name |

Rust is the strong path. The others are parsed from a concrete syntax tree and
then linked by the same project-wide resolution pass, which is a real resolution
step — but by name, not by type, so two same-named types in one project can be
told apart wrongly. The tag says which you are reading; do not treat
`name_resolved` as `resolved`.

Visibility follows each language's own convention rather than Rust's — a leading
underscore in Python and JS/TS, `#field` in modern JS, `private` / `protected`
where they are spelled, capitalisation in Go, and package-private / implicit
private in Java and C# — and it filters **methods and fields alike**. Interface
members are read as public even though they carry no modifier, because they
cannot carry one. Point `include_private` at applications and at every non-Rust
root: a language with no `pub` returns almost nothing under a library view.

## Limits

Only Rust resolves types, so relationships between items in the other languages
are thinner. Call edges are recorded for every call site but only become graph
edges when the callee is unambiguous — a method call carries no receiver type, so
edging it would invent ownership.

## Layout

```
cortex/            memory + project intelligence server
quartz-ctx/        API extraction server
templates/         configs and instruction files to copy into your workspace
scripts/           setup.ps1, setup.sh
docs/GRAPHIFY.md   optional third server
```
