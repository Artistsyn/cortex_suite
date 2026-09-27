# cortex_suite — review, research and upgrade plan (2026-09-27)

**Bottom line.** Capture works and pull retrieval got genuinely better in
September. The two push mechanisms, compaction and the edit guard, had
**never delivered anything to an agent**. The scoreboard's green arrows were
mostly measurement artifacts, and the claimed token savings were
counterfactual. This change makes delivery real and the scoreboard honest, and
it closes the failure-observation gap. The rest of this document is the
evidence behind that, what the field does, and the ordered plan for the rest.

This file is the tracker for cortex upgrade work. When an item ships, mark it
here with the date and the evidence.

---

## 1. What the audit found

All numbers were measured on this workspace (FlowMake) on 2026-09-27, from
`.cortex/memory.db` and the Claude Code transcripts under
`~/.claude/projects/-Users-user-FlowMake/`.

### 1.1 Delivery: two of three hook mechanisms were silent

| Mechanism | What happened | Evidence |
|---|---|---|
| `compact_output` (Bash, PostToolUse) | Computed a compacted copy of every output. Claude Code writes plain PostToolUse stdout to its debug log, so **the model never saw it**. | This session logged `cat scoreboard.rs` as 13,463 → 1,794 chars; the full file arrived. 5,810 compacted copies sit in transcripts as `hook_success` attachments. None of the 4,548 tee files (21 MB) was ever opened by an agent, apart from the session that built the feature. |
| `edit_guard` (Edit/Write, PostToolUse) | Warnings were computed and logged as fires, then **dropped** for the same reason. | A probe write fired trap #344 (`edit_guard_fires` row at 05:07:45); no warning reached the agent. 115 lifetime fires, zero delivered. |
| `note_challenge` (UserPromptSubmit) | Delivered: plain stdout on this event *is* shown to the model. | Per the [hooks docs](https://code.claude.com/docs/en/hooks). |

The docs are unambiguous. Only `hookSpecificOutput.additionalContext` reaches
the model from PostToolUse or PostToolUseFailure. `updatedMCPToolOutput` can
replace output for **MCP tools only**, so **no hook can shrink a Bash result**.
An `mcp_tool` hook's text is parsed exactly like a command hook's stdout. This
host already delivers `additionalContext`: the transcripts contain 45
`hook_additional_context` attachments from the desktop app's own hooks. It
already *tries to parse* cortex's hook text as JSON: two "Hook output looks
like a JSON object but is not valid JSON" errors on the Bash hook.

**Failed commands were invisible.** A Bash call that exits non-zero never
reaches PostToolUse. Claude Code routes it to **PostToolUseFailure**, with the
output in `${error}`. Only failures masked by a pipe (`cargo build 2>&1 |
tail`) were ever observed, which biased every observed pass rate upward and
hid failures from recurring-failure detection. This was found because a
command that exited 1 left no row while its neighbours did. It was verified
after the fix: the new PostToolUseFailure hook fired live in this session, and
its logged argument size (209 bytes) matches `error` = "Exit code 1\n" plus
the command's output, byte for byte.

### 1.2 The scoreboard measured the wrong things

| KPI (v1) | Claimed | What it actually was |
|---|---|---|
| Build pass rate | 100% | Closeouts only, which run only after success. No build failure had been recorded that way in 30 days, while hook-observed runs passed 65% (590/903). |
| Pattern reuse / session | 38 (6.5× ↑) | Rows, not uses. 144 of 228 came from **36 identical `get_context` calls in 9 minutes** (one loop, 4 distinct patterns). The rest was mostly one `recall` that listed 68 patterns. Real targeted calls in 14 days: 4 `recall`, 36 `get_context`. |
| Gaps / session | 0.17 (↑ improving) | Distinct queries, not misses. The only active gap was `get_item SceneRenderer`, **missed 36 times** in that same loop. `recall` had not logged a gap since Aug 2. |
| Compaction | ~686k tokens saved / 14 d | Counterfactual (§1.1). About 95% of the "saving" was windowing, i.e. hiding the middle of source files and query results, not removing redundancy. |
| Denominators | closed sessions | Closeout covered about 13 of 44 working sessions (28 d), while the numerators counted all sessions. |
| Store totals | 271 / 411 | Included retired rows (7 patterns, 4 anti-patterns). |

### 1.3 The token bill: where the cost really is

Measured from transcript `usage` fields over 14 days: **5,918 API calls**,
**2.95 billion cache-read tokens (98.6% of input)**, 41 M cache writes,
6.3 M output. The **average context per call is about 506k tokens**: sessions
start at a **~70k fixed prompt** (up from ~40k in mid-August) and grow to about
1M before auto-compaction, which drops them back to 55–85k.

Cost is context size × number of calls, because everything in context is
re-read on every later call. Attributing those re-reads to what put the
content there (14 days):

| Source | Injected | Re-read | Share of cache reads |
|---|---|---|---|
| Bash output | 1.64 M tok | 406 M | 13.7% |
| Assistant tool-call inputs (Edit/Write bodies) | — | 341 M | 11.5% |
| Assistant text + thinking | — | 196 M | 6.7% |
| **cortex retrieval** | 215 k tok | 60 M | **2.0%** |
| Fixed prompt (system, tools, CLAUDE.md, MEMORY.md) | ~70 k / call | ~385 M | ~13% |

A first `get_anti_patterns` call costs 36–48 KB (the one-line index of every
entry is about 80% of it), and a `since` repeat costs 8–12 KB. A full boot
(`get_anti_patterns` + `list_patterns` + `get_preferences`) is about 100 KB,
roughly 25k tokens. claude-mem primes a session with under 500 tokens.

### 1.4 Memory and learning

* **Capture works.** 74 markers in 14 days; 264 live patterns and 407 live
  anti-patterns; 54% of patterns carry a usage signal (6/113 in July).
* **There is no harvest gate.** 262 of 264 anti-pattern markers were promoted
  as proposed.
* **Pull delivery improved.** Since the Sep 17 caps, 0 of 19 `get_anti_patterns`
  results overflowed to a 2 KB file preview; before that it was 15 of 63.
* **Targeted use is rare.** In 14 days of Claude Code sessions: 4 `recall`,
  3 quartz-ctx calls, 1 graphify call, against 313 failed build/test runs.
* **Outcomes are roughly flat.** Observed build/test runs were 62% green over
  30 days vs 61% over the prior 30. A 14-day slice (66% vs 58%) looks better,
  but it is noisy and workload-dependent.
* **Repeats.** 14 of 195 failures in the last 14 days (7%) had first occurred
  before the window; 9 of those had nothing recorded. The `ANDROID_NDK_ROOT`
  failure hit 3 sessions over 12 days before it was recorded (Sep 20); it has
  not recurred since.
* **Precision of the old edit guard.** Replaying its logic over all 2,403 real
  edits of 120+ characters, it would have fired on **91%** of them. Of the 115
  fires that happened, about 52% were relevant on inspection.

---

## 2. What shipped in this change

| Change | Files | Verified by |
|---|---|---|
| Hook tools answer in `hookSpecificOutput.additionalContext` JSON, the one form the host shows the model. `format: "text"` is available for direct calls. | `push.rs` (new), `mcp/tools.rs`, `mcp/mod.rs` | Unit tests; JSON-RPC drive of the new binary with the exact hook input shapes (valid JSON, correct `hookEventName`, silence on repeats, prose files and passing runs). |
| `compact_output` is now an honest observer. No windowing, no tee files, no savings claimed; observed volume is still logged. | `mcp/tools.rs`, `audit.rs` | Tests; `fired` relabelled. |
| **Failure recall.** A failing build or test is answered with (1) the trap *linked* to its signature, (2) a trap its **message** names (identifier or ≥3 distinctive words; locations and paths excluded; rustc E-code churn excluded), or (3) for a specific failure seen in 3+ sessions with nothing recorded, a nudge with the exact `--resolves` command. At most once per failure and 4 per session. | `push.rs` | Unit tests; replay over 424 recorded failures, where 8 matched and about 7 were right (e.g. `ANDROID_NDK_ROOT` → #384, hook-frontier stall → #321). |
| **`PostToolUseFailure(Bash)` hook**, hook set v3. The installer upgrades existing installs on the next `cortex serve`, and `hooks-init` does it on demand. | `main.rs` | Installer test (right event per hook, merge-safe, idempotent, upgrades a v2 file). The live hook fired in this session. |
| **`--resolves` stores the link** (`recurring_errors.anti_pattern_id`) and **when** it was handled (`handled_at`). | `test_signal.rs`, `main.rs`, `crystallizer.rs`, `memory.rs` | Tests. |
| **Edit-guard precision.** Evidence must be a distinctive identifier plus one more distinctive token, or three distinctive words. Common English, generic std/JS identifiers and prose files are excluded. | `push.rs`, `mcp/tools.rs` | Replay over 2,403 real edits: fires dropped 91% → 32%. Hand-labelled random sample of 30: **~90% relevant** (old rule ~52%, intermediate 63%). |
| `push_log` table: every delivered push, with trap id and size. | `memory.rs` | Tests. |
| **Scoreboard v2** (details in §4). | `scoreboard.rs`, `main.rs` | 8 tests; runs on the live store in 4.5 s including the transcript scan. |
| `fired` watches "pushes delivered to agents" (currently NEVER, until servers restart on the new binary). | `audit.rs` | Live run. |
| Replay harnesses (`#[ignore]`) for the edit guard and failure recall, to evaluate any future matcher change against real history. | `mcp/tools.rs`, `push.rs` | Used for every number above. |

**To activate:** each Claude Code session's cortex MCP server must restart on
the new binary (new session, or reconnect cortex in /mcp). The hook config was
already upgraded to v3 on 2026-09-27; a backup is at
`.cortex/backups/settings.local.json.pre-hookset-v3-2026-09-27`.

**How to confirm delivery afterwards:** `cortex scoreboard` → "Host-confirmed
cortex context deliveries" counts `hook_additional_context` attachments that
contain `[cortex]`. That is the host's record, not cortex's claim. `cortex
fired` → "pushes delivered to agents" should turn live.

---

## 3. What the field does, and what we take from it

### Token saving

* **Output compression is a small, risky lever.** Two independent benchmarks
  of RTK found **no cost saving**:
  [Quesma](https://quesma.com/blog/does-rtk-make-ai-coding-cheaper/) (1,740
  runs: −5% to +17% cost, pass rate −1 to −2 pts) and
  [JetBrains](https://blog.jetbrains.com/ai/2026/07/rtk-claude-code-token-savings/)
  (425 trials: +7.6% cost at low effort, from extra turns). Cache reads
  dominate the bill (94–98% of input), tool output is a minority of it, and one
  extra turn caused by lossy output costs more than the compression saved.
  "A tool's self-reported savings are a claim about its counterfactual, not
  about your bill." **Adopted:** measure the bill (§4), never counterfactual
  bytes; no windowing.
* **Observation masking beats summarisation.** Hiding old tool outputs halves
  cost and matches LLM summarisation
  ([The Complexity Trap](https://arxiv.org/abs/2508.21433)). Claude Code
  already does this ("microcompact"), so cortex must not duplicate it. It does
  mean cortex's own injected results can silently disappear, which argues for
  small, re-fetchable, just-in-time memory over boot dumps.
* **The real lever is context length.** Anthropic's
  [context-engineering guidance](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
  (just-in-time retrieval by lightweight identifiers; structured notes outside
  the window; compaction with tool-result clearing) matches the data in §1.3:
  about 506k tokens of context re-read per call. Auto-compaction fires at about
  83% of the window; `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` can only lower it
  ([issue #31806](https://github.com/anthropics/claude-code/issues/31806)).
  Compacting earlier is safe only if working state survives compaction
  verbatim. Summaries lose specific categories: dates went from 3% preserved
  to 62% after a one-line prompt fix in
  [this study](https://arxiv.org/abs/2608.11775). For code the risky
  categories are identifiers, paths, error strings and measurements.

### Persistent memory

* **Code-grounded, verified-at-use memory.**
  [GitHub Copilot Memory](https://github.blog/ai-and-ml/github-copilot/building-an-agentic-memory-system-for-github-copilot/)
  stores `{subject, fact, citations, reason}` and re-checks the cited lines
  against the current branch before use. A/B result: **+7% PR merge rate
  (90% vs 83%)**, and adversarially seeded memories were caught.
  **Adopt:** citations plus just-in-time verification on top of
  `knowledge-drift`.
* **Forgetting is the production failure.** Across 13 memory configurations
  ([2606.15903](https://arxiv.org/abs/2606.15903)), failures are mostly stale
  facts not retired, rather than missed recall; an LLM hook at mutation time
  scored 91.7–93.2%. **Adopt:** a supersede/contradiction check when a marker
  is committed, not only the manual `supersede`.
* **Temporal validity.**
  [Zep/Graphiti](https://arxiv.org/abs/2501.13956) keeps bi-temporal validity
  on edges and invalidates rather than deletes. cortex's `superseded_by` is the
  start; add "valid while this code is unchanged".
* **Small always-on footprint.**
  [claude-mem](https://github.com/thedotmack/claude-mem): under 500 tokens of
  priming, then a 50–100-token/hit index, then full entries on demand.
  [Letta's plain filesystem](https://www.letta.com/blog/benchmarking-ai-agent-memory/)
  scored 74% on LoCoMo, beating specialised libraries.
  [Letta Context Repositories](https://www.letta.com/blog/context-repositories/)
  keep file names and descriptions always visible and bodies on demand, with
  sleep-time consolidation into 15–25 files. Claude Code's own
  [auto memory](https://code.claude.com/docs/en/memory) (MEMORY.md, capped at
  200 lines, plus "Auto Dream" consolidation) already covers generic memory.
  **cortex's unique value is what the harness cannot do:** code-grounded
  verification, outcome-linked learning, and event-triggered delivery.

### Self-learning and skills

* [ACE](https://arxiv.org/abs/2510.04618): evolving playbooks built from
  **incremental deltas** with helpful/harmful counters, avoiding brevity bias
  and context collapse; +10.6% on agent tasks, no labels needed.
  [ReasoningBank](https://arxiv.org/abs/2509.25140): distil strategies from
  **both successes and failures**, judged without labels. cortex's survival
  counters are the same idea. Missing: failure→fix distillation (T2.3).
* **Unverified self-authored skills do not help.**
  [SkillsBench](https://arxiv.org/abs/2602.12670) (7,308 trajectories):
  curated skills gave +16.2 pts on average but only **+4.5 for software
  engineering**, and **self-generated skills gave no benefit**. Focused
  2–3-module skills beat comprehensive docs.
  [Skill Issue](https://arxiv.org/abs/2609.12742): optimised repository SKILL
  files gave +4.9 pts, but at single-repo scale that is inseparable from
  run-to-run variance; repository-specific facts are what helped.
  [Socratic-SWE](https://arxiv.org/abs/2606.07412): skills distilled from
  failure/repair **traces**, validated by execution. **Adopt:** skills come
  only from verified failure→fix traces, reviewed by a human; measure them,
  don't assume them.

### Code understanding

* Cursor measured +12.5% accuracy from semantic search on top of grep
  ([blog](https://cursor.com/blog/semsearch)), and has since reportedly
  dropped indexing because agents search well enough on their own. Graph/AST
  MCP servers (CodeGraph, Serena, Aider-style repo maps) claim large token
  reductions; those are self-reported. Here, agents called quartz-ctx 3 times
  in 14 days. **Decision:** measure API-misuse errors per edit before
  investing further in structure pulls. rustc already names the item in most
  compile errors, and the replay showed text-matching those errors against
  traps was noise.

---

## 4. Scoreboard v2 — how to read it

`cortex scoreboard [--window-days N] [--no-tokens] [--format json]`

Every number is labelled as one of three kinds:

* **OBSERVED**: recorded by hooks, with no one choosing when.
* **DELIVERED**: reached an agent's context.
* **SELF-REPORTED**: closeouts; shown for reference, never used as a rate.

| Section | Read it as |
|---|---|
| Sessions worked / closed out | Denominator, and closeout coverage. |
| Outcomes | Build/test runs green, from every hook-observed run, and sessions whose last run was green. |
| Repeat failures | Of this window's distinct failures, how many **came back from before the window**. That is what memory should prevent. "Hit again after being recorded" means the store knew and it happened anyway. There is no trend arrow here: a failure counts where it was last seen, so the previous window is a floor. |
| Delivery | Targeted lookups (identical calls within 10 min counted once; repeats flagged as a loop), bulk listings, pushes delivered, edit-guard fires never delivered, lookup misses. |
| Store | Live entries only. |
| Token bill | From transcripts: calls, average context per call, cache reads/writes, fixed prompt size, tool results injected by source, re-reads until compaction, **host-confirmed cortex deliveries**, cortex hook errors. |

---

## 5. Plan: tranche 2, in order of measured impact

Each item says how it will be judged, and "shipped" means verified against
that measure.

1. **Safe earlier compaction (largest token lever).** Add a PreCompact hook
   that writes a structured working-state checkpoint: verbatim identifiers,
   paths, error strings, measurements, decisions, next step (`set_checkpoint`
   exists). Add a `SessionStart(source=compact)` hook that re-injects it
   (≤1.5k tokens). Then lower the auto-compact point (e.g. 400–500k) and
   measure. *Judge:* average context per call (target −40%); runs green and
   repeat reads must not get worse.
2. **Small boot, just-in-time pull.** A SessionStart brief of ≤1.5k tokens
   (project-scoped traps with recent fires or links, open review items, last
   checkpoint) replaces the ~25k-token three-call boot. Keep
   `get_anti_patterns` as search with a relevance floor and a clustered index
   ("N more in <domain>") instead of listing all ~410 lines. The earlier
   "narrowing hides traps" decision (#371) was measured against edit-guard
   fires, which §1.4 shows were about half noise, so re-measure against linked
   failures and labelled relevance first. *Judge:* cortex's share of re-reads
   (2.0%), plus the replay harness on relevance.
3. **Failure→fix mining.** From `test_outcomes` plus the Edit/Write trace: a
   failure signature followed by a green run of the same command yields the
   edits in between as a verified remedy. Draft an anti-pattern with that
   `correct`; a human reviews it. *Judge:* "came back with nothing recorded"
   trends down; time from first sighting to record (12 days for
   `ANDROID_NDK_ROOT`).
4. **Citations + just-in-time verification** (Copilot pattern) on top of
   `knowledge-drift`: pushes and pulls carry ✓ verified / ⚠ code changed;
   unverified, unused entries decay. *Judge:* share of served entries with
   valid citations.
5. **Mutation-time supersede check** at marker commit (duplicate or
   contradiction → proposal in the review queue). *Judge:* contradictions
   served together, which should be 0.
6. **Push feedback.** Did the agent act on a delivered trap? Check whether its
   next turn references the trap, or the edited region changes accordingly.
   Demote traps that are repeatedly ignored. *Judge:* per-trap precision on
   the scoreboard.
7. **Learned generic identifiers.** Replace the static list with document
   frequency across observed edits (the trace hook has them).
8. **One owner per fact.** Claude Code auto memory holds user, feedback and
   project facts; cortex holds code-grounded traps and outcomes. Stop writing
   the same lesson to both. *Judge:* MEMORY.md size, and overlap.
9. **Fixed prompt audit** (~70k per call): CLAUDE.md × 2, MEMORY.md, skill
   list, MCP instructions, and servers not needed for this project (pixellab,
   blender). *Judge:* median first-call context.
10. **Loop guard.** The same tool and arguments more than 3 times in 10
    minutes gets a one-line "unchanged since your last call" answer.
    *Judge:* identical repeats → 0.
11. **Hook robustness.** 672 historical "not connected" errors were clustered
    on deploy days. Consider command-type hooks that call the CLI (always the
    current binary, no connection dependency; ~50–80 ms per event).
12. **VS Code Copilot push parity.** *(Built 2026-09-27; see the end of this item.)* Copilot keeps everything it had: all MCP
    tools via `.vscode/mcp.json`, the pull workflow in
    `copilot-instructions.md`, skills in `.github/prompts`, and its sessions in
    the scoreboard's store metrics. It does not get pushes or build/test
    observation, and never did. VS Code's
    [agent hooks (Preview)](https://code.visualstudio.com/docs/agent-customization/hooks):
    * read `.github/hooks/*.json`, and `.claude/settings*.json` only with
      `chat.useClaudeHooks`;
    * support **only `command` hooks**, not `mcp_tool`;
    * have **no PostToolUseFailure** event;
    * ignore matchers in Claude-format files;
    * share the `hookSpecificOutput.additionalContext` output contract.

    Plan:
    * a host-agnostic `cortex hook <event>` CLI entrypoint that reads the hook
      JSON on stdin (Claude's `Bash`/`Edit`/`Write` and VS Code's
      terminal/edit tool names), runs the same `push.rs` logic, and prints the
      JSON;
    * `hooks-init --vscode`, which writes `.github/hooks/cortex.json`.

    Before shipping, capture VS Code's exact tool names and input fields with an
    audit hook, including how a failed terminal command is reported without a
    failure event. **Audit hook installed 2026-09-27** in the FlowMake workspace
    (`.github/hooks/cortex-audit.*`: every documented event; logs to
    `.cortex/vscode-hook-audit.jsonl`; no stdout, exit 0, ~20 ms, 20 MB cap;
    tested with bash and pwsh).

    **Captured and analysed 2026-09-27.** One Copilot agent session (13 hook
    records), cross-checked against Copilot's own transcripts for this
    workspace (12 sessions, 4,643 tool calls):

    | Fact | Evidence |
    |---|---|
    | Common input fields | `session_id` (UUID), `hook_event_name`, `timestamp`, `cwd` (the workspace root), `transcript_path`, and `tool_use_id` on tool events |
    | Terminal tool | `run_in_terminal`, `tool_input` = `{command, explanation, goal, mode}` (the tool's own arguments, identical to the transcript's) |
    | Terminal result | `tool_response` is **one string**: the terminal's text (prompt, command echo, output). Sometimes it is prefixed "Note: The tool simplified the command to `…`". **No exit code** anywhere. |
    | Failed commands | Reported as ordinary `PostToolUse`. Copilot's own transcript marks them `success=True` too (it means "the tool ran"). Failure can only be read from the output text, which `test_signal::classify` already does. |
    | Edit tools (from transcripts) | `replace_string_in_file` {filePath, oldString, newString} ×858; `multi_replace_string_in_file` {replacements[]: {filePath, oldString, newString}} ×54; `apply_patch` {input: "*** Begin Patch…" with `+` lines} ×51; `create_file` {filePath, content} ×21; `edit_notebook_file` {filePath, newCode} ×3 |
    | Concurrency | Hooks run **concurrently** (one call's PostToolUse overlaps the next call's PreToolUse). A shared append-only log interleaved two records, so the audit now writes one file per call. The cortex hook must be concurrency-safe; the SQLite store with a busy timeout is. |
    | Copilot transcripts | JSONL records: `session.start`, `user.message`, `assistant.turn_*`, `assistant.message`, `tool.execution_start` (arguments) and `tool.execution_complete` (only `success`). **No tool output and no token usage**, so they cannot confirm delivery or give a bill. `COPILOT_OTEL_FILE_EXPORTER_PATH` suggests an OpenTelemetry file that might. |
    | Environment | `VSCODE_*` variables and `COPILOT_OTEL_FILE_EXPORTER_PATH`; no Claude-style project-dir variable. Use `cwd` from the payload. |

    Entrypoint design, confirmed by the above: `cortex hook <event>` reads the
    payload on stdin and normalises it.
    * Terminal: Claude `Bash` → `tool_input.command` plus `tool_response.stdout`
      and `.stderr` (or `error` on PostToolUseFailure); VS Code
      `run_in_terminal` → `tool_input.command` plus the `tool_response` string.
    * Edit text: Claude `Edit.new_string`, `Write.content`,
      `MultiEdit.edits[].new_string`; VS Code `replace_string_in_file.newString`,
      `create_file.content`, `multi_replace_string_in_file.replacements[].newString`,
      the `+` lines of `apply_patch.input`, and `edit_notebook_file.newCode`.

    It runs the same `push.rs` logic, keys dedupe on `<host>:<session_id>`,
    and prints the `hookSpecificOutput.additionalContext` JSON. There are no
    matchers: it filters by `tool_name` itself. A verification run afterwards
    needs Copilot to report whether it saw a `[cortex]` line, since its
    transcripts do not record hook output.

    **Built 2026-09-27:** `cortex hook <event>` (`hook_cli.rs`) and
    `hooks-init --vscode`, installed in FlowMake as `.github/hooks/cortex.json`.
    Verified:
    * 10 unit and end-to-end tests.
    * The real binary driven with the captured payload shapes. A failing
      Android build returns trap #384; an `ignore_zoom` edit returns #344; a
      disputing prompt returns the challenge note; reads, `PreToolUse`,
      passing runs, garbage and a missing store are all silent. Exit 0
      every time, stderr empty, no store ever created.
    * About 20 ms for calls that exit early, about 40 ms for calls that do work.
    * 90 concurrent calls: every reply valid, `quick_check` ok.
    * Its own heartbeat row: `fired` shows it as "not in use" until
      VS Code first runs it.

    **First live session, 2026-09-27: the reply arrived one request late.**
    Copilot (gpt-5.4-mini) created a probe file with `apply_patch` and said no
    `[cortex]` context arrived. Everything up to VS Code worked: its hooks log
    shows cortex's reply (trap #344), and `push_log` recorded it. The drop is in
    Copilot Chat 0.61's tool loop. The function that runs the PostToolUse hook
    and appends its `additionalContext` to the tool result (as
    `<PostToolUse-context>`) is called without `await`, and the result is
    rendered into the next request straight away. That request started at
    about 22:56:54.429 (4,444 ms, ending 58.873). The hook started at 54.436
    and replied at 54.470. PreToolUse is awaited, and its context is appended
    to the same result synchronously.

    **Fix:** `cortex hook PreToolUse` runs the edit guard, since the edit's text
    is all it reads. The warning is kept as an offer (`edit_guard_offers`) and
    becomes a delivered push at that call's PostToolUse, which VS Code runs
    only when the tool succeeded, so an edit that throws cannot use up the
    warning. Parallel edits in one round share one warning (a 1 s window,
    checked and claimed in one statement). Failure recall needs the output, so
    it stays on PostToolUse and is one request late in VS Code.
    `hooks-init --vscode` now registers PreToolUse. Without it (an old hook
    file, or Claude Code), PostToolUse judges the edit as before.

    Verified:
    * 5 new tests (warn before, count after; a failed edit's retry; parallel
      edits; the PostToolUse-only path; the hook file). 282 pass.
    * The captured `apply_patch` call replayed through the binary against a
      snapshot of the live store. PreToolUse returns #344 with no
      `permissionDecision`, and nothing is counted. PostToolUse is silent and
      records one push.

    **Verified live, 2026-09-27 (a new chat, mai-code-1.1-flash):** the
    `create_file` result carried the `<PreToolUse-context>` note, Copilot quoted
    trap #344 word for word, and `push_log` recorded one push with no offers
    left. VS Code's saved chat (`workspaceStorage/<hash>/chatSessions/<id>.jsonl`,
    `requests[n].result.metadata.toolCallResults`) confirms both halves. There,
    the first chat's `apply_patch` result holds the late `<PostToolUse-context>`
    note its model never saw, and the new chat's result holds the PreToolUse note
    its model quoted.

    **Remaining caveat:** a command hook's session id is the host's, not the MCP
    server's, so per-session dedupe works but survival crediting will not join
    until the two are mapped. The same entrypoint would also serve item 11.

### Tranche 3 (research-grade)

* **Holdout:** suppress 10–20% of eligible pushes at random and compare
  repeat-failure and fix times. This is the only causal measure of whether
  memory helps.
* **Replay benchmark from git history** (the Skill Issue methodology), with
  enough tasks to beat run-to-run variance.
* **Sleep-time consolidation** (Letta / Auto Dream style): nightly dedupe,
  clustering, citation checks, decay, failure→fix drafts.
* **Native quiet flags** (`cargo -q`) via PreToolUse `updatedInput` **without**
  a `permissionDecision`, so the rewritten command still passes the normal
  permission rules. Lossless, but only worth it if the bill says Bash output
  matters (13.7% of re-reads today). Never pair it with `allow`, which skips
  the permission prompt.

### Rejected, with reasons

* Windowing or eliding output: lossy, costs extra turns, and a hook cannot
  deliver it anyway.
* RTK-style blanket command rewriting: no measured saving in two independent
  benchmarks.
* Counting counterfactual savings as saved.
* Self-generated skills without trace grounding and verification.
