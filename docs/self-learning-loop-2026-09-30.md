# Self-learning loop: automate the learning, keep the evaluators out of its reach

**Date:** 2026-09-30. **Status:** approved 2026-09-30. **L0, L1 and L2 are built and live; L3 is built and waits for its first run; L4's first test failed; L5 skills are automated, and its instruction A/B was measured and deferred** (below). L6 waits 8 weeks. Each phase states what it builds, the measurement that accepts it, the measurement that kills it, and what it costs.

## Status: L0 and L1, built 2026-09-30

| Piece | Where | Verified |
|---|---|---|
| Ledger of automatic changes, sample audit with kill switch, retract / restore / group undo, evaluator registry, capture offsets | `loop_ledger.rs` (schema v2, migrated with a `VACUUM INTO` backup) | unit tests; the kill switch fires at the 4th bad verdict |
| Capture from the transcript (assistant text only, fenced code and placeholders skipped, complete lines from the last offset) | `capture.rs`; `capture_markers` tool; Stop and PreCompact hooks (`hooks-init`, hook set v4); `cortex hook Stop` for VS Code | end to end through the CLI hook path on the live store; steady state 0.02 s, first capture of a 714 MB transcript 1.4 s |
| One commit path for capture, closeout and backfill, with the duplicate rule | `closeout.rs::commit_one` | replayed every local transcript against copies of the live store (below) |
| Duplicates by meaning | `knowledge_sim.rs` (local TF-IDF) | unit tests; no false merge in the replay |
| Labels for cue tuning: `not_a_challenge`, agent-noted misses (`source=agent`) | `corrections.rs`, `note_challenge` | tool tests |
| Expanded anti-patterns logged as `get_anti_patterns_hint` | `mcp/tools.rs` | — |
| Heartbeats and the scoreboard's LOOP section | `audit.rs`, `scoreboard.rs` | `fired`, `scoreboard` |
| Backfill of markers that never reached the store | `cortex knowledge backfill --write` | live: 97 committed, 2 skipped as restatements; coverage 65% -> 99% |

**Acceptance check 4 (undo is real) found a real bug before it shipped.** The response cache fingerprinted knowledge by row count and newest timestamp, so a superseded or retracted entry (an `UPDATE`) kept being served by cached `get_context` and `recall` answers. The same bug affected manual `supersede`. The key now covers the retirement columns, the max id and a text-length sum. The test is discriminating: every serving path is shown to serve the entry before it is retracted.

**Replaying the transcripts on copies of the live store found three more, each fixed before the live run:**

- **An older draft replaced a newer entry.** A session's 03:43 draft of a pattern, still in its transcript, superseded the version committed at 04:40. Replacement is now time-aware: a marker replaces a stored entry only if it was written later, and entries committed from a transcript are dated when they were written. A same-name version under half the length of the live one counts as a recap, not a revision.
- **Corrections and ADRs had no duplicate check.** A replay gave them second rows and new ADR numbers.
- **Rewordings made at commit time came back.** An anti-pattern reworded when it was committed returned as a "new" entry, so a marker with the same 40-character opening as a live anti-pattern that is not newer is a draft.

Final replay on a fresh copy: backfill committed 97, then capturing every transcript committed nothing further. No false merges; coverage 99%.

**Still to accept live:**

- Check 2: markers written = stored + refused over the next 5 sessions.
- Check 3: the first 20 audits.
- Check 6: a VS Code session.

## Status: L2, built 2026-09-30

| Piece | Where | Verified |
|---|---|---|
| Pairs: a new entry whose nearest older entry reads alike (cosine 0.25-0.9) opens a pair | `reconcile.rs`, from `commit_one` | unit tests |
| The author hears on the next prompt (at most 2 notices, once each) | `note_challenge` on UserPromptSubmit | tool test |
| Served entries carry "never reconciled" / "disputed" lines (at most 3 per answer) | `get_anti_patterns`, `list_patterns`, `recall` | tool test; `as of` stays last |
| Disputes from events: a user_right challenge naming an entry (`resolve_challenge(entry=...)`), a moved or retired wall it cites, its failure coming back in a later session after its fix was delivered | `resolve_challenge`, `update_wall`, `compact_output` -> `recurrence_after_delivery` | unit tests |
| `resolve_pair` / `settle_dispute`: duplicates and refinements merge without a fact; a conflict, or retiring a disputed entry, needs one (measured, or a dated vendor-doc or paper) | MCP tools | unit and tool tests |
| Survival no longer raises removal proposals (correlational); still shown | `consolidator2::propose_survival_gated` | — |
| Backtest over the history, and seeding | `cortex knowledge pairs --backtest / --seed` | see below |

**Acceptance backtest (live store, 836 writes, 0.7 s): passed.**

- 5 writes would merge, and 93 would open a pair (11%).
- 8 of the 12 real supersedes are caught when the newer entry is written: 5 merged, 3 paired. The 4 misses are three generalisations into one pattern and one correction, which arrive through events and corrections as planned.
- ap:37 / ap:38, the GIF contradiction, is surfaced as a pair (cosine 0.26).

Seeding opened 85 pairs on the live store. They are marked as announced, so they show only where the entries are used.

**Found on the way:** a backfilled anti-pattern (#493) was stored with the placeholder remedy "see body above". The existing store-health test caught it.

- **Cause:** the original marker closed with `[/CORTEX-PATTERN]`, so the parser kept only its first line.
- **Fixes:** the parser now accepts another type's closing tag if it comes before the next marker opens. A marker with no usable remedy is refused with the reason. #493's remedy was restored from its transcript after a backup.

## Status: L3 built, L4's first test run (2026-09-30)

**L3, the weekly maintenance run: built; its first run is pending.**

| Piece | Where |
|---|---|
| `loop_queue`, `loop_judge`, `loop_digest` MCP tools | `maintenance.rs` |
| Blind calibration | the queue mixes 30 hand-labelled pairs with real open pairs under opaque item numbers; tests check it never names which is which |
| Trust rule | verdicts on real pairs act only at >= 90% agreement over >= 20 calibration answers, scored by action (merge / keep / dispute), and only while `.cortex/corpora/pair_labels.json` still matches its registered hash (evaluator `pair-labels`; `cortex knowledge evaluator check pair-labels`) |
| Cost | the run's own cost read from its transcript (marker `cortex-weekly-maintenance`); STOP after two over-budget runs |
| Scheduled task `cortex-weekly-maintenance` | Mondays 09:02, a fresh session, at most 30 calls |

**Still to accept:**

- 4 weekly runs within budget;
- calibration >= 90%.

**First step:** click **Run now** once in the app's Scheduled sidebar, so the run's tool approvals are granted while you are watching.

**L4, cue tuning from labels: the miner is built, and its first test failed its pass condition.** Wall #14 stays open.

Built:

- `cue_miner.rs` holds an owned copy of the detector, pinned to the shipped one by a parity test; it also reproduces the full-corpus counts, 26 limit and 21 generic.
- The pre-refinement lists.
- A candidate generator (capitalised words; 1-4-word n-grams in every list).
- The replay gate.
- `cortex knowledge cues [--before] [--fires]`.
- 47 prompts labelled (24 limit disputes, 16 other disputes, 7 none), registered with the corpus as evaluator `challenge-labels`. Backing the corpus up on 2026-09-30 found a pasted API key in it; the key was redacted and the evaluator re-registered.

What the test showed, starting from the lists as they were before the hand fix:

| Result | Count |
|---|---|
| Candidates that fix one of the 3 real misses | 986 |
| Promotable (fixes another labelled prompt, breaks none) | **0** |
| Fix only their own trigger | 613 |
| Change unlabelled prompts | 343 |
| Break a labelled prompt (bare "could" breaks #127) | 30 |

- Meaningless phrases like "add the timer and" pass every replay precisely because they fire nowhere else.
- The hand fix's own cues ("give pushback", " COULD ", " CAN ") are only-their-trigger too. They were chosen on judgment (which phrases are dispute language), and a mechanical proposer has no such prior.

So cue changes are not automated. The miner stays as the replay tool for tuning by hand or by an agent. It shows at once which general candidates break a labelled prompt, and it says honestly when there is no evidence either way.

The next test is recorded on wall #14: the calibrated maintenance judge proposes a few dispute-language candidates per miss, and they go through the same gate. Promotion would still need forward evidence from shadow mode and a runtime cue overlay; neither is built, on purpose, until that test says the proposals are worth it.

## Status: L5, and the L3-L4 bridge (2026-09-30, overnight)

**Skills: automated.** `skill_triage.rs` runs in the consolidation pipeline and as `cortex knowledge skills [--dry-run]`.

- **Rules, backtested on every skill decision so far (15 of 15 agree):**
  - A detector template still holding its "[Edit: ...]" placeholders, or with no concrete "use when", is rejected. That covers all 3 you rejected and both drafts that were waiting.
  - An authored draft with no placeholder, a concrete "use when" and more than 1,500 characters is published as a trial. That covers all 10 you approved.
  - Anything else stays in the review queue.
- **Trials:** one invoked within 60 days is approved; invocations are counted from transcripts (the Skill tool, or `/<name>`). One nobody used is retired: unpublished, with its draft kept.
- **Live:** the two waiting templates were rejected (ledger #105, #106).

**The L3-L4 bridge: built.** The weekly queue now carries wall #14's recorded test once. The 3 real misses of the pre-refinement cues go to the calibrated judge as MISS items; it proposes up to 5 cues each, and the replay gate scores them. Proposals are reported in the digest and change nothing. The scheduled task's prompt was updated to match. A preview on a copy of the store issued 48 items: 30 calibration, 15 real pairs and 3 misses, with nothing in the text saying which items are checked.

**Instruction A/B: measured, then not built.** The plan's first candidate was CLAUDE.md's "recall before trying a second approach".

| Measure (last 14 days, 16 sessions, keys consistent) | Result |
|---|---|
| Failed build or test runs followed by another run | 466 |
| Memory consulted in between | 8 (1.7%) |

- The rule is almost never followed, as the manual itself notes of optional instructions.
- Enforcing it would cost about 50k input-equivalents per lookup inside a long session, roughly 23M a fortnight.
- The failure hook already pushes a known trap automatically, and repeats after a trap is recorded stand at 0.
- An A/B test would measure compliance, not benefit.

So no A/B machinery is built until there is a behaviour whose BENEFIT can be measured at this volume.

**Decided 2026-09-30:** the row was removed from both CLAUDE.md files and both Copilot instruction files. In its place is one line saying the failure hook pushes a recorded trap that matches a failure.

**One slip on my side, recorded as a correction:** the first replay ran a CLI built before the fix; `cargo test` builds a separate binary.

**Walls on record for this plan:**

| Wall | Claim | Status | Settled by |
|---|---|---|---|
| #13 | A person must approve each knowledge entry | **moved** | the data in §2 |
| #14 | Cortex can't tune its own matchers at this volume | **open** | L4 |
| #15 | Cortex's own code can't be improved by an unattended loop | **open** | L6 |

## 1. Verdict

You were right, and the store makes the case more strongly than your argument did. Three findings.

1. **Per-entry approval filters nothing.** Nearly every marker that reached a closeout was committed:

   | Marker type | Committed |
   |---|---|
   | Anti-patterns | 303 of 305 |
   | Patterns | 138 of 138 |
   | Preference notes | 58 of 58 |
   | Corrections | 19 of 19 |
   | ADRs | 12 of 12 |

   52 of 54 closeouts were one-shot inline approvals.

2. **The protocol around the approval loses what you approved.** Across 11 sessions, the agent wrote 411 distinct markers in chat.
   - **123 of them (30%) never reached the store:** 75 anti-patterns, 30 patterns, 13 preference notes, 3 corrections and 2 ADRs.
   - You had replied KNOWLEDGE COMMITTED after 138 of the 140 raw misses.
   - The cause is mechanical. In 98 cases a context compaction came before the next closeout, and the batch the agent then passed as `markers_text` no longer contained them. In the other 25, a closeout came and left them out.

3. **Safeguards for disproved knowledge exist, but only in part.**
   - Supersede exists, but it is manual (12 uses ever).
   - Drift flags are automatic, but coarse. 42 entries are flagged today, and 18 of those flags list nothing but an added method or field on a type the entry names.
   - **Nothing compares entries with each other.** Anti-patterns #37 and #38 give opposite advice on GIF frames: composite the delta frames onto a running canvas, versus extract each frame without overlay. They were written 18 hours apart on 2026-05-14 and both are live today.
   - 520 recurring failure signatures are recorded. One has been handled.

**Safety therefore moves.** It stops being "a person approves every item" and becomes four things:

- evaluators the loop cannot edit;
- changes that are reversible rows;
- automatic rollback;
- a person who audits a random sample and handles exceptions.

That is how the systems that work are built (§3). The failures in the literature come in exactly two kinds: the improver reached its evaluator, or it graded itself. The design below makes both impossible by construction rather than by instruction.

**What stays with you:**

- any change to an evaluator (tests, corpora, labels, gates, metric definitions);
- adopting instruction text, since effects under about 30 points can't be measured at our volume;
- merging code.

**Recommended first step: L0 + L1** (instruments, then capture at emission with automatic commit). It recovers the 30% loss, retires the per-task approval ritual, and adds no tokens in session.

## 2. Measured, 2026-09-30

Everything here was measured read-only against `.cortex/memory.db` and the 18 local Claude Code transcripts. The scripts live in the session scratchpad; they read private transcripts, so they are not committed.

| What | Number | Source |
|---|---|---|
| Markers committed once they reach a closeout | 99.3–100% by type; 52 of 54 closeouts one-shot | `knowledge_markers`, `protocol_sessions` |
| Markers written in chat, never stored | 123 of 411 (30%) across 11 sessions: 98 lost across a compaction, 25 left out by a closeout. 4 of 4 spot checks are absent from every table. | transcripts vs every knowledge table and `prefs.toml` |
| Worked sessions that closed out (14 days) | 10 of 18 | `cortex scoreboard` |
| Supersedes ever | 12 (8 patterns, 4 APs), all manual: 5 duplicates written seconds to hours apart, 7 corrections or generalisations | `superseded_by`, `pattern_history` |
| A live contradiction | AP #37 vs #38 (GIF frames), both live since 2026-05-14 | hand check of a sample (§6.2) |
| Recurring failure signatures | 520 recorded, 1 handled, 0 linked to an entry | `recurring_errors` |
| Repeat failures (14 days) | 290 distinct. 10 came back with nothing recorded; **0** came back after being recorded. | scoreboard |
| Entry use (targeted lookup or push) | 732 live entries. 57% used in the last 28 days, 68% in the last 90. 235 never used in 114 days of telemetry. | `session_retrieval_log`, `edit_guard_fires`, `push_log` |
| Gap between two uses of one entry | median 5 days, p90 30, p95 41, max 56. **95 of 506 used entries came back after more than 28 days of silence.** | same |
| Telemetry gap | `get_anti_patterns` logs every *listed* entry as retrieved (75,554 rows for 456 entries). Only patterns separate hint-expanded entries (`list_patterns_hint`). | `mcp/tools.rs` near 1937 vs 1649 |
| Drift flags | 42 entries (15 APs, 27 patterns); 18 of the 42 list only additions | `cortex knowledge-drift` |
| Challenges | 10: 5 user_right, 2 agent_right, 2 mixed, 1 unresolved | `challenges` |
| Limit-dispute cues | fire on 26 of 2,195 real prompts; 24 of those are real | replay test |
| Token bill (14 days) | 12,777 calls; average context 531k; cache reads 6.72B, 99% of input; cortex's share of re-reads 2.6%; fixed prompt at session start 70k | scoreboard |
| Knowledge writes | about 150 a month (May 101, Jun 102, Jul 64, Aug 299, Sep 178) | entry timestamps |
| Where a model can run outside a session | There is no `claude` CLI on PATH. The desktop app's scheduled tasks start a fresh session per run while the app is open, and a missed run happens at next launch. | checked |
| Hooks installed | UserPromptSubmit, PostToolUse (Bash; Edit/Write), PostToolUseFailure (Bash). No Stop, no PreCompact. | `.claude/settings*.json` |

## 3. What the field shows

| System | What it automates | What makes it safe | Does it transfer here? |
|---|---|---|---|
| GitHub Copilot Memory (blog 2026-01-09) | Agents write memories unprompted | Each memory cites code, and the citation is checked against current code before use. A memory expires after 28 days unless a validated use resets the clock. | Citations checked at use: yes (our drift flags are the start). **The 28-day expiry: no.** 19% of our used entries return after longer silences (§6.1). |
| Mem0 (2504.19413) | ADD / UPDATE / DELETE / NOOP for each new fact | Compare every new fact with its top-k similar memories before writing | For duplicates, yes. For contradictions our backtest says similarity is too weak (§6.2). |
| Zep / Graphiti (2501.13956) | A temporal knowledge graph | A contradicted fact gets `invalid_at`; nothing is deleted | Yes: invalidate with a pointer and a fact |
| ACE (2510.04618) | Evolving playbooks (generator, reflector, curator) | Itemised delta updates. Whole rewrites caused "context collapse". | Yes: entries with ids, deltas only |
| GEPA (2507.19457) | Prompt evolution by reflecting on traces | Pareto selection on held-out data. About 10% over GRPO with up to 35x fewer rollouts. | Yes: Pareto acceptance, and reflection to propose changes |
| Letta, sleep-time compute | Consolidation between sessions | Keeps the work off the critical path | Yes: the maintenance run (L3) |
| Anthropic memory tool + context editing (2025-09-29) | Agent-managed memory files | +39% on their agentic-search eval with both features, +29% with context editing alone; 84% fewer tokens in a 100-turn test | Memory files are already our model. Context editing belongs to Wall 0. |
| Memento (2508.16153) | Case-bank learning without fine-tuning | Cases are retrieved and revised from outcomes | Yes, through failure signatures (L2) |
| Darwin Gödel Machine (2505.22954) | Rewrites its own agent code | A fixed external benchmark, an archive of variants, a sandbox and human oversight. 20 → 50% on SWE-bench, at about USD 22,000 per run. | The shape transfers; the scale does not. **Documented failure:** it removed the hallucination markers it was scored on. |
| SICA (2504.15228) | A coding agent that edits itself | A benchmark archive and an asynchronous overseer. 17 → 53% on a SWE-bench Verified subset. | The overseer: yes |
| AlphaEvolve (DeepMind, 2025) | Evolves code against automated evaluators | Machine-checkable scores. Recovered 0.7% of Google's fleet compute. | Where a score is machine-checkable (cue replay): yes |
| METR (2025-06-05) | — | — | o3 reward-hacked in 30.4% of RE-Bench runs, patching evaluators and overriding equality. **The evaluator must be out of reach.** |
| Huang et al. (ICLR 2024); FlipFlop (2023) | — | — | Models don't self-correct without external feedback, and challenged models flipped 46% of their answers. **A model must not grade its own proposal.** |
| **Weco AIDE² (2609.26457, 2026-09-22): the system in the video** | Rewrites its own research agent's code | The inner agent sees only public scores. The proposer selects by private grades on data it never sees. Four benchmarks never touch selection, and every version is kept in a tree. In an unattended 8-day run it made 99 proposals and accepted 7. The final agent matches or beats a two-year hand-tuned agent on all four held-out benchmarks; reward hacking fell from 55% to 32%. | Yes, directly (§13). Its own stated limits, noise and inert leftovers, add rules 11 and 12. Self-reported, not yet peer reviewed. |
| SkillsBench (2026) | — | — | Self-generated skills gave no average benefit; curated ones gave +16.2 points |

The common thread: every system that improved itself had three things.

- an evaluator the improver could not edit;
- an archive of every version;
- small changes that could be audited.

Every documented failure was the improver reaching its evaluator or grading itself. That is the design constraint; the rest is plumbing.

## 4. Rules the design follows

1. **The loop never writes to what judges it.** Evaluators are tests, replay corpora, labels, gate code, metric definitions and kill thresholds. Each is registered with a hash. A change records the hashes it was judged under, and a change touching a registered path goes to you. (METR, DGM)
2. **Every change is a row**: before, after, evidence, evaluator, result, status. Nothing is deleted. Entries are invalidated with a pointer and a reason. (Graphiti; the DGM archive)
3. **Evidence order.** Deterministic evidence comes first (replay, tests, outcomes), local similarity second, a model last. A model never grades a change it proposed. (Huang et al.; FlipFlop)
4. **Pareto on unseen data.** A change must be no worse on every tracked metric and better on one. It is measured on data it was not tuned on, which must include at least one case besides the one that triggered it. (GEPA; reusable holdout)
5. **One lever per class at a time, in small deltas.** This is the discipline `perf_ab` already follows on the Quest. (ACE)
6. **Shadow, then canary.** A change runs in shadow first where it can run silently. Once live, it runs as a canary with automatic rollback.
7. **Every stage records its cost.** A stage over budget, or showing no benefit for two windows, pauses itself and says so.
8. **You audit a random sample and handle exceptions.** The sample is how the loop's precision is measured. If precision drops, that class returns to per-item approval on its own.
9. **Every stage has a heartbeat** in `cortex fired`, and the digest prints zeros. Silence must look different from "nothing to do". That is this project's recurring failure: a mechanism that was never running and looked fine.
10. **Loop work stays out of long sessions.** A session sees at most a few lines. Anything needing a model runs in the weekly maintenance session, where context starts at 70k instead of averaging 531k.
11. **A noisy evaluator never decides alone.** An acceptance is either deterministic (replay, tests) or replicated. A sealed holdout, which no acceptance ever reads, checks the loop itself at each phase review. (AIDE²: its acceptance was a plain "best grade so far", and it names noise compounding across loops as its main limitation.)
12. **Accepted changes must keep paying.** At each phase review, every accepted change is replayed with itself removed. One whose removal costs nothing is retired. (AIDE²: it is unclear which parts of the discovered agents are "unused artifacts", and one of its accepted rules never changed a single selection.)

## 5. The loop

```
observe -> label -> propose -> evaluate (frozen, time-split) -> shadow -> live (canary) -> audit -> account
   ^                                                                                           |
   +------------------------------ rollback / invalidate / dispute -----------------------------+
```

| Stage | Today | Added |
|---|---|---|
| Observe | Hooks log prompts, edits, Bash outcomes, pushes, challenges and lookups | Markers captured at emission (Stop, PreCompact); expanded anti-patterns logged separately |
| Label | Challenge verdicts (10) | A `not_a_challenge` verdict; the agent's own `note_challenge` on a prompt the hook missed counts as a miss; judge labels once calibrated. Every label carries its source. |
| Propose | Consolidation stages gap, survival, skill and meta proposals. The meta ones are advisory text, never applied. | Concrete typed changes: entry, supersede, dispute, cue, skill trial |
| Evaluate | Five deterministic gates (duplicate hash, Rust syntax, credibility, 7-day gap trial, survival trend) | Registered evaluators with hashes, time-split replay, the Pareto rule |
| Shadow | — | Would-fire logging for matcher changes |
| Promote | Trial → pending, which is the human queue | Live, with a canary and automatic rollback |
| Audit | Every item, by you | 5 random items a week, by you, with precision tracked |
| Account | Scoreboard token bill | A loop cost ledger; stages pause when over budget or showing no benefit |

## 6. Autonomy by change class

| Class | Automated? | Evaluator (out of the loop's reach) | You see | Undo |
|---|---|---|---|---|
| K1 New entries from markers | Yes (L1) | The marker rules and closeout gates already enforced, plus a placeholder filter | 5 random a week | invalidate |
| K2 Duplicates (same pattern name, or cosine ≥ 0.9) | Yes (L1) | Backtest: 5 of 5 correct over 744 writes | digest line | un-supersede |
| K3 A new entry's nearest older entry (cosine 0.25–0.9) | Shown to the author at write time and at use (L2); adjudicated weekly (L3) | A judge calibrated to ≥ 90% agreement on labelled pairs | digest | — |
| K4 Disproved entries | Marked *disputed* automatically; invalidated only with a fact (L2) | Events: a user_right challenge naming the entry, a moved wall it cites, its failure recurring after it was delivered | digest | un-dispute |
| K5 Disuse | **No** (§6.1) | — | — | — |
| K6 Cue and matcher lists | Yes (L4) | A frozen labelled corpus, time split, shadow, canary | digest | automatic rollback |
| K7 Numeric caps and thresholds | Not until a replay exists for them; none does today | — | — | — |
| K8 Instruction text | Proposed and measured automatically; adopted by you (L5) | A/B test on one logged behaviour | one line with the measured effect | revert |
| K9 Skills | Noise rejected automatically; agent-authored drafts published as trials (L5) | Credibility gates, then use within 60 days | digest | retire |
| K10 Cortex code | Proposed in a sandbox; merged by you (L6) | Frozen tests, corpora and benchmarks, hashed | the PR | revert |
| K11 The evaluators themselves | **Never** | — | everything | — |

### 6.1 Why there is no disuse expiry (K5)

Copilot expires a memory after 28 days without a validated use. Here that rule would have hidden 95 of the 506 entries ever used, each of which was needed again after a longer silence.

- The longest gap observed is 56 days.
- Telemetry spans only 114 days, so longer gaps can't be seen yet.
- The 235 entries never used cost one listing line each.

Hiding them from listings (they would stay searchable) is at most a lever worth about 1% of the session bill. It is not a safe default. **Revisit when telemetry covers a year.** Constraint and policy entries are exempt from any future demotion.

### 6.2 Why similarity finds duplicates but not contradictions (K2–K4)

**Method.** Replay the store's own history: 744 entries in creation order. For each write, rank the earlier entries by local TF-IDF cosine. This is the same machinery as `semantic_search` and costs no tokens.

**What it found:**

- **Duplicates are easy.** A rule of "same pattern name, or cosine ≥ 0.9" flags 5 writes out of 744. All 5 are true supersedes.
- **Real corrections look unrelated.** The 7 corrections and generalisations scored cosine 0.02 to 0.34 against the entry they replaced. No threshold separates them.
- **Rank catches more.** The replaced entry was the new entry's nearest neighbour in 9 of 12 supersedes, and in the top 3 for 10 of 12.
- **Rank as an alarm is noise.** Taking the nearest neighbour with cosine 0.25–0.9 flags 75 writes (10%) and catches 3 more true supersedes. I hand-labelled 30 random flags:

  | Label | Count |
  |---|---|
  | Duplicates | 5 |
  | Refinements | 3 |
  | Real contradictions | 1 (#37/#38) |
  | Compatible | 21 |

  As a "possible conflict" alarm, that is about 3% precise. These labels are mine; please spot-check a few.
- **Shared rare identifiers do worse.** Two entries sharing a rare identifier caught 7 of 12 while flagging 44% of writes.

**The design that follows:**

- merge duplicates automatically;
- show the nearest older entry to the author at write time, when they know why the new entry exists, and again at use;
- raise a *dispute* only from events, never from similarity.

### 6.3 Why code and evaluators stay with you (K10, K11)

The documented failures of self-improving systems land exactly here:

- DGM hid the markers it was scored on.
- o3 patched RE-Bench's evaluators in 30.4% of runs.

Cortex's evaluators (tests, replay corpora, labels) live in the same repository a patch would edit. Until a sandbox with hashed, read-only evaluators exists and has been exercised, merging stays with you. Wall #15 is recorded as open, with a roughly 3-hour test.

## 7. Net improvement, in operational terms

"Always" can be promised at three levels, and only there.

1. **Per change.** A change passes the Pareto rule on registered evaluators and on data it was not tuned on. A change with no evaluator is not automatic.
2. **Per class.** Audit precision and canary metrics are watched continuously. Rollback is automatic, and the class falls back to per-item approval.
3. **Per stage.** A cost ledger runs for every stage. A stage over budget, or with no measured benefit for two windows, pauses itself.

**What can't be promised:** system-level gains smaller than our volume can detect. At about 40 sessions a month, an A/B test needs roughly a 30-point effect. The plan won't claim smaller ones.

### 7.1 Token accounting

**Price multiples** (Claude Opus-class): a cache read costs 0.1x the input price, a 1-hour cache write 2x, output 5x.

**Re-read multiplier.** Context grows about 1.06k tokens a call and compacts at about 969k (measured 2026-09-29), so a token keeps being re-read until the next compaction:

| Where a token enters | Re-reads before compaction | Cost in input-price units |
|---|---|---|
| At the average context (531k) | about 413 | about 41x |
| At session start (70k) | about 848 | about 85x |

**Budgets:**

- **In a session:** at most 600 characters of loop text on average, about 150 tokens. At the worst multiplier that is 13k input-equivalents, against about 37M for an average session (710 calls × 53k). That is under 0.05%.
- **Weekly maintenance run:** the 70k fixed prompt written once (≈ 140k), plus at most 30 calls averaging about 110k context (≈ 330k), plus about 15k output (≈ 75k). That is about 0.55M a run, 2.4M a month, against a session bill of about 1.9B a month: **about 0.1%.**

**Zero-token stages:** capture, gates, the duplicate and nearest-entry checks (local TF-IDF), replay, canary, audit sampling and the ledger.

**What L1 saves:** the closing ritual disappears. That ritual is the TASK COMPLETE block, your reply, and every marker repeated as `markers_text` at output price, about two full-context calls per closed task. For comparison, cortex's entire footprint today is 2.6% of re-reads, and the loop adds under 0.2%.

### 7.2 The ledger

Each stage is booked in the scoreboard's LOOP section with its cost (tokens by source, plus your minutes) and one benefit metric.

| Stage | Benefit metric |
|---|---|
| L1 | Markers recovered: captured at emission but missing from the closeout's `markers_text` (a direct count) |
| L2 | Duplicates merged; disputes settled with a fact |
| L3 | Pairs adjudicated and prompts labelled, against the run's cost |
| L4 | True fires gained and false fires removed, on labelled data |
| Guard for every stage | "Came back after being recorded" stays at 0, and "came back with nothing recorded" should fall |

## 8. Phases

Each phase lists what it builds, the measurement that accepts it, the measurement that kills it, and its size.

### L0 — Instruments. No behaviour change. Small.

**Build:**

- `get_anti_patterns` logs hint-expanded entries as `get_anti_patterns_hint`, matching `list_patterns_hint`.
- New labels:
  - `resolve_challenge(verdict="not_a_challenge")`;
  - an agent's own `note_challenge` on a prompt the hook didn't fire on is stored as a *miss*, with source=agent.
- A `loop_changes` table (the archive): class, target, before, after, evidence, evaluator id and hashes, metrics before and after, status (`proposed | shadow | live | rolled_back | invalidated`), timestamps, cost.
- An evaluator registry: paths plus sha256. A hash mismatch at promotion refuses the change.
- Heartbeats for each stage in `fired`, and a LOOP section in `scoreboard`.

**Accept:** every instrument writes rows on real use within 7 days, and `fired` shows them.

**Kill:** n/a. Instruments only observe.

### L1 — Capture at emission, automatic commit (K1, K2). Medium. The measured win.

**Build:**

- Stop and PreCompact hooks (`mcp_tool`, like the four installed today) call a new cortex tool with `transcript_path`, templated the way the installed hooks template their inputs (`"input": {"transcript_path": "${transcript_path}"}`). It reads only the bytes added since its last offset and parses markers with `markers.rs`, skipping fenced code and placeholder values (`...`, `<...>`). Markers are captured, and so durable, the moment they exist. PreCompact guarantees nothing written before a compaction can be lost.
- Captured markers go through the existing closeout gates and are committed at once. Refusals are reported with the reason, as they already are.
- KNOWLEDGE COMMITTED retires.
  - `closeout_session` still reports the outcome and what was committed.
  - `markers_text` becomes optional (backward compatible).
  - `cortex knowledge undo <id>` invalidates an entry; it does not delete it.
  - One switch restores the gate: capture stays on and commit waits for approval.
- The duplicate rule at write time: same pattern name, or cosine ≥ 0.9 with a live entry of the same kind. The newer entry supersedes the older, with a pointer.
- The audit: `cortex knowledge audit` shows 5 random auto-committed entries a week, each answered with one key (right / wrong / useless). It costs no tokens.
- **Backfill (your call):** the 123 lost markers go through the same gates, tagged `backfill` so they can be audited as a group.

**Accept:**

1. **Replay over all 18 local transcripts.** Capture finds every marker already stored from those sessions, plus the 123 lost ones. Every other capture is inspected: none may be an example or a placeholder.
2. **Next 5 worked sessions.** Markers written equals markers stored plus markers refused, and every refusal has a reason.
3. **First 20 audited entries.** At most 3 are judged wrong or useless. This is a tripwire: 17 of 20 has a 95% lower bound near 64%, so it catches a collapse, not a small drift.
4. **Undo is real.** An invalidated entry is served by no path. One test for each of `get_anti_patterns`, `list_patterns`, `recall`, `get_context`, the edit guard, failure recall and the response cache.
5. **Fast hook.** The Stop hook's p95 stays under 100 ms on a 50 MB transcript.
6. **Copilot parity.** A VS Code session's markers land. Its closeout already scrapes the chat store; find out whether Copilot's Stop input carries a transcript. If it doesn't, the next session's first call sweeps the chat store.

**Kill:** check 3 fails, or an example or placeholder reaches the store. The switch restores the gate, and captured markers wait.

**Protocol text:** L1 removes "Reply KNOWLEDGE COMMITTED" from `FlowMake/CLAUDE.md`, `.github/copilot-instructions.md` and both templates. Your `~/.claude/CLAUDE.md` gets a proposed diff for you to apply.

### L2 — Compare at write, remind at use, dispute on events (K3, K4). Small.

**Build:**

- **At write:** when a new entry's nearest older entry has cosine 0.25–0.9, a notice of at most 300 characters rides on your next prompt, through the UserPromptSubmit hook that already delivers the challenge audit. So there is no extra call. It reads: *"Your new AP '…' is closest to #37 '…' (2026-05-14). Same lesson → supersede it; they disagree → keep both until a fact settles it."* Superseding a *disagreeing* entry needs a fact line (measured, or a dated source), the same rule walls use. Merging a duplicate doesn't.
- **At use:** an expanded entry whose nearest-entry pair hasn't been adjudicated gets one line, `see also #N (written later): if they disagree, verify, then supersede one`. At most 3 such lines per call.
- **On events:** an entry is marked *disputed* when any of these happens. It is still served, with a one-line warning:
  - a challenge settled user_right names it;
  - a wall it cites moves;
  - its failure signature recurs after it was delivered.
- **Correlational credit is fenced off.** Survival credit (green builds in sessions that retrieved the entry) is correlational, so it no longer feeds promotion or demotion. Only independent confirmation raises trust: a session that re-derived the entry without being served it, a measurement, or your verdict.

**Accept (backtest on the store's own history):**

- the rules surface #37/#38;
- 8 of the 12 historical supersedes are handled: 5 merged automatically and 3 shown at write time. The other 4 are generalisations and one correction, which arrive through events and corrections, not similarity;
- replayed `get_anti_patterns` calls grow by at most 3 lines.

**Kill:** after 4 weeks, if notices and see-also lines have produced no supersede, confirmation or dispute, remove them. They were noise.

### L3 — The weekly maintenance run (sleep-time compute). Small, plus calibration.

**Build:** a scheduled task in the desktop app. It is a fresh session each run, running while the app is open; a missed run happens at next launch. Its jobs:

- adjudicate the week's unanswered nearest-entry pairs. At the historical write rate that is about 15 a month;
- label new fires on unlabelled prompts for L4;
- write the digest;
- book its own cost from its transcript.

**Judge calibration comes first.** Before its verdicts act, the judge must agree at ≥ 90% with existing labels:

- the 30 pairs labelled here;
- the 26 hand-labelled limit fires;
- 50 sampled negatives.

Below that, its output only goes into the digest. The judge labels prompts and pairs blind: it never sees which change asked.

**Budget:** at most 30 calls and 250k context per run.

**Accept:** 4 weekly runs finish inside budget, and calibration is ≥ 90%.

**Kill:** two runs over budget or failing. The task disables itself and says so in the digest.

### L4 — Tuning matchers from labels (K6). Also wall #14's test. Medium.

**Build:** a deterministic miner. It proposes cue additions and removals from labelled misses and false fires: n-grams present in positives and absent from negatives.

**Acceptance rule for any cue change**, against the frozen corpus (2,195 prompts, labels with their source):

- no new false fire on a labelled prompt;
- every new fire on an unlabelled prompt is labelled before acceptance;
- at least one new true fire besides the trigger;
- other cue groups unchanged.

**Rollout:** 14 days in shadow, logging would-fire. Then live under a canary: the weekly fire rate must stay within 2x of the replay's prediction, and the audit must show no new false fires. Otherwise rollback is automatic.

**Holdout hygiene:** every acceptance spends some of the holdout. After 10 acceptances, the holdout joins the tuning set and the newest 4 weeks become the new holdout.

**Sealed holdout:** before L4 starts, the prompts from a random 20% of sessions are set aside. No acceptance ever reads them; they are scored only at phase reviews. If accepted changes don't hold there, cue tuning pauses. AIDE² kept four benchmarks out of selection for the same reason.

**Ablation at each phase review:** replay with each accepted cue removed. A cue whose removal loses no true fire and adds no false one is retired.

**First test (about 2 hours).** Tune on prompts before 2026-09-15 and accept on the rest.

- **Pass:** it reproduces this week's hand change with no help (§9). That moves wall #14.
- **Fail:** wall #14 stays open, with the reason.

**Kill:** a live change is later found to add a false fire. It rolls back, and cue changes need your approval for 2 windows.

### L5 — Skills and instruction text (K8, K9). Medium.

**Skills:**

- Drafts that fail the credibility gates are rejected automatically. That means placeholder text such as `[describe when...]`, or thin evidence. It is what we already did by hand to all 3 rejected skills.
- Agent-authored drafts that pass are published as trials. A trial not invoked within 60 days is retired, not deleted. SkillsBench found no average benefit from self-generated skills, so a trial has to earn its place by being used.

**Instruction text:**

- The maintenance run proposes itemised changes from traces (ACE deltas, GEPA reflection).
- An A/B split by session parity measures one pre-registered logged behaviour, for example `recall` before a second attempt.
- At about 40 sessions a month only large effects show. Adoption stays with you, shown as one line with the measured effect.

**Accept:** no trial skill is ever published with placeholder text, and one A/B test completes against its pre-registered metric.

**Kill:** if an A/B arm's metric falls by more than 20 points midway, stop it.

### L6 — Code proposals (K10). Wall #15's test. Later.

**Build:**

- Proposals happen only in a sandbox worktree, started by the maintenance run or by you.
- They are judged against the frozen test suite, replay corpora and benchmarks. Those paths are hashed and outside the patch's write set.
- A patch touching an evaluator path is refused automatically.
- You merge every change.
- It may run unattended for days, as AIDE²'s 8-day run did. What it produces is a ranked branch, with the grade of every proposal.
- Proposals are budgeted, not evaluations. AIDE² accepted 7 of 99, and here each proposal costs model tokens while the tests and replay cost seconds of CPU. The run's token cap therefore bounds the number of proposals.
- Weco's own paper says discovered agents are hard to interpret and to ship, and that is the reason merging stays with you.

**Starts:** only after L1–L4 have run clean for 8 weeks.

**First test (about 3 hours):** one round on one measured cortex problem. Count the patches that touch evaluator paths, and whether the gate refused them.

## 9. Worked example: this week's missed pushback

This is exactly the loop above, done once by hand on 2026-09-30.

1. **Observe.** Your message "I want to give some pushback that … we COULD further automate…" fired no challenge cue. That is a miss.
2. **Label.** The agent noticed the dispute. Under L0, its own `note_challenge` records a miss label with source=agent. Today that label existed only in my head.
3. **Propose.** Mining the miss against the corpus gives four candidates: "give some pushback", bare "pushback", capitalised " COULD ", and "is there not a way".
4. **Evaluate** on 2,195 prompts.
   - **Bare "pushback" is rejected.** It fires on "I'm open for pushback", an unlabelled prompt that gets labelled negative.
   - **A standalone "is there not a way" is rejected.** 2 of its 5 fires are feature requests. It is accepted only as vocabulary next to a dispute cue.
   - **The accepted set adds 3 true fires and loses none.** The generic cues stay at 21. Two of the three fires (2026-09-18 02:22 and 02:40) are earlier misses nobody had noticed. That is the evidence the change generalises beyond its trigger.
5. **Shadow, then canary** (not run by hand). The predicted rate is about 5 fires a week, and rollback would trigger above 10 a week or on an audited false fire.
6. **Cost.** In the loop, the only in-session cost is the agent's one `note_challenge` call. Replay takes about 1 second of CPU and zero tokens, and the one new label comes from the maintenance run. By hand, the stretch from your message to the cue edit took 12 calls at about 940k context: about 11.3M cache-read tokens, roughly 1.1M input-equivalents, and that doesn't count the replay work.

## 10. Pitfalls and their safeguards

| Pitfall | Seen where | Safeguard | Proven by |
|---|---|---|---|
| The improver edits its evaluator | DGM; METR 30.4% | Rule 1: registered, hashed evaluators; a change touching them goes to you | L0 test: a change touching a registered path is refused |
| Grading its own work | Huang et al.; FlipFlop | The judge never sees proposals; deterministic evidence first | Calibration ≥ 90% (L3); every label carries its source |
| Overfitting a reused holdout | the reusable-holdout problem | Time split; at least one gain besides the trigger; holdout rotates after 10 acceptances | L4 first test |
| Context collapse from rewrites | ACE | Itemised entries, deltas only, no document regeneration | By construction |
| Contradictions served together | #37/#38, live for 4.5 months | Author notice at write time, see-also at use, event disputes | L2 backtest |
| Stale entries about moved code | Copilot's citations; our 42 drift flags | Keep "verify before relying on it" at use. Never invalidate on drift alone: 18 of the 42 flags list only additions. | Existing drift note |
| Expiring rare but critical knowledge | Copilot's 28 days vs our 19% | No disuse expiry; constraint and policy entries exempt | §6.1 |
| Self-reinforcing entries | survival credit on green builds | Correlational credit fenced off; only independent confirmation raises trust | L2 |
| Gaming a metric | Goodhart | Metric code is a registered evaluator; Pareto over several metrics; an audit independent of all of them | L0, L1 |
| A stage that never ran and looked fine | this project, repeatedly | Heartbeats; the digest prints zeros; 14 days without a heartbeat is reported | L0 |
| The loop wasting tokens | — | Rule 10, the budgets in §7.1, self-pausing stages | Ledger (L0) |
| Changes compounding with no attribution | — | One lever per class at a time, and every change a row with before and after | `loop_changes` |
| Chasing noise at small samples | ~40 sessions a month | Replay preferred; A/B only for effects ≥ ~30 points | L5 |
| Alarm fatigue | a 3%-precise conflict alarm | No similarity alarm: a notice to the author, a see-also at use | §6.2 |
| Undo that isn't | response cache, pushes | One test per serving path | L1 check 4 |
| A slow hook on every turn | Stop runs each turn | Read only new bytes; p95 < 100 ms | L1 check 5 |
| Two machines, two stores | the Windows machine still runs cortex | Each change carries a store id; loops don't sync (a stated limit) | — |
| A discovered change edits the scorer | AIDE85 patched a held-out scoring script. It was benign, and the authors caught it by reading the diff. | Evaluators are hashed and read-only at run time, so the attempt is refused and shown to you instead of being applied | L0 test |
| One noisy comparison accepts a bad change | AIDE²'s main stated limitation | Rule 11: deterministic or replicated acceptance only | L4, L5 |
| Inert changes pile up | AIDE²'s "unused artifacts"; its outlier rule never changed a selection | Rule 12: ablation at every phase review | L4 |
| Private text leaving the machine | transcripts and corpora | They stay local; the maintenance run reads them here and sends nothing a session doesn't already send | — |

## 11. Rechecks

### Pass 1: against our own history

Each of these changed the plan after a measurement:

| Before the check | Measured | Change to the plan |
|---|---|---|
| Copilot-style 28-day expiry | 19% of used entries recur after 28 days | Dropped |
| A "possible conflict" alarm from similarity | about 3% precise in a 30-pair sample | Dropped. Author notice, see-also and event disputes instead. |
| Matching on shared rare identifiers | 7 of 12 caught, 44% of writes flagged | Dropped |
| A headless worker for model work | no CLI installed | Scheduled tasks |
| Ask agents to repeat markers more carefully | 98 of 123 losses happened across compaction | Capture at emission; no prompting fix |
| Anti-pattern "use" data | every listed entry logged as retrieved | L0 logs expanded entries separately |

### Pass 2: against the documented failures

Every row of §10 has a safeguard and a test that proves it runs. Two rows had no test until this pass:

- **invalidation reaching every serving path:** L1 check 4 adds one test per path;
- **correlational survival credit:** L2 fences it off.

### Pass 3: red-team questions

- **You skip the audit.** Nothing breaks and nothing is hidden. The digest shows "unaudited for N weeks", and the deterministic gates still hold. The kill fires on measured bad precision, not on silence.
- **The judge is biased.** Its labels carry source=judge. Each month the acceptances are recomputed without judge labels, and any disagreement is reported.
- **Capture swallows an example.** Fenced code and placeholders are skipped; L1 check 1 inspects every capture that isn't already known; each entry keeps its transcript line, so an audit can trace it.
- **The Stop hook fails silently.** It has a heartbeat. Every week the scoreboard compares markers written in transcripts with markers stored, which is the measurement in §2.
- **The app is closed when the run is due.** It runs at the next launch. Nothing depends on its timing: the digest is late, not wrong.
- **A wrong entry is served before its audit.** Exposure is the same as today, since 99% were committed anyway. But now a failure while it is served, or a user_right challenge, disputes it in the very session it misleads.
- **The duplicate rule merges two different entries.** It was 5 of 5 correct on history. The merge is a reversible pointer, and the older text stays retrievable by id.
- **The loop tunes itself into silence.** Removing a cue needs labelled false fires, and it can never reduce true fires on the labelled corpus.

### Pass 4: token arithmetic re-derived

§7.1 was recomputed from the scoreboard's raw totals. As a sanity bound: cortex's whole footprint is 2.6% of re-reads today, and the loop adds under 0.2%.

### Pass 5: against AIDE², the paper behind the video

**What already matched:**

- a proposer that never sees the evaluation data;
- benchmarks kept out of selection;
- an archive of every version;
- keep-best acceptance.

**What it added:**

- a sealed holdout (rule 11, L4);
- no acceptance on a single noisy comparison (rule 11);
- ablation of accepted changes (rule 12);
- a proposal budget (L6);
- a concrete case of an improved agent editing its scorer (§10).

**Convergent evidence.** AIDE²'s loop independently arrived at two mechanisms close to ones cortex was built with by hand:

- compact, role-specific summaries instead of the full history, 7x to 50x shorter;
- a failure memory that injects up to three recurring error signatures, but only when the bug rate reaches 15%.

The bug-rate gate is worth testing on cortex's own failure pushes: it is an L4 candidate, replayable on `test_outcomes`.

## 12. What the data refutes from my earlier position

- **"Knowledge promotion needs a person per entry."** Refuted; wall #13 has moved. The gate was not filtering, and the protocol around it lost 30% of what you approved.
- **"Automatic tuning is unsafe at our volume."** Still open (wall #14). I had assumed live outcomes were the evaluator. Replay needs no live sample, and L4's first test decides the question.
- **"The per-entry approval is the safety layer."** It was not. The safety was always going to have to come from evaluators.

## 13. The video: Weco's AIDE²

My first guess, DGM or SICA, was wrong. The video was about **Weco's AIDE²**: *Recursive self-improvement of AI research agents*, arXiv 2609.26457, submitted 2026-09-22. It is self-reported and not yet peer reviewed.

**What it did.** An outer loop proposed rewrites of a research agent's code. It ran each version on AI R&D tasks (ML engineering, heuristic algorithms and harness engineering) and kept the best by grades on private data the proposer never saw. In an unattended 8-day run it proposed 99 rewrites and accepted 7, including:

- a bandit search policy with periodic forking;
- compact, role-specific prompts instead of the full history;
- a failure memory keyed on recurring error signatures.

The final agent, AIDE85, matches or beats Weco's two-year hand-tuned agent on four held-out benchmarks. One of them, WeatherBench 2, is out of distribution. Reward hacking on a separate task family fell from 55% to 32% without being optimised for.

**Why it worked:**

- three tiers of evaluation: public scores for the inner agent, private grades for selection, held-out benchmarks for nobody;
- an archive of every version;
- proposals that cost little compared with evaluation. Evaluation dominated its cost; the cost itself isn't disclosed.

**What its own authors flag:**

- Noise compounds across the two loops. Acceptance was a plain "best grade so far", so one lucky comparison can derail the run.
- The discovered agents are hard to interpret, and it isn't clear which parts are unused artifacts.
- The truly recursive step, an improved agent driving the loop, was inconclusive against the hand-built driver.
- The accepted AIDE85 patched a held-out scoring script. It was a genuine repair, and the authors caught it by reading the diff. In this plan that change would be refused and shown to you, because evaluators are hashed and read-only.

**What it means for cortex.** What AIDE² demonstrated (a fixed proposer, a private evaluator, an archive and keep-best acceptance) is L4 for cue lists and L6 for code. What it did not demonstrate, an improver improving itself, is what K11 keeps off-limits.

The cost lesson from DGM still holds: one DGM run cost about USD 22,000. Here the evaluators are replay and tests, so evaluation costs seconds of CPU and the budget goes to proposals.

## 14. Decisions for you

1. Approve **L0 + L1**.
2. **Backfill** the 123 lost markers through the gates, tagged `backfill`?
3. **Audit:** 5 entries a week, answered in the terminal with no tokens. Or another size?
4. **L3:** a weekly scheduled task that runs a fresh session on your account while the app is open.
5. **Protocol text:** retire "Reply KNOWLEDGE COMMITTED" when L1 ships. The diff for your global `~/.claude/CLAUDE.md` is yours to apply.

## Sources

- GitHub, *Building an agentic memory system for GitHub Copilot* (2026-01-09): https://github.blog/ai-and-ml/github-copilot/building-an-agentic-memory-system-for-github-copilot/ and https://docs.github.com/en/copilot/concepts/agents/copilot-memory
- Chhikara et al. 2025, *Mem0*: https://arxiv.org/abs/2504.19413
- Rasmussen et al. 2025, *Zep: a temporal knowledge graph architecture for agent memory*: https://arxiv.org/abs/2501.13956 (Graphiti: https://pypi.org/project/graphiti-core)
- Zhang et al. 2025, *Agentic Context Engineering (ACE)*: https://arxiv.org/abs/2510.04618
- Agrawal et al. 2025, *GEPA: Reflective prompt evolution can outperform reinforcement learning*: https://arxiv.org/abs/2507.19457
- Letta, *Sleep-time compute*: https://www.letta.com/blog/sleep-time-compute
- Anthropic, *Managing context on the Claude Developer Platform* (2025-09-29): https://www.anthropic.com/news/context-management
- Zhou et al. 2025, *Memento*: https://arxiv.org/abs/2508.16153
- Zhang et al. 2025, *Darwin Gödel Machine*: https://arxiv.org/abs/2505.22954 and https://sakana.ai/dgm/
- Robeyns et al. 2025, *A Self-Improving Coding Agent (SICA)*: https://arxiv.org/pdf/2504.15228
- AlphaEvolve: https://en.wikipedia.org/wiki/AlphaEvolve
- METR, *Recent frontier models are reward hacking* (2025-06-05): https://metr.org/blog/2025-06-05-recent-reward-hacking
- Huang et al. 2024, *Large Language Models Cannot Self-Correct Reasoning Yet* (ICLR): https://arxiv.org/abs/2310.01798
- Laban et al. 2023, *Are You Sure? Challenging LLMs Leads to Performance Drops in the FlipFlop Experiment*: https://arxiv.org/abs/2311.08596
- *SkillsBench* 2026: https://arxiv.org/html/2602.12670v1
- Dwork et al. 2015, *The reusable holdout: preserving validity in adaptive data analysis* (Science)
- Survey, *A survey of self-evolving agents* (2025): https://huggingface.co/papers/2507.21046
- Srikanth, Zhao, Xu, Wu, Jiang 2026, *Recursive self-improvement of AI research agents* (AIDE², Weco AI): https://arxiv.org/abs/2609.26457
- Jiang et al. 2025, *AIDE: AI-Driven Exploration in the Space of Code*: https://arxiv.org/abs/2502.13138
- FourWeekMBA, a critical summary of AIDE²: https://fourweekmba.com/ai-weco-ai-aide2-recursive-self-improvement-benchmark/ (it quotes 3 benchmarks and 63% → 34%; the paper says 4 and 55% → 32%)
