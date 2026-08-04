# Agent Operating Manual

Replace `<PROJECT>` and the example crate names with your own. Keep this file
short — a long manual gets skimmed.

## 0) How to work

- **Act, don't overplan.** When you have enough to act, act. Give a
  recommendation, not a survey.
- **Lead with the outcome.** First sentence is the bottom line.
- **Ground every claim.** Only report something as done if you can point to
  evidence. If it failed, say so. If you skipped a step, say that.
- **Assess, don't act uninvited.** If the user is describing a problem, the
  deliverable is your assessment. Report and stop.
- **Match effort to the task.** Deep reasoning for hard work, fast for routine.

## 1) Tool routing — structure vs judgment

**quartz-ctx owns STRUCTURE — what the code *is*.** Parsed live from source, so
it is never stale.
- `get_api_context(hint)` — **start here for any coding task**; one budgeted
  packet of the relevant types, variants and signatures
- `get_item(name)` — full definition, including methods from every `impl` file
- `get_variants(enum)` — exact variants with field types
- `search_items` / `list_items` / `find_related_types`
- `get_trait_implementations` / `get_builder_methods` / `get_return_type_usage`
- Every item carries `file:line`, so cite it rather than making the reader search.

**cortex owns JUDGMENT — what we *learned*.** DB-backed, grown from sessions.
- `get_anti_patterns(hint)` — known traps
- `list_patterns(hint)` — vetted approaches
- `get_preferences(hint)` — style and API notes
- `recall(topic)` / `semantic_search(query)` — have we solved this before?
- `get_context(hint)` — one compact packet of the above
- `query_graph` / `simulate_change` — relationships and blast radius

**Decision rule:**
- What the code *is* → **quartz-ctx**
- What we *learned* → **cortex**
- Both in one task → run both

## 2) Always pass a `hint`

`get_anti_patterns`, `list_patterns`, `get_preferences` and `get_api_context` all
list every entry regardless — the hint decides what gets **expanded**. Two
reasons it matters:

1. **Tokens.** Measured: ~34k → ~10k per session boot, with nothing dropped.
2. **Learning.** Only hint-matched retrievals count as targeted, and only
   targeted retrievals feed pattern survival scoring. A hintless call records no
   usage signal at all.

## 3) Pre-code check (no trigger word needed)

Before writing any non-trivial function — anything that constructs, ticks,
spawns, or touches shared state:

1. `get_api_context(hint="<what you are about to write>")`
2. `get_anti_patterns(hint="<same>")`
3. `list_patterns(hint="<same>")`

Skip only for renames, typos and comments.

**Why:** these hold project-specific failure modes that are not in training data.
Three calls beat one debug cycle.

## 4) Mid-task checks — the ones that get skipped

Consult memory at every "I'm not sure" moment, not only at session start.

| Situation | Call |
|---|---|
| First approach failed | `recall <error keyword>` **before** trying a second |
| Two attempts failed | STOP. `recall` / `semantic_search` before a third |
| Unfamiliar compiler error | `semantic_search <description>` before reading source |
| Compiles but behaves wrong | `recall <behaviour>` — may be a known runtime trap |
| Choosing between approaches | `list_patterns` + `get_anti_patterns` |

The most valuable lookup is the one you skip because you already feel confident.

## 5) Capturing what you learn

Embed markers in your responses as you discover things:

```
[CORTEX-AP: description="..." tags="..."]wrong: ...\ncorrect: ...[/CORTEX-AP]
[CORTEX-PATTERN: name="..." intent="..." trust="verified"]body[/CORTEX-PATTERN]
[CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
[CORTEX-ADR: title="..." tags="..."]Context: ... Decision: ...[/CORTEX-ADR]
[CORTEX-PREFS-NOTE: tags="..."]note[/CORTEX-PREFS-NOTE]
```

When a task is **verifiably** complete (build passes, tests pass), end with:

```
✓ TASK COMPLETE: [one line]
Verified: [compile/test output]
Knowledge captured: [list]

Reply KNOWLEDGE COMMITTED to commit, anything else to skip.
```

On `KNOWLEDGE COMMITTED`, call `closeout_session(outcome_type="build_pass",
inline_approve=true, markers_text=<your markers>)`.

**`markers_text` is required on Claude Code.** There is no chat store to scrape
outside VS Code, so omitting it commits nothing while reporting success.

Tag entries so they are findable by concept, not just exact API name: include the
API name, the behaviour, the domain, and the colloquial term people actually use.
Start an anti-pattern description with **what goes wrong**, not what the feature
is.

## 6) Editing safety

- Smallest safe patch; match surrounding style.
- No unrelated refactors, no scope expansion.
- Never `git add` / `commit` / `push` unless explicitly asked in this
  conversation.
- If unexpected external edits appear in a file you touched, pause and confirm.

## 7) Verify before claiming

- Run the build or focused tests; report pass/fail with the actual output.
- `cargo test` builds a *separate* binary — passing tests do not mean the CLI or
  MCP server changed. Verifying behaviour through an executable needs an explicit
  `cargo build`.
- A failed build leaves the previous binary in place. Ask the artifact its
  version rather than trusting an exit code.

## 8) Housekeeping

After changing any tool surface, **grep this file for removed tool names**. A
manual that routes to a tool which no longer exists misdirects every future
session, and nothing errors.
