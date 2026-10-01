# Token efficiency: measured bill, validated levers, plan (2026-09-30, revision 3)

**Status.** Steps 1–5 are done and verified offline against real sessions (§0). Both
experiments are settled by measurement: E2 is rejected, and E1 is not recommended for this
workspace. Nothing is committed to git.

Revision 2 follows the user's direction:
- aim for big, automatic savings;
- keep every tool available and cost the user no convenience;
- look for ideas in lesser-known research and solo-developer experiments, then confirm each one against current official sources before relying on it.

Revision 1's small toggles have been dropped; §4 lists them with the reason. The numbers
come from the Claude Code transcripts in `~/.claude/projects/-Users-user-FlowMake/`
(14 days, 27.0k main-thread calls) and the VS Code Copilot chat logs (30 days). They are
priced at Anthropic's official per-model rates. §7 gives the method.

## 0. Revision 3: what shipped, and what it measured

**Step 1 (model rule).** Recorded here and in memory; nothing to configure.

**Step 2 (compact earlier).**
- `CLAUDE_CODE_AUTO_COMPACT_WINDOW=600000` in `.claude/settings.local.json`, and
  `# Compact instructions` in CLAUDE.md and the template.
- **New: `restore_after_compact`**, a SessionStart hook with the `compact` matcher
  (`cortex/src/restore.rs`). The transcript keeps everything before a compaction boundary,
  so the hook reads back what a summary blurs, with no model in the loop:
  - the latest requests;
  - files edited;
  - commands whose last run went green;
  - commands still failing, with the end of their output verbatim;
  - the agent's last words.

  It stays within 6,000 characters (the host caps a hook at 10,000) and is also stored as
  a checkpoint. On this session's 30 MB transcript it took 119 ms and returned 1.3k
  characters. It is installed through `cortex hooks-init` as hook set v5, so other
  machines get it at their next server start. It has not yet fired in a live compaction:
  this session's servers predate it. The heartbeat (`cortex fired`) will show the first
  run.

**Steps 3 and 4.** Done as planned. `MEMORY.md` went from 28.2KB to 21.7KB with every
entry kept.

**Step 5 (structure tools as the default).** quartz-ctx gained `get_source`,
`find_references` and `get_outline` (`quartz-ctx/src/nav.rs`):
- They read from disk at call time and include private items.
- Rust is parsed by syn. WGSL is read both in `.wgsl` files and inside Rust string
  literals, where space_soup's shaders live. The other nine languages go through
  tree-sitter.
- Each file version is parsed once.

Replayed against 14 days of real grep → read episodes:

| Question | Result |
|---|---|
| Definition reads (361): does `get_source` find the definition? | 337 (93%). Every remaining miss is code deleted since, a parameter, or a tree outside the roots |
| Same files, the agent's own sed range size and offset: was the read complete? | No in 36%: the range ended before the definition did. 16% also skipped the doc comment |
| Cost against grep + sed on the same files | 38% fewer characters; about 48% fewer calls (2.06 → 1.08 per lookup, counting the 6% of reads agents actually continued) |
| `find_references` against the agent's own `grep -n`, re-run today (44 searches) | 194 of 194 code uses found, none missed. 138 substring-only and 144 comment/string hits filtered out. 37 uses that `head` had cut were shown |
| Reference answer size | 1.8× grep's on those searches: it lists what `head` cut and names each enclosing function |

- **Always loaded.** The three tools carry `_meta["anthropic/alwaysLoad"]: true`, so they
  load like Read and Grep instead of sitting behind a tool search (~700 tokens a call,
  about $3.5 per 14 days). The first build used `anthropic/defer_loading: false`, a key a
  summarizing web fetch had invented; the restarted session still listed the tools as
  deferred. The documented key was then confirmed in the raw docs
  (`code.claude.com/docs/en/mcp.md`, "Exempt a server from deferral").
- **Usable wherever grep is (2026-10-01).** Reading the Claude Code 2.1.284 client showed
  two more reasons an agent would fall back to grep, both now fixed:
  - An MCP tool without `annotations.readOnlyHint` is treated as a writer. Plan mode
    refuses it ("Cannot call ... while in plan mode") while Grep and Read go through,
    and its calls never run in parallel. Every quartz-ctx tool and 24 read-only cortex
    tools now declare `readOnlyHint: true`. Tests pin the list both ways, so a writer
    cannot claim to read only.
  - quartz-ctx answered `initialize` only after building its index (1,829 ms). The
    client builds the first prompt without waiting for servers that are not
    `alwaysLoad` at the server level, so a session could start without the tools. The
    index now loads on a thread: `initialize` answers in 3 ms and the first tool call
    waits for the index (about 1.4 s, once).
  - None of this reaches a resumed session. Claude Code pins each session's system
    prompt and tool list in a `prompt_snapshot` transcript entry, taken at session start
    and again after every compaction, and `--resume` reuses it to keep the prompt cache
    valid. A tool-list change shows in new sessions, and probably after the next
    compaction. Restarting does not show it.
  - Verified in a new session (2026-10-01, 075cc1e0). Its first snapshot carries
    `get_source`, `find_references` and `get_outline` inline among 47 tools, while the
    other ten quartz-ctx tools and all cortex tools stay deferred. Plan-mode access
    rests on the client code and the served annotations; it has not been exercised
    live.
- **`restore_after_compact` fired live** at this session's compaction (2026-10-01). Its
  output arrived as SessionStart context, and `cortex fired` now shows its heartbeat.
- **Server instructions.** The server now sends MCP `instructions` and echoes the client's
  protocol revision, so a 2025-03-26+ host reads them.
- **Coverage.** Eight crates agents read but the index left out are now in
  `.cortex/index-sources.json`: bake, cortex, quartz-ctx, quartz_forge, space_soup_sky,
  crystalline, prism and courier_hex. About 600 of the 4,500 reads that followed a grep
  landed in them. Navigation also walks each crate's `examples/`, `tests/` and `benches/`.
- **Crash found by the replay, fixed in both servers.** proc-macro2's thread-local span map
  grows with every parse and wraps at 4 GiB, and the replay crashed quartz-ctx after a few
  hundred calls. quartz-ctx and cortex, which links the same parser, now clear the map at
  each request boundary.

**cortex's pre-code listings, ranked and bounded.** Replayed by the store's own procedure.
The ground truth is edit-guard firings, each joined to the hinted call in the *same*
transcript (112 pairs). Joining by time across sessions credits one session's firings to
another's hint.

| | Before | After |
|---|---|---|
| `get_anti_patterns`, 100 real calls on today's store | median 58KB, max 70.5KB. Every call is past the host's ~50KB inline limit, so the answer is saved to a file and only a 2KB preview of the oldest entries is shown | median 33.6KB, max 35KB |
| Fired trap expanded or in a short ranked section | 21% | 71% |
| Fired trap effectively unseen | 9% (cut off; growing with ~55 new entries a week) | 3% (id index only) |
| `list_patterns`, 60 calls | median 46.6KB; truncated at its cap in 10 calls, losing a matched pattern | median 34.3KB; nothing lost |

The new order is:
1. hint matches;
2. traps the edit guard fired in the last 14 days;
3. BM25-related entries;
4. the rest, newest first;
5. an id index once past 30KB.

Firing history is the strongest single signal: half of all fired traps share no word with
the hint. Every entry stays reachable (`expand_memory` now opens anti-patterns), the
expanded sets are identical, and the delta and full tiers are unchanged.

**Measurement.** `cortex scoreboard` prints a GUARDS block, using baselines from the same
code over the 7 days to 2026-09-30:

| Guard | Baseline |
|---|---|
| Growth per call in the 30 calls after a compaction, against elsewhere | 3.24k vs 2.32k |
| Calls per user turn | 31.1 |
| quartz-ctx calls against greps and sed range reads | 15 vs 3,083 and 2,940 |
| `sleep` waits | counted |
| Bill at list prices, per model | priced |

**E2 (API tool-result clearing): rejected offline.** Replaying every session's real growth
and real tool-result sizes on top of the 600k window, clearing **costs more in every
setting that triggers**:
- +1.5% to +5.6% as measured;
- +1% to +13.5% with tool output doubled as a stress test.

Each clear rewrites the remaining context at 40× the read price, and the reads it saves
before the next compaction never pay that back. No live test is warranted.

**E1 (rust-analyzer plugin): measured, not recommended here.** The component was
installed with the user's approval (rust-analyzer 1.97.1). The plugin itself was not
installed: that is a persistent change to every session.
- **Navigation** is already covered by the tools above, which also read WGSL; the LSP
  cannot.
- **Diagnostics after edits**, the plugin's remaining selling point, are wrong on this
  code. On space_soup, which `cargo check` compiles cleanly, rust-analyzer reports
  **70 errors, all false** (E0282 "type annotations needed"), plus 633 weak warnings. An
  agent given those after each edit would chase type errors that do not exist. cortex came
  back clean (0 errors).
- **Cost.** Indexing takes 15 s for cortex and 22 s for space_soup, at peak memory of
  **2.0 GB and 3.6 GB**. That is per workspace.
- Revisit if a rust-analyzer release clears the E0282 false errors on space_soup.

**New finding: what fills the context.** By characters, the main thread over 14 days:

| Kind | Share |
|---|---|
| Tool results | 38% |
| Tool *inputs* | 30% |
| Thinking | 26% |
| Assistant text | 4% |

- Inline Bash heredoc scripts were 20M characters, two-thirds of all tool inputs.
  - 58% of them are Python file patches, which are Edit-sized.
  - 1.8M were identical scripts sent again.
- Clearing tool results cannot touch any of this, which is part of why E2 fails.

## 1. Bottom line

- **The model choice was 72% of the bill.** The last 14 days came to **$6,991** at API
  list prices. A subscription meters the same tokens against its plan limits, so the
  figure is a proxy for limit consumption.
  - $5,050 of that ran on Opus 5.
  - Opus 5.5 reads its cache at **$0.20/MTok, against $0.50** on Opus 5. Its output
    and cache writes are also 20% cheaper.
  - The same work on Opus 5.5 costs **37% less**. New sessions already run 5.5, so this
    saving is arriving by itself. Keeping it takes one rule (Step 1).
- **The largest remaining lever is still compacting earlier.** It was re-simulated at
  Opus 5.5 prices, including returns after an idle break and the work of recovering
  after each compaction:
  - a 450k window saves **31–35%**;
  - a 350k window saves **38–41%**.

  It runs automatically and keeps every tool. Two protections are cheap:
  - Claude Code's documented `# Compact instructions`;
  - checkpoint hooks.
- **Make fewer calls, but fuller ones.** Every call re-reads the whole context.
  - Runs of one-at-a-time read-only calls are up to **15%** of re-read cost.
  - Polling-shaped waits are a further **~6%**.

  Batching independent reads and waiting on background jobs instead use tools the agent
  already has. The estimated saving is 5–8%.
- **The tools we built go almost unused.** In 14 days agents ran 4,889 `sed`/`cat` reads
  of Rust files and 3,088 Rust greps, against 27 quartz-ctx calls (Step 5).
- **Two experiments are worth testing:**
  - the official Rust code-intelligence plugin (rust-analyzer);
  - API-side clearing of old tool results (observation masking) through the documented
    `CLAUDE_CODE_EXTRA_BODY`.
- **Validation removed several ideas**, among them clearing old thinking (it loses money
  on Opus 5.5), a 5-minute cache, and Sonnet-executor routing. See §4.
- **Fix first: the memory index is over its loading limit.** 10 entries, including today's
  token-bill memory, do not load in new sessions.
- **Estimate.** About **60% below the last 14 days**, or ~35–41% below today's Opus 5.5
  baseline. That comes mostly from the model and the compaction window, and it does not
  touch reasoning effort.

## 2. The bill, priced per model

| Model | Calls | Cost (14 d) | Cache reads | Cache writes (1 h) | Output |
|---|---|---|---|---|---|
| Opus 5 | 15,812 | $5,050 (72%) | $3,864 | $859 | $327 |
| Opus 5.5 | 11,014 | $1,860 (27%) | $1,174 | $399 | $286 |
| Fable 5.1 | 130 | $81 (1%) | $20 | $45 | $15 |

Prices are from [Anthropic pricing][pricing], in $/MTok.

| Model | Read | 1-hour write | Output |
|---|---|---|---|
| Opus 5.5 | 0.20 (0.05× input) | 8 | 20 |
| Opus 5 | 0.50 | 10 | 25 |

- **No long-context premium.** The 1M window is billed at the standard rate.
- **No fast mode** appears in any call.

**At Opus 5.5 prices the same work costs $4,436.** The forward bill splits as follows:

| | Share | Detail |
|---|---|---|
| Cache reads | ~62% | |
| Cache writes | ~25% | half of them are returns after an idle break |
| Output | ~13% | 45% of it is thinking |

**What the re-read context consists of:**

| Item | Share of context read |
|---|---|
| Earlier turns' thinking (exact, from `output_tokens_details.thinking_tokens`) | 20.7% |
| Standing overhead on every call (system prompt + tools ~44k tokens; instructions, skills, deferred names) | ~14% |
| Conversation content | ~65% |

Within the conversation content:
- **Shell output is the largest part.** By command family:
  - code reads (`sed -n`, `cat`) 42%;
  - searches 21%;
  - Python 9%;
  - other 21%;
  - build, test, git and device output 7%.
- **Our own commands and scripts** are about a quarter.
- **cortex replies** are ~8%.

## 3. Plan: big, automatic steps (ranked)

### Step 1. Newest Opus everywhere (already arriving)
- **What happened.** The two long sessions started on Opus 5 and switched to 5.5 midway.
  Every newer session is on 5.5.
- **Rule: switch models only right after a compaction, never mid-context.**
  - A switch rewrites the whole cached context.
  - Six mid-session switches rewrote 2.7M tokens. Two of them went to Fable and back, at
    $20/MTok writes.
- **Saving:** 37% against the last 14 days. It is already in the forward baseline.

### Step 2. Compact earlier (largest lever, validated)
**Simulation.** It replays each session's real per-call growth at Opus 5.5 prices and
includes:
- cold returns, where a gap over 1 h rewrites the context;
- each compaction's read, summary and new prefix;
- Wall 0's measured recovery of +30k tokens per compaction.

At the current trigger it reproduces history to within 6%.

| Window | Saving | Compactions in 14 days |
|---|---|---|
| ~969k (today) | — | 58 |
| 600k | 22–26% | 103 |
| 450k | 31–35% | 152 |
| 350k | 38–41% | 212 |

Each compaction now takes a median 6 s.

**Evidence that a shorter context need not cost quality:**
- Bounded history (the last 5 tool calls plus a summary) raised completion from 71% to
  91.6% while cutting tokens 63% ([Less Context, Better Agents][lessctx], Jun 2026).
- ACON cut peak tokens 26–54% and improved success ([ACON][acon]).
- Accuracy falls as input grows ([Context Rot][rot]).

**The consequence to guard against.** Compression can raise *interaction* cost without
changing completion: the agent spends calls re-acquiring state it lost. One study measured
21 → 64 retrieval calls at 5× compression ([What Does Context Compression Cost an
Agent?][compcost], Aug 2026). So the guards measure recovery, not just success.

**Protections. All are automatic once set up.**
- **`# Compact instructions` in CLAUDE.md.** This is documented in
  [Claude Code costs][cccosts]. The section names what a summary must keep:
  - the objective and next step;
  - files in play and why;
  - open errors, verbatim;
  - commands that went green;
  - user constraints;
  - pending approvals.

  It costs ~100 tokens of always-on context.
- **L1 capture.** It already commits knowledge markers at PreCompact.
- **Checkpoint hooks (Wall 0 step 3).** PreCompact writes the task state, and
  SessionStart(`compact`) puts back at most 1.5k tokens of it. cortex already has
  `set_checkpoint`. This externalizes state, which is the condition under which agents can
  safely drop history ([When Can Agents Forget Their Reasoning?][forget], Sep 2026).

**Setting.**
- Use `autoCompactWindow` in FlowMake's project settings, or `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
- It takes a **plain integer**, such as `450000`. Per [env vars][ccenv], "450k" reads as
  450 and clamps to the 100k minimum.
- Once it is set, the status line's percentage no longer predicts when compaction runs.
- Confirm the setting took by reading the context size at the next compaction.

**Stages.**
1. 600k with the compact instructions on day 0.
2. 450k after a week.
3. 350k after another week.

Move to the next stage only while the guards hold (§5). Copilot's equivalent is in §6.

### Step 3. Fewer, fuller calls (tools the agent already has)
- **Batch independent reads, searches and edits in one message.**
  - Runs of consecutive single read-only calls (738 runs of 2, 412 of 3 … 313 of 6 or
    more) would cost up to **14.7% less to re-read** if issued three at a time.
  - Some of those reads depend on each other, so the realistic saving is about a third of
    that.
  - Parallel tool use is Claude's documented default and can be encouraged by prompting
    ([tool use][tooluse-impl]).
- **Wait on long jobs with `run_in_background` or `Monitor`, not by polling.**
  - Calls whose only purpose is waiting cost 5.5% of re-reads: `sleep` waits, logcat
    checks, tailing a log, `ps`/`pgrep`.
  - Exact repeats of the previous command add 0.8%.
  - Background-job notices are already the cheapest way to wait, at 3.6% of cost for 91
    turns.
- **Apply.** Add two lines to CLAUDE.md and `copilot-instructions.md`.
- **Saving:** ~5–8% of the bill. Quality is unaffected, and the agent uses its tools more,
  not less.

### Step 4. Fix the memory index (correctness first)
- **The problem.** `MEMORY.md` is 26.8KB against the loader's 24.4KB limit, so the last 10
  lines are cut off in new sessions.
- **The fix.** Rewrite each index line to 150 chars or less, as the loader asks. The detail
  stays in the topic files.
- **Kept word for word:** the rules that must act without a lookup, such as the Quest
  serial and never installing the game on the Quest.
- **Side effect:** saves ~4k tokens per call, about 1%.

### Step 5. Make the structure tools the default path (they exist but go unused)
- **The gap.** Over 14 days, agents ran these against the structure tools:

  | Raw access | Count | Structure tools | Count |
  |---|---|---|---|
  | `sed`/`cat` reads of `.rs` files | 4,889 | quartz-ctx calls | 27 |
  | greps over Rust | 3,088 | graphify calls | 1 |

- **Why it matters.** The stored measurement puts `get_api_context` at ~878 tokens against
  ~36,730 to read a module's files directly.
- **Why the tools go unused:**
  - Claude Code lists MCP tools by name only, so using one takes an extra load step.
  - The harness's auto-mode note suggests `sed` and grep for reading.
  - Our servers send no MCP server instructions. pixellab and claude-in-chrome do, and
    theirs reach the system prompt, as seen in this session.
- **Change.**
  - Give quartz-ctx, cortex and graphify short server instructions, one line per job:
    "a Rust symbol → `get_item NAME`, one call, with file:line". That is ~150 tokens
    across the three, so the agent reaches for them without being told.
  - Keep the CLAUDE.md routing as the fallback.
  - Pair this with E1, the official LSP option for navigation and diagnostics.
- **Measure:** structure-tool calls against Rust `sed`/grep per user turn, and code-read
  volume per task.
- **Estimate.** Searches and code reads are ~18% of the context read. Moving a quarter of
  exploration to packets many times smaller saves ~3–5% of the bill, with fewer reads of
  the wrong file.

### Experiments: each must beat Step 2 alone in a measured side-by-side

**E1. The Rust code-intelligence plugin** (`rust-analyzer-lsp` from the official marketplace).
- **What it does.** Anthropic recommends it to replace grep-then-read-the-candidates with
  go-to-definition. It also reports type errors after each edit without running a compiler
  ([code intelligence][cclsp], [costs][cccosts]).
- **Why it matters here.** Searches plus code reads are 63% of our shell output.
- **Unknowns.**
  - Whether it runs in desktop-app sessions. The docs say "terminal sessions" and exclude
    only cloud sessions.
  - Memory use while indexing.
  - The documented false-positive diagnostics in multi-crate workspaces.
- **Test.** One session. Compare grep and `sed` volume per task and the number of cargo
  checks against the baseline.

**E2. API observation masking.**
- **What it is.** `clear_tool_uses_20250919` passed through the documented
  `CLAUDE_CODE_EXTRA_BODY`, which merges JSON into every request.
  - The research supports it. Hiding old tool output matches LLM summarization at about
    half the cost ([The Complexity Trap][masking]). Anthropic calls tool-result clearing
    the lightest-touch compaction.
  - It would bring the idle clearing that Claude Code withholds from desktop sessions.
- **Consequences found while validating:**
  - Clearing invalidates the cache from the cleared point onward. On Opus 5.5 a rewrite
    costs 40× a read, so it must be batched with a high `trigger` and a large
    `clear_at_least`. TokenPilot and issue #94177 both say to batch the prune
    ([TokenPilot][tokenpilot], [#94177][i94177]).
  - It needs the `context-management-2025-06-27` beta. `ANTHROPIC_CUSTOM_HEADERS` might
    replace Claude Code's own `anthropic-beta` header, which would silently drop its betas.
  - A top-level merge would replace Claude Code's own `context_management`. Claude Code
    sent that field 5 times near the context limit.
  - Masked results may be fetched again. The guard is the re-read rate.
- **Test.** One throwaway session first: check that the requests succeed,
  `applied_edits` appears, and the other betas still work. Only then run a side-by-side
  against Step 2.

### The user's call: reasoning effort
- **What it is worth.** Thinking is ~21% of the bill, and 90% of it falls in tool-loop
  steps. Each 10% less thinking saves ~2%.
- **Why it is not in the plan.** Effort is the only thinking control on Opus 5.5. Lowering
  it trades quality, so it stays out unless the user wants an A/B.
- **Never change it mid-session**, because the change invalidates the cache.

## 4. Validated, then rejected or dropped

| Idea and where it came from | What validation found |
|---|---|
| Clear old thinking: `clear_thinking_20251015` through `CLAUDE_CODE_EXTRA_BODY` ([context editing][ctxedit]; a Sep 2026 paper reports −33% cache reads) | **Loses money on Opus 5.5.** Keeping 1 turn saves $411 of reads per 14 days but costs $440 of rewrites, since each clear invalidates the cache from that turn. Keeping 2 turns is worse. It paid only at Opus 5's read price. |
| Claude Code's own idle microcompact (keep last 5 results after 65 min idle) | Not available here. It runs only in `repl_main_thread` sessions; SDK and desktop sessions are excluded, and its values are hard-coded with no setting ([#98204][i98204]). This is why it never fired for us. Upvote the issue. |
| "Resume from a summary" dialog | It appears only when a closed session is reopened from its transcript. Our 128 cold returns were in open desktop sessions. |
| 5-minute cache TTL (writes at 1.25× instead of 2×) | Worse. 470 pauses of 5–60 min would each rewrite the context: 254M tokens, against 104M units saved on writes. |
| Advisor routing: Sonnet executor plus Opus advisor, "73% cheaper than Opus end-to-end" | Opus 5.5's cache reads cost the same as Sonnet 5.5's ($0.20), and reads are ~62% of our bill. At most ~20% is possible, and it lowers the executor's quality. |
| Shell-output filter proxies (rtk, snip: "60–90%") | 63% of our shell output is code reads and searches, which a filter cannot cut. Build, test, git and device output is 7%, worth about 1–2% of the bill. Filtering also risks hiding the one line that matters. |
| API compression proxies (ClaudeSlim: "60–85%") | They sit in the auth path, rewrite prompts and break cache prefixes. |
| Read-once dedupe | Exact duplicate reads are 0.1%. Re-reads of unchanged lines in the same segment are 10%, partly needed in long contexts. |
| Idle consumers: loops, cross-session messages, goal check-ins ([costs][cccosts]) | None in our data. |
| E2: API tool-result clearing (`clear_tool_uses_20250919` through `CLAUDE_CODE_EXTRA_BODY`; "matches summarization at half the cost") | **Costs more on Opus 5.5 at every setting that triggers**: +1.5–5.6% over the 600k window alone, and +1–13.5% with tool output doubled. Each clear rewrites the rest of the context at 40× the read price. Tool results are only 18–33% of context growth here. |
| Revision 1's small items: prompt suggestions off, hiding plugin skills, MCP or browser toggles, Copilot tool diets, a `/compact`-before-a-break habit | Dropped, because they cost convenience or tool availability. For reference: prompt suggestions are one full-context cache read per reply, ~3% at Opus 5.5 ([costs][cccosts]). Browser tools are 1.2%. Plugin skills are 0.8%. |

## 5. Guards: measure interaction cost, not just success

Each stage of Step 2, and each experiment, reads these numbers from the scoreboard
(§7's measurements, folded into `cortex scoreboard`). Every cost figure is priced per
model.

- **Recovery growth** over the 30 calls after a compaction. The baseline is 2.58k per
  call, against 1.59k elsewhere.
- **Re-reads after a compaction:** files read again that had been read before it.
- **Calls per user turn.**
- **Corrections per 100 user turns,** from the challenge hook.
- **Failure-hook recurrences.**

**Kill rule.** For any stage, step back one stage if, over a week:
- recovery growth or calls per turn rises more than 50% above baseline; or
- corrections rise more than 25% above baseline.

## 6. Copilot

**Copilot is small for us.** Over 30 days it made 41 logged requests with 3.1M prompt
tokens, against Claude Code's 13.6B tokens read in 14 days. Its billing has been
token-based since 2026-06-01 ([usage-based billing][ghbilling]).

- **`copilot/auto` showed 0% cached tokens.** Pinned models (gpt-5.3-codex, Haiku 4.5)
  showed 73–91%.
  - If confirmed, pinning a model for agent sessions makes most of the input
    cache-priced.
  - Check the log field before acting, since auto's metadata may simply not record cache
    use.
- **Summarization point.** If the 1M window is selected, set
  `chat.advanced.summarizeAgentConversationHistoryThreshold` (a ratio or a token count).
  This is Copilot's version of Step 2 ([mechanics][vscompact]).
- **Instructions.** Step 3's two lines go into `copilot-instructions.md`. Copilot keeps all
  its tools.

## 7. Method (to reproduce)

- **Calls.** Assistant entries with `usage`, deduped by `message.id`, with `isSidechain`
  excluded. A call's context is input + cache_read + cache_creation.
- **Prices.** Per `message.model`, at the official rates in §2.
- **Thinking.**
  - Exact, from `output_tokens_details.thinking_tokens`. Its re-read share resets at each
    `compact_boundary`.
  - Old thinking stays in context: growth after a call tracks that call's billed output
    with a slope of 0.995 (r² 0.81).
- **Clearing old thinking.**
  - Saving: the thinking of earlier *user* turns, read on each later call.
  - Cost: at each new user turn, everything from the cleared turn onward is written
    again.
- **Window simulation.**
  - Per-call growth, capped at 100k.
  - A compaction costs a read of the whole context, an 8k summary, the median 79k
    post-compaction prefix written, and +30k recovery over the next 30 calls.
  - A gap over 1 h rewrites the whole simulated context.
- **Batching.** Runs of consecutive calls that each issue one read-only tool, priced as if
  issued three at a time.
- **Copilot.** Usage fields (`prompt_tokens`, `completion_tokens`, cached tokens) in
  `workspaceStorage/*/chatSessions/*.json`.

[pricing]: https://platform.claude.com/docs/en/about-claude/pricing
[cccosts]: https://code.claude.com/docs/en/costs
[ccenv]: https://code.claude.com/docs/en/env-vars
[cclsp]: https://code.claude.com/docs/en/plugins/code-intelligence
[ctxedit]: https://platform.claude.com/docs/en/build-with-claude/context-editing
[tooluse-impl]: https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/implement-tool-use
[lessctx]: https://arxiv.org/abs/2606.10209
[acon]: https://arxiv.org/abs/2510.00615
[rot]: https://trychroma.com/research/context-rot
[compcost]: https://arxiv.org/abs/2608.16370
[forget]: https://arxiv.org/abs/2609.29875
[masking]: https://arxiv.org/abs/2508.21433
[tokenpilot]: https://arxiv.org/abs/2606.17016
[i94177]: https://github.com/anthropics/claude-code/issues/94177
[i98204]: https://github.com/anthropics/claude-code/issues/98204
[ghbilling]: https://github.blog/news-insights/company-news/github-copilot-is-moving-to-usage-based-billing/
[vscompact]: https://alexop.dev/posts/how-vscode-copilot-chat-conversation-compaction-works/
