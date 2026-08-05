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

Closeout folds the session into the consolidation pipeline automatically when the
last run is over 8h old. Nothing reaches your instructions without a person:
anything awaiting review is listed under **AWAITING YOUR REVIEW** in the closeout
report and in `get_session_health`, with the command that resolves it
(`cortex skill-approve <name>`, `cortex review-proposals`).

## Verify before claiming done

Run the build or focused tests and report the actual result. A failed build
leaves the previous binary in place, so ask the artifact its version rather than
trusting an exit code.
