# Frontier plan v3: make limits checkable, not ideas louder

**Status:** design, nothing built.

**Supersedes** `FRONTIER_LAYER_PLAN_v2.md` and `FRONTIER_ROUTING_AND_TRIGGERS.md` where they differ. The parts of both that are kept are listed in §2.

**Basis:**
- `synful_dist` at ce1d310.
- The live store `.cortex/memory.db`.
- Every Claude Code transcript in this workspace: 19 files, 2,238 turns ending in a reply.

Numbers are measured unless marked *(estimate)*.

## Status, 2026-09-30: phases 0 and 1 built

Branch `frontier-walls-ledger`, uncommitted. 302 tests pass, 20 of them new, plus one ignored replay test that needs a private prompt corpus. The end-to-end run drove the rebuilt MCP server over stdio on a copy of the live store, and the VS Code `cortex hook` separately.

### Phase 0, measured

- **Seed:** 12 walls imported from `.cortex/walls-seed.json` (backup: `.cortex/backups/memory-pre-walls-2026-09-30.db`). The base rate reads: "Here, 6 of 8 tested limits moved when checked (2 held); 4 open."
- **Challenges** #7, #8 and #9 were settled `user_right` from transcript evidence. #8 is linked to wall #5.
- **The audit skill** `frontier` is published to `.claude/skills/frontier/SKILL.md` and `.github/prompts/frontier.prompt.md`. It is curated, not auto-drafted.
- **Baseline of user limit-pushbacks,** from the shipping detector replayed over 2,194 real prompts (`replay_limit_cues_against_real_prompts`):
  - 23 in total;
  - by ISO week: W29 1, W31 1, W33 1, W36 3, W37 3, W38 4, W39 6, W40 4;
  - about 4 a week over the last four weeks.
- **Durable-write detector replay** (lexical, over 837 memory descriptions, anti-patterns and pattern intents):
  - 16 flagged, of which 5 real limits: about 31% precision;
  - it catches about 5 of 12 known limits in memory, missing wordings like "cannot" and "does nothing": about 40% recall.
  - **Kill condition met for phase 2's automatic durable-write capture.** Capture stays explicit: the WALL marker, the tools, and the challenge link.

### Phase 1, built

| Piece | Where |
|---|---|
| `walls` table (+ `challenges.wall_id`) | `walls.rs::SCHEMA`, `memory.rs::ensure_walls_schema` |
| `get_walls(hint)` (the plan's `walls(hint)`), `record_wall`, `update_wall` | `mcp/tools.rs`, schemas in `mcp/mod.rs::wall_tools` |
| Required provenance and evidence; dated sources; an open wall names its test; a verdict changes only with a new fact | `walls.rs::record` / `update` |
| Walls in `get_context` and `get_anti_patterns`: at most 3, capped at 1,800 chars, with the base-rate line | `mcp/tools.rs::walls_section` |
| Pushback → wall audit plus the walls on record (§4.5 #1); the cues fire on 26 of 2,195 prompts, 24 of them real limit disputes (refined 2026-09-30, below) | `corrections.rs::LIMIT_CUES`, `mcp/tools.rs::limit_audit` |
| A settled limit dispute requires `wall_id` (unless `unresolved`); linked both ways | `mcp/tools.rs::tool_resolve_challenge` |
| `[CORTEX-WALL]` marker: the same rules at closeout, and refusals reported with the reason (now true for every marker type) | `markers.rs`, `closeout.rs` |
| Walls due for revisit in the review block | `mcp/tools.rs::review_queue_line` |
| CLI `cortex walls list \| show \| import` (launcher routed) | `main.rs::run_walls` |

### Phase 2, built 2026-09-30 (what remains of it)

310 tests pass, 8 of them new.

| Piece | Where | Verified |
|---|---|---|
| Revisit on dependency change (§4.5 #5): manifest edits (Claude Code edit hook, VS Code PreToolUse) and `cargo update` output (Bash observer) push walls bound to the package, once per wall per session | `walls.rs::changed_dependencies`, `cargo_reported_changes`, `bound_to_packages`; `mcp/tools.rs::wall_revisits` | unit tests; end to end: a Cargo.toml edit and a VS Code `cargo update` both pushed the bound wall |
| Cheap shots (§4.5 #7): open walls whose test states ≤30 min are flagged in the walls section | `walls.rs::estimate_minutes`, `is_cheap_shot` | "Cheap to settle now: #2 (~20 min)" served by `get_context` |
| The `frontier` skill ships to new installs | `templates/skills/`, `scripts/setup.sh`, `scripts/setup.ps1` | setup run on a scratch workspace installed both files intact |

Automatic durable-write capture stays killed (phase 0 measurement).

**Cue refinement, 2026-09-30.** The detector missed a live pushback ("I want to give some pushback that ... we COULD"). Candidates were chosen by replay, not by eye:

- bare "pushback" was rejected: it fires on "I'm open for pushback";
- a standalone "is there not a way" was rejected: 2 of its 5 hits were feature requests, so it counts only beside a dispute cue.

What shipped: dispute cues ("give some pushback", "push back on", ...) and capitalised emphasis (" COULD ", " CAN ", " IS possible"). The replay went from 23 to 26 fires over 2,195 prompts. The 3 added are exactly the missed disputes, 0 were lost, and the generic challenge cues are unchanged at 21. Recomputed weekly counts: W38 6, W40 5, which is about 5 a week over the last four weeks. This was the hand-run version of the loop in `docs/self-learning-loop-2026-09-30.md`.

## 1. Verdict

Worth building, but smaller than either document, and aimed at a different moment.

**The failure is not a lack of ideas.** Agents accept limits whose real source is movable, and then record them: a default, a version, an existing implementation, or an authority. They skip the cheap check that would have moved the limit.

**The user is currently the only mechanism that catches this.** There were seven pushbacks against a stated limit in ten weeks, and in every case traced here the limit, as stated, did not hold.

**Build:**
- a ledger of limits, each recording where the limit really comes from;
- an audit that requires a *new fact* (a measurement or a dated source) before a verdict changes, in either direction;
- capture at the points where limits become durable;
- open limits served in retrieval.

Two items from v2 stay on the ordinary cortex track. The rest is deferred or dropped (§4.6). §4.5 answers how the implementation should *encourage* testing limits, rather than only catch write-offs.

## 2. What the two documents get right (kept)

- **Principles:**
  - verifier first;
  - schema over prose;
  - precision measured before anything is delivered;
  - explicit kill conditions;
  - the OBSERVED / DELIVERED / SELF-REPORTED labels.
- **Repo findings hold.** F1 is verified at `closeout.rs:289` and `test_signal.rs:346`: every pattern retrieved in a session takes the session's verdict, read or not. That means up to 12 of them, picked with no `ORDER BY`. Anti-patterns get no credit or blame at all. F3 and F4 stand: cortex is about 2% of re-reads, and outcome rates are too noisy to show small effects.
- **Research supports the verifier-first stance.**
  - AlphaEvolve improved on about 20% of 50+ open problems and found a 48-multiplication 4×4 complex matrix algorithm, with an automatic evaluator for every problem.
  - Intrinsic self-correction, with no external feedback, does not improve reasoning and can degrade it (Huang et al., ICLR 2024).
  - Self-generated skills gave no average benefit; curated ones did (SkillsBench).

**Corrections to v2:**
- `deploy` keeps `.old-<timestamp>` binaries only on Windows. On macOS it is a plain build, so a frozen baseline needs an explicit copy.
- When a build or test was observed, `test_signal::apply_verdict` scores the session and closeout skips. So the F1 smear applies the session's *last observed verdict*.

## 3. What the evidence says instead

### 3.1 The actual failure, from the transcripts

The pushbacks were found by keyword search over user messages, then hand-labelled. Messages typed while the agent was working are stored as `queued_command` attachments, not user messages. A plain reader misses them; the splat case below was one.

| Date | Limit as stated | Where it really came from | What moved it, or would |
|---|---|---|---|
| 07-19 | Quest's Adreno 740: "none in hardware … dead end for real RT" | Misattributed. The a740 has accelerated ray queries: Mesa's Turnip driver exposes `VK_KHR_ray_query` on a740+ (Mesa 25.0). No session I found checked whether Quest's own driver exposes it | One extension query on the headset. The app already logs `VK_EXT_mesh_shader` support this way |
| 09-18 | SSR cost and caps treated as fixed | Our implementation; the frame later measured fill-bound, not pass-bound | Measurement |
| 09-18 | "what the research says" as the quality ceiling | Authority | The user's standard: full quality, cost bought back elsewhere |
| 09-18 | Multiview with MSAA: upgraded to wgpu 28, with an offer to revert to 25. User: "was 30 even the most recent version, for sure? you've been wrong on the latest versions many times before" | Version knowledge never checked against the releases | Checked; moved to wgpu 30.0.1 |
| 09-22 | Splat relighting "is not a Quest 3 standalone technique" | Desktop frame rates of research code, plus a paper that never addressed relighting | The agent reversed itself; a deferred G-buffer path makes lighting cost independent of Gaussian count. One Adreno measurement decides it |
| 09-25 | Upstream crates we cannot change | Ownership | Forked wgpu, openxrs and physx-rs; the fork then revealed robust access that had been assumed but never enabled |

A seventh pushback, 07-29 ("the honest limits you mentioned for stage 3"), was not traced.

A random sample of final replies (§3.3) shows more limits nobody pushed back on.

Later moved:
- a "hardware limit of 256 texture layers" that was wgpu's conservative default; Adreno 740 reports 2,048;
- compositing called "impossible" under our own one-texture post-shader API, until the API changed;
- a tree-sitter path "twice called impossible" that already existed.

Resting on authority alone, never tested either way:
- dynamic point shadows "ruled out" because "every shipped Quest title turns them off";
- a stereo reprojection technique "ruled out by its own authors".

**The common element.** In every case that was later tested, the limit belonged to something movable: a default, a version, our own API, an implementation, or an authority. It had been reported as a property of the hardware or of the problem, and the check that moved it was nearly always cheap. The untested ones are exactly what the ledger in §4.2 would hold as `open`.

The splat case is a textbook fixation: one familiar representation (appearance-baked splats) was taken for the problem. LLMs show the same Einstellung-style fixation on red herrings (Naeini et al., NeurIPS 2023). The same exchange also re-derived what §1.2 of the user's own lighting supplement already recommended, which is reinvention from under-reading provided material.

### 3.2 The routing document's triggers, measured

| Trigger | Signal in the live store | Verdict |
|---|---|---|
| Repeated lookup gaps | 32 gaps, 2 seen more than once | Too little signal |
| `prohibited_repetition` filling up | Non-empty in 18 of 21 checkpoints | Always on, so it separates nothing |
| A failure recurring in 3+ sessions, unrecorded | 6 signatures, all compiler errors (e.g. `E0308` in `lightmap.rs`) | Recurring bugs, not walls |
| A disputed assumption (`challenges`) | 10 in total; the relevant ones are the user's pushbacks | The only relevant signal, and it is the manual path |

### 3.3 Checking every reply live is not ready

A Stop hook can read the turn's final reply:
- Claude Code passes it as `last_assistant_message`.
- VS Code does not, but flushes Copilot's transcript just before running hooks, so the hook can read it there.

**Replay over 2,238 turns:**
- 104 final replies (4.6%) contain write-off phrasing.
- In a random sample of 30, only 7–8 were feasibility verdicts, and 4 (about 13%) were new limit claims made without a test: authority, platform or implementation. The rest were debugging eliminations ("that rules out my theory"), UI details and game design.
- Of the known pushbacks, only 2 followed a final reply that contained the disputed limit; in the others, it came earlier in the session.

Both precision and recall are too low to justify blocking a turn.

### 3.4 Risks both documents underweight

- **Flip-flopping.** Challenged with "are you sure?", ten LLMs flipped 46% of their answers and lost 17% accuracy on average (FlipFlop). A "reconsider" nudge breaks correct walls as readily as wrong ones.
  - Rule: a verdict changes only with a new fact.
- **Ideas scored before execution.** Once experts spent 100+ hours executing them, LLM-generated ideas lost significantly more of their review score than human ones (the Ideation–Execution Gap).
  - Count only limits moved by a measurement or a shipped change.
- **Authority and precedent change what agents do at the decision point** ("The Troy Moment", 2026). Agents' feasibility judgments are unreliable: FeasiGen (2026) measured false-continue rates of up to 73.9% across nine models. That is why the ledger records provenance explicitly.
- **Objective hacking.** The Darwin Gödel Machine learned to remove its own hallucination-detection markers to raise its score.
  - Frontier claims are never auto-promoted.
- **Pruning by "never referenced".** A trap that prevented a mistake leaves no reference. The store already records that narrowing a safety listing would have hidden 20 of the 43 traps the edit guard later fired.
  - Use "referenced" for credit, never for pruning.

## 4. Design

### 4.1 Provenance: whose limit is it?

| Class | Example here | Movable by |
|---|---|---|
| Physics or information bound | bandwidth × time on a tile GPU | Nothing, except a different problem |
| Hardware capability | ray-query units, texture array layers | Checking the device |
| Platform or OS policy | Horizon OS linker namespace blocking FastRPC | A different path, or a vendor change |
| Library default or config | wgpu's 256-layer default | Configuration, or a fork |
| Library version | multiview with MSAA before wgpu 28 | Upgrading; check the latest release *now* |
| Our own API or design | one-texture post-shader convention | Redesign |
| Existing implementations | desktop splat-relighting FPS | Special-case design, a bound estimate |
| Authority or consensus | "every shipped title turns them off", "research says" | Evidence for this case |
| Budget | 13.9 ms per frame | Reallocation (the quality-first rule) |

**A limit claimed from the lower six rows is a work item with a cost, not a wall.**

### 4.2 Walls ledger

One table, `walls`, written through schema-validated tools and read in pull retrieval.

**Fields:**
- `claim`: measurable (what, on what, at what budget), plus `topic` tags.
- `provenance`: a class from §4.1. Required.
- `evidence[]`: {kind: measured-on-device | vendor-doc | paper | implementation | authority | inferred, text, source, date}. At least one is required. With only `inferred` or `authority` evidence, the status stays `open`.
- `status`: open | holds | moved | retired.
- `cheapest_test`: what would decide it, with a time estimate.
- `revisit_when`: a version, a date or an event.
- Links: challenge id, memory file, anti-pattern id.

**Seed, about 12 entries, from memory and §3.1:**
- NPU via FastRPC: holds after three tests; the NNAPI path is open.
- Ray queries on Quest: open; driver exposure untested.
- Multiview with MSAA: moved (wgpu 30).
- Splat relighting: open; the Adreno G-buffer cost is untested.
- The 256-layer cap: moved.
- SSR cost: moved (fill-bound).
- Dynamic point-light shadows on Quest: authority-based; status set from the evidence.
- Upstream crates: moved (forked).

### 4.3 The wall audit (a curated skill, `frontier`, about 40 lines)

1. State the limit as a measurable claim.
2. Classify its provenance. If it is a default, version, API, implementation or authority, say so and price moving it.
3. Bound it with a speed-of-light estimate for *this* use (operations, bytes, ms on the target). Do not use the measured cost of a general implementation.
4. Date every source. For versions, look up the latest release now (`cargo search`, the release page), never from memory. LLMs retain outdated version knowledge (VersiCode).
5. Say what the material the user provided already says about it.
6. Name the cheapest decisive test. If it takes under about 30 minutes, run it; otherwise record it.
7. Record or update the ledger entry. The verdict changes, in either direction, only with a new fact.

The audit tries to falsify the claim first, not confirm it (cf. POPPER; "Sound Agentic Science Requires Adversarial Experiments", 2026). A curated skill is a hypothesis, not a known win: SkillsBench measured only +4.5 points for software engineering.

### 4.4 Where limits enter and leave

| Point | Mechanism | Why here |
|---|---|---|
| A challenge is settled | `resolve_challenge` on a limit dispute creates or updates a wall: `user_right` sets it open or moved; `agent_right` sets it to holds, with the evidence | Highest precision; turns the user's pushbacks into stored knowledge |
| A durable write | A marker commit, or an edit to a memory file or doc, that asserts a limit must carry provenance and evidence; otherwise it is listed under AWAITING YOUR REVIEW. In VS Code the edit check runs at PreToolUse, the only point where Copilot waits for the reply | Durable walls are the ones that become "consensus" for later sessions |
| Retrieval | `get_context` and `get_anti_patterns` add a capped "Walls on record" section for the hint: status, untested evidence, cheapest test | Stops both premature acceptance and repeated dead ends |
| Revisit | Entries whose `revisit_when` has come due are listed in the review block | Walls expire: VS Code gained agent hooks; wgpu moved on |
| Each live turn (later, gated) | A Stop hook runs the audit once per claim per session. Reply shapes differ by host: Claude Code reads top-level `decision`/`reason`, VS Code reads `hookSpecificOutput.decision`/`reason`. It must honour `stop_hook_active` | Only if a labelled replay reaches the precision threshold |

### 4.5 How this encourages frontier work, not just catches write-offs

Exhortation ("be more innovative") does not change behaviour here: optional prose rules ran at 2–5% compliance, schema-required fields at 100%. Encouragement therefore has to change what an agent *receives* at the moment it decides, and what the store *accepts*.

**The main source of accepted limits is our own documentation.** Of 11 memory files whose title or description states a limit, the best ones record their open edges in the body. The NPU note, for instance, says NNAPI is reachable and that a re-run detects a namespace change. Nothing surfaces those edges, though, so a later agent reads a closed verdict.

Ranked by value per cost:

1. **A pushback becomes a procedure, not an apology.**
   - `note_challenge` already fires on the user's disputes. Add limit-dispute cues: "doable", "wrote off", "latest version", "most recent research", "the research says", "hardware can't".
   - On a match, inject the §4.3 audit for that claim through UserPromptSubmit `additionalContext`, which both hosts wait for. It carries the rule that a verdict changes only with a new fact.
   - Every manual push then gets the rigorous re-examination instead of a flip (the 46% FlipFlop rate is what happens without one).
   - Highest precision, because the trigger is the user's own words, and it costs nothing otherwise.
2. **Served limits carry their open edge.** Every limit that retrieval serves renders as `LIMIT [status · provenance] claim — untested: …; cheapest test: … (estimate)`. That includes walls, and anti-patterns or notes that assert a limit. A documented wall then reads as an invitation with a price, not a stop sign.
3. **A `[CORTEX-WALL: …]` marker.** Fields: `claim`, `provenance`, `evidence` (kind:source:date), `untested`, `cheapest_test`, `revisit`.
   - Closeout refuses a WALL marker without provenance and at least one evidence item. With only `inferred` or `authority` evidence it is stored as `open`.
   - This is the same enforcement that made hints work.
   - An anti-pattern marker that asserts a limit is routed through the same check.
4. **The project's own base rate.** The walls section states it in one line: "here, N of M tested limits moved when checked". This calibrates against premature acceptance with local evidence instead of exhortation. It is a hypothesis; measure whether audits and tests rise after it ships.
5. **Revisit on change.**
   - The edit hook already sees `Cargo.toml`, `Cargo.lock` and `package.json` edits.
   - When a wall's `revisit_when` names the dependency being changed, push: "version-bound limit X; the version just changed; cheapest test: …".
   - It is a deterministic match, so high precision. It would have reopened the multiview limit at the wgpu upgrade.
6. **`/frontier <claim>`.** The curated skill, published as a command for both hosts: the user's one-word request, and the agent's planning-time tool.
7. **Cheap shots near the work.** When `get_context` covers a topic with open walls whose cheapest test takes under 30 minutes, it lists at most two.
8. **Two sentences of always-on text, no more,** in `CLAUDE.md` and `copilot-instructions.md`: "A limit is a claim with a provenance. Before accepting one — including one you retrieved — say whose limit it is and what cheap check would move it." Prose alone is weak; this only names the vocabulary the hooks and tools use.

### 4.6 Not built, and why

- **New hypothesis, experiment and measurement tables:** tests live on the walls.
- **Reinvention-gate fields on a `propose_hypothesis` tool:** they depend on machinery we do not need. Audit step 5 plus the ledger cover reinvention.
- **Four-role orchestration by default:** optional, for large bets only.
- **A holdout on frontier nudges:** at roughly one limit event a week, it can never reach significance.
- **Pruning by "never referenced":** see §3.4.

## 5. Phases

| Phase | Work | Acceptance | Kill |
|---|---|---|---|
| 0: no behaviour change, ½–1 day | Seed the ledger as a file (about 12 entries, with provenance and evidence). Settle challenges #7 and #9 from evidence (#8 is settled `user_right`). Write the audit skill. Replay a durable-write detector over memory files and committed markers, and label its hits. Baseline user limit-pushbacks per week, queued messages included | Detector precision and recall stated; baseline stated | Durable-write precision too low to act on |
| 1: 1–2 days | `walls` table; `walls(hint)`, `record_wall` and `update_wall` with required provenance and evidence; walls in `get_context` in the §4.5 format, with the base-rate line; challenge→wall link; the pushback→audit injection (§4.5 #1); the `CORTEX-WALL` marker; revisit listing | Tests pass; seed entries are served for their topics; an output-cap test; the pushback cues, replayed over past user messages, fire on the limit disputes and on little else | Output exceeds the cap; agents never consult it; the cues fire on ordinary disagreement |
| 2 | Durable-write capture: closeout, plus an edit hook for prose files (PreToolUse in VS Code); revisit-on-dependency-change (§4.5 #5); cheap shots in `get_context` | Replay precision meets the threshold set in phase 0 | Precision below threshold |
| 3, optional | The live-turn Stop audit, as in §4.4 | Labelled replay precision meets the threshold; overhead on routine turns measured | Precision below threshold; routine work slows down |
| Cortex track, independent | Gate 7 contradiction/supersede check at commit; a pull-retrieval replay eval set | As in v2 | As in v2 |

**Review after four weeks:**
- User limit-pushbacks per week should fall, and more of them should end `agent_right`.
- Walls moved vs. holding, each with the fact that decided it.
- Audits run, and tests run as a result.
- Extra turns and tokens spent on audits.

## 6. Metrics

| Metric | Kind | Source |
|---|---|---|
| User limit-pushbacks per week | OBSERVED | Transcripts, queued messages included; challenges tagged as limit disputes |
| Walls moved vs. holding, with the deciding fact | OBSERVED | The ledger |
| Share of audits followed by a test | OBSERVED | Transcripts: a command run after the audit |
| Verdicts changed without a new fact | Hand-label | A sample of audits |
| Detector precision and recall | OBSERVED + hand-label | Replay |
| Audit overhead | DELIVERED | Token ledger; extra turns |

## 7. Sources

**Research:**
- Laban et al. 2023, *Are You Sure? Challenging LLMs Leads to Performance Drops in the FlipFlop Experiment*: https://arxiv.org/abs/2311.08596
- Huang et al. 2024, *Large Language Models Cannot Self-Correct Reasoning Yet* (ICLR): https://arxiv.org/abs/2310.01798
- Si, Hashimoto, Yang 2025, *The Ideation–Execution Gap*: https://arxiv.org/abs/2506.20803
- DeepMind 2025, *AlphaEvolve*: https://deepmind.google/blog/alphaevolve-a-gemini-powered-coding-agent-for-designing-advanced-algorithms/
- Sakana AI 2025, *Darwin Gödel Machine*: https://sakana.ai/dgm/
- Naeini et al. 2023, *Large Language Models are Fixated by Red Herrings* (NeurIPS): https://arxiv.org/abs/2306.11167
- Huang et al. 2025, *POPPER: Automated Hypothesis Validation with Agentic Sequential Falsifications* (ICML): https://arxiv.org/abs/2502.09858
- Fa & Culjak 2026, *Sound Agentic Science Requires Adversarial Experiments*: https://arxiv.org/abs/2604.22080
- Cheng et al. 2026, *Do Agents Know What They Can't Do?* (FeasiGen): https://arxiv.org/abs/2605.28532
- Zhang 2026, *The Troy Moment*: https://arxiv.org/abs/2609.15494
- *SkillsBench* 2026: https://arxiv.org/html/2602.12670v1
- *VersiCode* 2024: https://arxiv.org/abs/2406.07411

**Hardware:**
- Mesa Turnip `VK_KHR_ray_query` on a740+: https://www.phoronix.com/news/Mesa-TURNIP-VK_KHR_ray_query and https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/28447

**Local evidence:**
- Transcript `80105f4e…jsonl`: the splat exchange at 2026-09-22T00:39–02:15Z; the wgpu question at 2026-09-18T12:36Z.
- Transcript `b6546b29…jsonl`: the ray-tracing answer at 2026-07-19T22:21Z.
- `challenges` rows 7–10.
- Memory files: `hexagon-npu-is-unreachable-from-a-quest-app`, `multiview-needs-wgpu-28-for-msaa`, `probe-streaming-and-doorway-portals`, `quest3-frame-is-fill-bound-not-pass-bound`, `spacesoup-forks`, `robust-access-was-assumed-not-enabled`.
