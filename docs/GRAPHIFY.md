# graphify — optional third server

**Optional.** cortex and quartz-ctx work without it. Add graphify when you need
cross-crate architecture questions that neither of the other two answers well.

## What it adds

The three servers answer different questions, and the boundary is worth knowing
before you install a third one:

| Question | Server |
|---|---|
| "What is `Canvas`? What methods does it have?" | **quartz-ctx** — parsed live from source |
| "Have we hit a problem with this before?" | **cortex** — learned judgment |
| "Which modules cluster together? What are the hubs? Are there cycles?" | **graphify** |

graphify builds a whole-repository graph across **every** language and file type
it can read, not just the crates you list in `index-sources.json`. On the
reference workspace it covered 20 projects where cortex's index covered 11
configured Rust roots — including the tooling itself. That breadth is the reason
to have it, and also the reason not to use it for API facts: it is a **structural
map, not ground truth about signatures**.

## Install

```bash
cargo install graphify-rs
```

Installs `graphify-rs` to `~/.cargo/bin`. Verified against **v0.8.0**. No other
dependencies; `--no-llm` / `--code-only` keep it fully local.

## Build a graph — `--output` is NOT optional

> **The single thing to get right.** By default `graphify-rs build` does **not**
> write into your project. It writes to a global per-project cache with a hash
> suffix:
>
> ```
> ~/.graphify-rs/<project-name>-<hash>/graph.json
> C:\Users\<you>\.graphify-rs\myproject-f0eb0c119d0c4fff\graph.json
> ```
>
> The hash is not predictable, so you cannot write a portable MCP config against
> it — and an MCP server pointed at `.graphify-output/graph.json` will simply
> never find a file. Always pass `--output`.

From your workspace root:

```bash
graphify-rs build --path . --code-only --format json --output .graphify-output
```

| Flag | Why |
|---|---|
| `--output .graphify-output` | **required for a predictable path.** Without it the graph goes to the hashed global cache and your MCP config cannot reach it |
| `--code-only` | skip prose/doc extraction; faster, and enough for architecture questions |
| `--format json` | the MCP server reads `graph.json`. The default writes *every* format (html, graphml, cypher, svg, wiki, obsidian, report) — slower, and on a large repo that is tens of MB you do not need |
| `--no-llm` | keeps the build fully local |
| `--update` | re-extract only files changed since the last build |

Verify before wiring anything:

```bash
ls .graphify-output/graph.json
```

If that file is not there, the MCP server will start and answer nothing. Add
`.graphify-output/` to your `.gitignore` — it is regenerated output.

## Wire it as an MCP server

Point `--graph` at the **same path you passed to `--output`**. Add to **both**
`.mcp.json` and `.vscode/mcp.json` — the same drift trap as the other two
servers applies.

`.mcp.json` (Claude Code):
```json
"graphify": {
  "command": "graphify-rs",
  "args": ["serve", "--graph", ".graphify-output/graph.json"]
}
```

`.vscode/mcp.json` (VS Code):
```json
"graphify": {
  "type": "stdio",
  "command": "graphify-rs",
  "args": ["serve", "--graph", ".graphify-output/graph.json"],
  "description": "Whole-repo structural graph. Call graph_stats for live counts."
}
```

`graphify-rs` resolves from `PATH` once `~/.cargo/bin` is on it, so no relative
path is needed. Restart the editor afterwards.

## Tools worth knowing

- `smart_summary(level)` — token-efficient architecture overview; start here
- `god_nodes` — most-connected nodes, i.e. the hubs a refactor will disturb
- `detect_cycles` — circular dependencies
- `get_community` / `community_bridges` — module clusters and what links them
- `shortest_path` / `find_all_paths` — how two things are connected
- `pagerank` — structural importance
- `graph_diff` — compare two snapshots

## Pitfalls

**Stale graphs are silent.** Unlike quartz-ctx, which re-reads source within ~5s,
graphify serves whatever was in `graph.json` when the server started. A graph
built before a refactor answers confidently and wrongly. Rebuild after
significant changes — and pass `--output` again, or the rebuild lands in the
global cache while the server keeps serving the old in-project file:

```bash
graphify-rs build --path . --code-only --update --format json --output .graphify-output
```

**`graph_stats.community_count` reports 0** even when the graph is fully
partitioned — a known defect in that one aggregate. The community data itself is
correct. Use `smart_summary` (its header prints the true count),
`get_community`, or `community_bridges`. Never gate logic on
`graph_stats.community_count`.

**Never write counts into the MCP `description` string.** They are correct for
one day. On the reference workspace a description advertised "10,529 nodes,
16,141 edges" while the live graph held 7,186 / 10,025. Call `graph_stats`.

**Do not use it for API facts.** Node labels are structural, not authoritative
signatures. If you want to know what a method takes, ask quartz-ctx.

## How cortex uses it — `.graphify-output/` is a hard contract

cortex does **not require** graphify: `query_graph` and `simulate_change` run off
its own internal type-reference graph and work with graphify absent.

But if you do install it, **the path is not configurable.** cortex looks for the
graph at exactly:

```
<repo-root>/.graphify-output/graph.json
<repo-root>/.graphify-output/snapshots/
```

That path is hardcoded (`closeout.rs`, `consolidator2.rs`, `main.rs`). Build the
graph anywhere else and cortex silently finds nothing — no error, drift analysis
just reports zero forever.

What cortex does with it:

1. **At session closeout** it snapshots `graph.json` into
   `.graphify-output/snapshots/graph_<timestamp>.json`.
2. **If the graph is stale** it rebuilds first, invoking graphify itself with
   `--output .graphify-output` — so cortex's own rebuild is correct even if your
   habit is not.
3. **If the rebuild fails** it *skips* the snapshot and says why, rather than
   emitting a drift measurement against a stale file.
4. **The consolidation pipeline** compares consecutive snapshots to detect
   architectural drift.

Two consequences worth knowing:

- **Never rebuild without `--output`.** The build reports exit 0, writes to
  `~/.graphify-rs/<project>-<hash>/`, and leaves `.graphify-output/graph.json`
  exactly as stale as before — so cortex's staleness check fires on every
  closeout and never clears. (Observed: a rebuild without the flag left a 15-day
  old file in place and reported success.)
- **A graph that is never rebuilt produces no drift signal.** Consecutive
  snapshots are identical, and the pipeline reports all-zero drift — which reads
  as "nothing changed" but means "no data". If you see drift permanently at zero,
  rebuild the graph before concluding anything.
