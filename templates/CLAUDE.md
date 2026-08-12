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

## 2) The `hint` is required — say what you are doing

`get_anti_patterns`, `list_patterns` and `get_preferences` will refuse a call
without a `hint`. That is deliberate and it is not about tidiness:

1. **Tokens.** The hint decides what gets expanded. A hinted call is roughly 42%
   the size of `detail="full"` while containing the entries that actually apply.
2. **Learning.** Only a hinted retrieval counts as *targeted*, and only targeted
   retrievals feed pattern survival scoring. A hintless call taught the system
   nothing about which knowledge was worth keeping.

It is enforced rather than requested because requesting measurably failed:
across 823 real calls, every tool with a **required** parameter ran at 100%
compliance, and every tool where the hint was **optional** ran at 2–5% — while
this very file asked for one, twice.

Reviewing everything is still fine; just say so:
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

### What happens after closeout

Closeout runs the consolidation pipeline itself when the last run is more than
8h old (~4s), so clustering, skill detection, gap and survival proposals, trial
promotion and drift analysis do not depend on anyone remembering a command.

Nothing is committed to your instructions or preferences without a person.
Automatic promotion moves a proposal from `trial` to `pending` — into the review
queue, not past it.

Anything awaiting a human appears under **AWAITING YOUR REVIEW** in the closeout
report and in `get_session_health`, with the command that resolves it:

```
cortex skill-approve <name>     # publish a drafted skill
cortex skill-reject <name>      # discard it
cortex review-proposals         # pending proposals, then proposal-approve / proposal-reject <id>
```

The block is silent when the queue is empty. Read a drafted skill before
approving it: a draft containing placeholder text (`[describe when...]`) is a
detector firing on a thin signal, and publishing it teaches future sessions
nothing. Approval publishes to `.claude/skills/<name>/SKILL.md` (loaded
automatically here) and to `.github/prompts/<name>.prompt.md` (invoked as
`/<name>` in Copilot Chat), so one approval covers both editors with no further
wiring.

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
