# SSNebula in cortex_suite — plan (2026-10-24)

Goal: agents write code that already passes ssnebula, instead of cleaning it
afterwards. Reviewed against `ssnebula/SSNEBULA_INTERACTIVE_HANDOFF.md`.

## What the survey found

- ssnebula is fast: the whole `cortex` crate (no clippy) scans in **0.5 s**. A
  single file will be milliseconds. Speed is not the problem the handoff thinks
  it is; clippy is the only slow part and does not belong in the edit loop.
- `ssnebula cortex_suite/cortex --no-clippy` reports **1,432 findings**:
  floating_function 775, vague_name 229, comment_essay 127, long_function 90,
  comment_phrase 46, comment_restates_code 31, deep_nesting 22, ...
  So the existing code does *not* follow these rules, and "match the
  surrounding style" (CLAUDE.md §6 and the agents' own system prompts) actively
  pulls agents the other way. Any integration must (a) say which wins for new
  code and (b) never report legacy findings, or it drowns in noise.
- cortex already has every hook point needed: `edit_guard` on PostToolUse
  Edit|Write (mcp_tool), `hook_cli` judging edit text at PreToolUse for VS Code,
  `capture_markers` on Stop, the managed CLAUDE.md section (`instructions.rs`,
  begin/end markers), `get_context` serving `kind="policy"` patterns first,
  `scoreboard` / `fired` heartbeats. `hooks-init` installs for both hosts.
- The README mentions a `/ssnebula` skill; none exists in `.claude/skills`.

## Where the handoff falls short

It is entirely *after the fact* (lint changed files, block, fix). That is
cleaning with a shorter delay. Writing it right the first time needs a
prevention layer in front, and the gate needs diff scoping so the 1,432 legacy
findings never surface. Three MCP tools are also more surface than needed: the
hook does the work automatically; one tool is enough for on-demand use.

## Design — four layers, most automatic first

### L1. Prevention: the rules are in context before the first line is written
1. **Style card generated from `ssnebula.toml`** (not hand-written, so it cannot
   drift): every lint at warn/deny rendered as one imperative line with its
   limit ("functions ≤ 60 lines", "no `get_` on getters", "no comment that
   narrates: 'this function', 'here we'..."). ~25 lines.
2. Ship it three ways:
   - into the **managed cortex_suite section of CLAUDE.md and
     copilot-instructions.md** via `cortex instructions` (always loaded);
   - as **`kind="policy"` patterns** tagged `style rust naming comments`, so
     `get_context` / `list_patterns` serve the relevant rules for the hint;
   - appended (top 5 matching the hint) to **`get_api_context`** results, the
     call every coding task starts with.
3. **Precedence rule, stated once:** new and rewritten code follows ssnebula;
   untouched surrounding code is not refactored (keeps §6 "smallest safe patch").
   This replaces "match the surrounding style" *for the lints only*.

### L2. Immediate feedback on every edit (the core)
- Extend the existing edit hook (`hook_cli` / `edit_guard`) for `*.rs`:
  lint the **post-edit text in memory** (apply `new_string` to the file, or
  take Write's `content`) and keep only findings whose span intersects the
  **lines this edit added**. File-level lints (long_file, comment_density,
  over_documented) fire only when this edit *crosses* the threshold.
  No baseline file needed; legacy findings are invisible by construction.
- Reply via `additionalContext`, capped at 5, deny first, each with the
  ssnebula `help` line. One line per finding, ~40 tokens each.
- **PreToolUse vs PostToolUse:** judging at PreToolUse (as VS Code already
  does) lets a `deny`-level finding (`dbg!`, `todo!`, unwrap in lib code)
  return `permissionDecision: "deny"` with the reason, so the bad text never
  lands and nothing needs cleaning. Warn-level stays advisory. *Decision for
  you* — see below.
- Integration mechanism: make ssnebula a **lib + bin** exposing
  `check_text(path, text, &Config) -> Vec<Finding>` and depend on it by path
  from cortex. In-process = no binary-path drift, no JSON schema to version,
  no 2 s subprocess budget. (Fallback if the colleague prefers no lib: add
  `--stdin --as <path> --lines a-b` to the CLI and shell out.)

### L3. Gate at the end of a turn
- Stop hook (already runs `capture_markers`): re-lint the added regions of
  every `.rs` file touched this session (cortex already records edits). If any
  deny-level finding remains, return `decision: "block"` with the list — once
  (`stop_hook_active` prevents loops). Warn-level goes into the closeout report.
- `closeout_session(build_pass)` refuses while deny findings are open, like a
  failing build.
- CI / handoff: the handoff's `--profile ci --fail-on` CLI work, low priority.

### L4. Learning — the system gets better without anyone curating it
- Every finding the hook delivers is logged (lint, file, session). A lint that
  keeps firing for agents becomes an **anti-pattern proposal** through the
  existing recurring-failure path ("agents keep writing `get_` getters:
  wrong/correct"), so it is pushed earlier next time.
- **Scoreboard metric:** findings per 100 added lines, per session, per lint.
  This is the acceptance number — it should fall week over week. `fired`
  gets a heartbeat for the lint hook so a silent uninstall is visible.
- Lints that agents keep "fixing" in ways the user rejects → signal to tune
  `ssnebula.toml`, surfaced in AWAITING YOUR REVIEW.

## Build order (each step usable on its own)

1. ssnebula: lib target + `check_text` + line-range filter + unit tests
   (colleague's repo — coordinate, or do the `--stdin` CLI fallback).
2. cortex: style-card generator from `ssnebula.toml` → `cortex instructions`
   section + policy patterns. *(Prevention alone, smallest change, ship first.)*
3. cortex `hook_cli`/`edit_guard`: diff-scoped lint → `additionalContext`;
   `hooks-init` installs for Claude Code and VS Code; heartbeat in `fired`.
4. Stop-hook gate + closeout refusal on deny.
5. Logging → scoreboard metric → recurring-lint anti-pattern proposals.
6. One MCP tool `style_check(paths?, explain?)` for on-demand full scans and
   lint explanations (replaces the handoff's three tools); a `/ssnebula` skill.
7. CLI profiles / `--fail-on` / JSON `schema_version` for CI.

Verification per step: unit tests in ssnebula and cortex; then an E2E replay —
feed recorded Edit payloads from `.cortex/session-trace.jsonl` through the hook
and check (a) no legacy finding is ever reported, (b) a planted `get_x()` /
`dbg!` / narrating comment is caught, (c) hook latency < 50 ms per edit.

## Decisions needed from you

1. ~~Rules vs existing code.~~ **Decided 2026-10-24: ssnebula defaults
   everywhere for new and rewritten code**, including `floating_function`, even
   where neighbouring code differs. Untouched code is still not refactored.
2. ~~Pre-edit deny~~ **Decided 2026-10-24: never block an edit.** Blocking at
   edit time would stop temporary debug output (`dbg!`, the only error-level
   lint by default) from being written at all, which gets in the way of
   debugging. Edits are only reported. Error-level findings block only at
   "done": `closeout_session` and a commit (pre-commit check in each git repo).
   The end-of-turn hook lists them without stopping the turn, since the user
   may be mid-debug across turns. This replaces the PreToolUse deny in L2 and
   the Stop-hook block in L3.
3. ~~Library dependency~~ **Decided 2026-10-24: library; revised the same
   day.** ssnebula now has a lib target, but its GitHub repo is private, so a
   cargo dependency would stop cortex_suite building for anyone without access.
   cortex runs `ssnebula rules` instead (CORTEX_SSNEBULA, then
   `<workspace>/ssnebula/target/{release,debug}`, then PATH). Switch to the lib
   dependency if the repo becomes public or is published.

## Progress

- **Step 1-2 done 2026-10-24 (uncommitted).** ssnebula: lib + bin split
  (`src/bin/ssnebula/`), a full rule text per lint (`lint!` 5th argument),
  `Lint::terms` quoting limits and word lists from config and code, `guide.rs`,
  `ssnebula [DIR] rules`. Same 1,432 findings on cortex before and after;
  ssnebula passes its own scan with clippy. cortex: `style_rules.rs` writes
  `.claude/rules/rust-style.md` (`paths: **/*.rs`) and
  `.github/instructions/rust-style.instructions.md` (`applyTo: **/*.rs`) from
  `cortex instructions`, stamped like the managed section (hand edits kept until
  `--force`). 5 new tests pass, 15 instructions tests pass. Claude Code was seen
  loading the rules file on reading a `.rs` file. CLAUDE.md, the Copilot file
  and both templates carry a short pointer section, no summary of the rules.
- **Step 3 done 2026-10-24.** `ssnebula check FILE --lines a-b,...` (JSON, one
  file, unparsable mid-edit files give nothing). cortex `edit_lint.rs` runs it
  from `tool_edit_guard` (Claude Code, PostToolUse) and from `cortex hook`'s
  PostToolUse branch (Copilot), on the lines the edit's text now occupies (more
  than 3 places = ambiguous, skipped). At most 5 findings listed, errors first;
  never blocks. Heartbeat `edit_lint` in `fired`. Seen live: a Write reported 3
  findings, an Edit beside them reported only its own line, the fix was silent;
  Copilot payload 35 ms. 403 cortex tests pass; cortex findings unchanged at
  1,432.
- Next: step 4, closeout refuses while error-level findings remain in lines
  written this session, plus the commit-time check.
