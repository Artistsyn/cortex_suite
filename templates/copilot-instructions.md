# Copilot Instructions — <PROJECT>

The section between the `cortex_suite` markers is maintained by
`.cortex/cortex.sh instructions` (`.\.cortex\cortex.ps1 instructions` on
Windows): an update replaces that section and leaves the rest of this file
alone, so keep your own rules outside it.

<!-- cortex_suite:begin -->
## Code navigation and project memory

Two MCP servers back this workspace. Use them before writing code.

- **quartz-ctx** — structure: what the code *is*. Parsed live from source, never
  stale.
- **cortex** — judgment: what we *learned*. Patterns, anti-patterns, decisions.

Reading code under the indexed roots: `get_source(name)` instead of grep then
sed/cat (whole definition with line numbers, `Type::method`, `a|b`),
`find_references(name)` instead of `grep -n` (uses grouped by enclosing
function; `include_comments=true` for renames), `get_outline(path)` instead of
reading a whole file, `search_code(pattern)` instead of grep/rg for code, logs
and config (a `grep -E` regex; matches grouped by enclosing item as `12:text`,
`~` a comment line), and `get_source(file, lines="120-160")` instead of
`sed -n`/head/tail. grep stays right for filtering a command's output.

## Before writing any non-trivial code

1. `get_api_context(hint: "<what you are about to write>")` — types, variants and
   signatures for the task, in one budgeted packet.
2. `get_anti_patterns(hint: "<same>")` — known traps for this kind of change.
3. `list_patterns(hint: "<same>")` — approaches already vetted here.

Skip only for renames, typos and comments. Follow `get_preferences(hint: ...)`
for naming, error handling and line length.

## The hint is required

`get_anti_patterns`, `list_patterns` and `get_preferences` refuse a call without
a `hint`. It decides what gets expanded (a hinted call is ~42% the size of a full
dump) and it is the only thing that records which knowledge proved useful.

Aim it at names, not intent. Matching is on whole words, an entry needs **two**
distinct hint tokens before it expands (one for a hint under three words), and at
most **12** expand per call — closest first, with the rest counted in the reply.
A vague hint now returns less, not more.

Reviewing everything is fine, just say so:
`list_patterns(hint: "auditing all patterns", detail: "full")`.

**When an entry you add CORRECTS an older one, retire the old one** with
`cortex anti-pattern supersede <old-id> --by <new-id>` (same for `pattern`).
Otherwise both are served and the store contradicts itself.

### Ask for a delta on repeat calls

Every `get_anti_patterns` response ends with an `as of <timestamp>` line. Pass it
back as `since` on your next call in the same session and unchanged entries are
counted rather than re-listed.

Measured on a 171-entry store: 25,493 bytes -> 3,113, an 87.8% saving, roughly
5,600 tokens per repeat call. Anything your hint matches is still sent in full,
so the saving comes out of repetition, not out of the answer.

The hint controls how much of each entry you see; `since` controls how many times
you see the same thing. Use both.

## When you get stuck

A failed build or test that matches a recorded trap is pushed to you by the failure hook; the rows below cover what it cannot see.

| Situation | Call |
|---|---|
| Two attempts failed | Stop. `recall` or `semantic_search` before a third |
| Unfamiliar compiler error | `semantic_search <description>` first |
| Compiles but behaves wrong | `recall <behaviour>` — may be a known runtime trap |

## When a `[cortex]` warning arrives

With cortex's VS Code hooks installed (`cortex hooks-init --vscode`), a recorded
trap is attached to a tool's result at two moments. An edit that shares
distinctive evidence with a trap carries it in its own result, inside
`<PreToolUse-context>` tags. A build or test whose output shows a failure the
store already knows gets it inside `<PostToolUse-context>` tags, but VS Code
attaches that one after you have read the result, so it appears one step later
on that earlier command's result: look back for it. Read it before your next
step. If it applies, follow its fix. If it does not, ignore it: the matching
is lexical, about nine in ten relevant, not certain.

A nudge that a failure has recurred across sessions with nothing recorded is a
request. Once you know the cause, record it with the printed
`anti-pattern add ... --resolves '<signature>'` command.

## Limits

A limit is a claim with a provenance. Before accepting one — including one you
retrieved — say whose limit it is and what cheap check would move it. Check
`get_walls(hint: "<the limit>")` first, and record what you accept, dispute or
test with `record_wall` / `update_wall`: a verdict changes only with a new fact
(a measurement or a dated source).

## API facts

- `get_item(name)` returns the full definition including methods from **every**
  `impl` file, plus `file:line`. Cite the location instead of making the reader
  search.
- If a bare name is ambiguous across crates, alternatives are listed — pass
  `scope=` or the full unit id to pin one.
- `get_variants(enum)` before using any enum. Prefer an existing variant over
  inventing a parallel representation.

## Recording what you learn

Embed markers as you go:

```
[CORTEX-AP: description="..." tags="..."]wrong: ...
correct: ...[/CORTEX-AP]
[CORTEX-PATTERN: name="..." intent="..." trust="verified"]body[/CORTEX-PATTERN]
[CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
[CORTEX-WALL: claim="..." provenance="hardware" cheapest_test="..."]measured: what @ where @ 2026-09-30[/CORTEX-WALL]
```

`correct:` must start its own line; the split is per line. A WALL needs a
`provenance` and at least one evidence line `kind: text @ source @ date`
(sources dated), or closeout refuses it and says why.

A pattern takes an optional `kind="constraint|policy|fact"` (default
`procedure`). Constraints and policies are served in their own sections ahead
of ordinary patterns in `get_context`.

Write the description as **what goes wrong**, not what the feature is, and tag
with the API name, the behaviour, the domain and the colloquial term — entries
are found by concept, not exact spelling.

Approved skills are published to `.github/prompts/<name>.prompt.md` — invoke one
as `/<name>` in Copilot Chat. (The same skill is written to `.claude/skills/` for
Claude Code; one approval covers both.)

Closeout folds the session into the consolidation pipeline automatically when the
last run is over 8h old. Nothing reaches your instructions without a person:
anything awaiting review is listed under **AWAITING YOUR REVIEW** in the closeout
report and in `get_session_health`, with the command that resolves it
(`cortex skill-approve <name>`, `cortex review-proposals`).

### Closing the session

When the work is verifiably done, call `closeout_session(outcome_type="build_pass")`.
It commits your `[CORTEX-*]` markers through the gates without waiting for approval:
in VS Code it reads them from this chat; in any other host, pass them as
`markers_text`. A person audits a random sample (`cortex knowledge audit`) instead
of approving every entry. If the work did not verify, call
`closeout_session(outcome_type="build_fail")`.

**Close the session.** In VS Code, markers reach the store only through
closeout; a session that never closes loses them. (Claude Code also captures
them from its transcript as they are written.)

If closeout answers "staged mode", the audit has switched automatic commit off.
Present a short summary and ask for the word `KNOWLEDGE COMMITTED`; on that reply
call `closeout_session(outcome_type="build_pass", inline_approve=true)`.

### Freshness - what "current" means here

Answers are checked against the disk at the moment they are given. Nothing
depends on a timer, a `reindex`, a server restart or a git commit.

- **quartz-ctx** re-stats its roots before every tool call and re-parses only
  files whose size or nanosecond mtime moved (a file written within ~3 s of
  being read is content-hashed instead of trusted). An edit is visible to the
  very next call. A file that fails to parse is named on not-found answers,
  since its items are missing: `[parse error] ... their items are not served`.
- **cortex** climbs a check ladder before every code-backed answer, and before
  `get_delta`, `get_anti_patterns` and `list_patterns`: metadata -> content
  hash -> re-parse -> API facets, each rung only for what the one below could
  not clear. ~5 ms when nothing changed, ~0.3 s for an edit in a 1,000-unit
  crate. Renamed and deleted items leave the index.
- **graphify** is served through `cortex graphify-serve`, which rebuilds
  `graph.json` (~1.5 s, JSON only) and reloads graphify when the source has
  moved past it. `graphify-rs serve` alone loads the file once and never again.

`[stale index]` appears only when a refresh FAILED, naming the root and the
error. Believe it; it is not a routine banner any more.

`get_delta` answers "what changed in the API" from the index's own change
journal - no git needed, uncommitted edits included, net per item. Default
window is this session; `since` takes a time, `90m`/`2h`/`1d`, or a git ref.
Its last line says how deep the check went, which is what makes "no changes"
mean something.

An expanded pattern or anti-pattern that names code changed after it was
written carries `⚠ code it names changed since it was written: ...`. Re-verify
before relying on it, then update or supersede the entry. `knowledge-drift`
lists every such entry; closeout counts them under AWAITING YOUR REVIEW.

`reindex` still exists for a full rebuild (graph passes across every root), but
it is never needed for correct answers.

## Launcher commands

`.cortex/cortex.sh` on macOS and Linux, `.cortex/cortex.ps1` on Windows. Same
commands on both.

| | |
|---|---|
| `reindex` | full rebuild of every manifest source (never needed for correctness - see Freshness) |
| `refresh` | re-index only roots whose source changed; what the servers do before answering |
| `knowledge-drift` | patterns/anti-patterns naming code that changed after they were written |
| `deploy` | rebuild cortex without stopping the MCP server |
| `check-mcp` | validate both MCP configs: relative paths, no drift between hosts |
| `status` / `doctor` | store summary / pipeline health |
| `skill-status` | drafts awaiting a human |
| `instructions` | add or update the cortex_suite section of CLAUDE.md and `.github/copilot-instructions.md`, leaving the rest of each file alone |
| `-- <args>` | pass anything straight through to the binary |

`deploy` exists because Windows blocks deleting a running executable. It renames
the live binary out of the way, which Windows does permit, so a rebuild never
requires hunting and killing the server first.
<!-- cortex_suite:end -->

## Style

- Smallest safe patch. No unrelated refactors.
- Never run git commands unless explicitly asked.
- Fewer, fuller requests: every request re-sends the whole conversation. Run
  independent reads, searches and edits together, and wait on long jobs in the
  background instead of polling with `sleep` or repeated checks.

## Verify before claiming done

Run the build or focused tests and report the actual result. A failed build
leaves the previous binary in place, so ask the artifact its version rather than
trusting an exit code.
