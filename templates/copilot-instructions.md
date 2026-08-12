# Copilot Instructions — <PROJECT>

Two MCP servers back this workspace. Use them before writing code.

- **quartz-ctx** — structure: what the code *is*. Parsed live from source, never
  stale.
- **cortex** — judgment: what we *learned*. Patterns, anti-patterns, decisions.

## Before writing any non-trivial code

1. `get_api_context(hint: "<what you are about to write>")` — types, variants and
   signatures for the task, in one budgeted packet.
2. `get_anti_patterns(hint: "<same>")` — known traps for this kind of change.
3. `list_patterns(hint: "<same>")` — approaches already vetted here.

Skip only for renames, typos and comments.

## The hint is required

`get_anti_patterns`, `list_patterns` and `get_preferences` refuse a call without
a `hint`. It decides what gets expanded (a hinted call is ~42% the size of a full
dump) and it is the only thing that records which knowledge proved useful.

Reviewing everything is fine, just say so:
`list_patterns(hint: "auditing all patterns", detail: "full")`.

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

| Situation | Call |
|---|---|
| First approach failed | `recall <error keyword>` before trying a second |
| Two attempts failed | Stop. `recall` or `semantic_search` before a third |
| Unfamiliar compiler error | `semantic_search <description>` first |
| Compiles but behaves wrong | `recall <behaviour>` — may be a known runtime trap |

## API facts

- `get_item(name)` returns the full definition including methods from **every**
  `impl` file, plus `file:line`. Cite the location instead of making the reader
  search.
- If a bare name is ambiguous across crates, alternatives are listed — pass
  `scope=` or the full unit id to pin one.
- `get_variants(enum)` before using any enum. Prefer an existing variant over
  inventing a parallel representation.

## Style

- Follow `get_preferences(hint: ...)` for naming, error handling and line length.
- Smallest safe patch. No unrelated refactors.
- Never run git commands unless explicitly asked.

## Recording what you learn

Embed markers as you go:

```
[CORTEX-AP: description="..." tags="..."]wrong: ...\ncorrect: ...[/CORTEX-AP]
[CORTEX-PATTERN: name="..." intent="..." trust="verified"]body[/CORTEX-PATTERN]
[CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
```

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

Markers alone do not save anything. When the work is verifiably done, present a
short summary of what you captured and ask for the word:

```
KNOWLEDGE COMMITTED
```

On that reply, call `closeout_session(outcome_type="build_pass",
inline_approve=true, markers_text=<your [CORTEX-*] markers from this session>)`.

**Pass `markers_text`.** Outside VS Code there is no chat store to scrape, so
omitting it commits nothing — the session ends and every marker you wrote is
lost. If the work did not verify, call `closeout_session(outcome_type="build_fail")`
and do not pass `inline_approve`.

### `[stale index]` in a response

Answers drawn from indexed code carry a one-line notice when the source has
changed since it was last indexed, naming the roots. It appears once per change,
not once per call.

It means results about those roots may predate your edits. Either re-run
`cortex.sh reindex` (`cortex.ps1 reindex` on Windows) or treat answers about
those roots as possibly behind. It is silent when the index is current, so when
it does appear it is worth believing.

## Launcher commands

`.cortex/cortex.sh` on macOS and Linux, `.cortex/cortex.ps1` on Windows. Same
commands on both.

| | |
|---|---|
| `reindex` | regenerate api-graphs and re-index every source in the manifest |
| `deploy` | rebuild cortex without stopping the MCP server |
| `check-mcp` | validate both MCP configs: relative paths, no drift between hosts |
| `status` / `doctor` | store summary / pipeline health |
| `skill-status` | drafts awaiting a human |
| `-- <args>` | pass anything straight through to the binary |

`deploy` exists because Windows blocks deleting a running executable. It renames
the live binary out of the way, which Windows does permit, so a rebuild never
requires hunting and killing the server first.

## Verify before claiming done

Run the build or focused tests and report the actual result. A failed build
leaves the previous binary in place, so ask the artifact its version rather than
trusting an exit code.
