# Wall 0, measured: earlier compaction

**Status:** assessment of `WALL0_CONTEXT_LENGTH.md`, with its first action (M1), plus M3 and a trigger simulation, done from data. Nothing has been changed in the host or in cortex.

**Supersedes:** that document's cost model (§4) and prior-attempt table (§5) where they differ.

**Data:**
- Every Claude Code transcript in this workspace: 18 sessions, 34,460 deduplicated main-thread API calls, 2026-07-12 to 2026-09-30.
- Each call's `usage` (input, cache read, cache write split into 5-minute and 1-hour, output) and each `compact_boundary` record's `compactMetadata`.
- Scripts are in the session scratchpad and can be re-run.

## 1. Verdict

The document's premise holds and its arithmetic question is settled. Growth per call is small (median 1.1k tokens), so compaction cycles are about 460 calls long, far beyond its break-even of about 7. Lowering the trigger saves a large share of the bill even after compaction and recovery costs are included (§3).

What remains undecided is not the arithmetic but three other things:
- silent loss of working state;
- two minutes of stall per compaction;
- whether the subscription's usage limits weigh cache reads the way API prices do.

Build the checkpoint first, then lower the trigger in stages.

## 2. Measured

| Quantity | Value |
|---|---|
| Cache reads / 1-hour cache writes / 5-minute cache writes / output / uncached input | 17.01B / 199.7M / 5.5M / 32.1M / 0.09M tokens |
| Share of cost-equivalent units (API price ratios: read 1, 1-hour write 20, output 50) | reads **75.0%**, writes 17.9%, output 7.1% |
| Context per call | mean 500k, median 488k |
| Compactions | 61, all automatic, at a median of **969k** tokens (about 97% of the 1M window) |
| Compaction duration | median **126 s**, p90 180 s |
| First call after a compaction | median 74k context: 39k still cached, 39k written anew |
| Host summary | about 5.3k tokens (p90 7.4k), plus a verbatim `preservedSegment` of recent messages |
| Growth per call | median 1.06k, mean 1.64k, p90 3.4k, p99 12k |
| Calls between compactions | median 462 |
| Recovery after a compaction (M3) | growth over the next 30 calls is 2.58k/call vs 1.59k elsewhere: **about 30k extra tokens per compaction** |
| Context drops over 20% with no compaction | 2 in 34k calls |

## 3. Trigger simulation

The simulation replays each session's actual per-call growth under a lower trigger:
- each compaction costs a read of the window, 5.3k of summary output, and 39k of rewritten context at the 1-hour write rate;
- each compaction adds the measured 30k recovery, spread over the next 30 calls.

It reproduces history closely: 17.70B cache reads simulated vs 17.01B measured, and 58 compactions vs 61.

| Trigger | Compactions | Saving, share of all units | Extra stall over the period |
|---|---|---|---|
| 969k (today) | 58 | none | none |
| 800k | 73 | 11.8% | +0.5 h |
| 600k | 104 | 25.5% | +1.6 h |
| 450k | 154 | 36.2% | +3.4 h |
| 350k | 219 | 43.3% | +5.6 h |
| 250k | 370 | 49.9% | +10.9 h |

The saving holds even at the p99 growth rate: the baseline also pays for its compactions, and they are larger ones. The document's §4 compared one compaction's cost with one call's saving; the right comparison is units per call under each trigger.

## 4. Corrections to the document

- **Its cost table is 10× low in every row.** For example, 450k × 1 = 450k, not 45k. The break-even ratio survives only because every row carries the same error.
- **Cache writes here are 1-hour writes (20× a read), not about 12×.**
- **After a compaction only about 39k is rewritten, not 80k:** the stable prefix stays cached.
- **"The host's summary is the only thing carried across": no.** The host also keeps a verbatim segment of recent messages.
- **"Observation masking is already done by the host (microcompact)": not in these sessions.** Context never drops without a compaction boundary (2 cases in 34k calls). Our own upgrade plan made the same claim from a search result; it is corrected there.
- **The trigger sits at about 97% of the 1M window, not about 83%.**

**Host controls:**
- `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` can only lower the threshold.
- PreCompact can block a compaction but cannot inject context.
- SessionStart with `source: compact` can inject context, as plain stdout or `additionalContext`.
- PostCompact exists, but only observes.

## 5. What still decides it

- **Silent loss.** The 30k recovery is what agents notice they lost. What they lose without noticing is unmeasured. Test it offline with M5 before moving the trigger.
- **Latency.** At the observed pace, a 450k trigger adds about 96 compactions over 11 weeks, about 2 minutes each: roughly 18 minutes a week.
- **Weights (M2).** The unit shares use API price ratios. If the subscription's limits weigh cache reads less, the saving shrinks in proportion. Hitting usage limits in long sessions is consistent with reads mattering, but that is not proof.
- **Quality may improve, not only cost.** Accuracy decays as input grows, even for long-context models (Chroma, *Context Rot*, 2025: 18 models). A shorter working context is a candidate quality gain, to be measured, not assumed.

## 6. Sequence

1. **Done:** M1, M3 and the simulation.
2. **M5, offline.** For each of the 61 compactions, build a mechanical checkpoint from the segment before it: files edited, open error signatures verbatim, commands that went green, the objective, the next step. Then check what the next 30 calls re-read or asked about against three things: the host summary, the preserved segment, and the checkpoint. If the checkpoint adds nothing the host kept, stop here and lower the trigger with the host's mechanism alone.
3. **Checkpoint hooks, trigger unchanged.** PreCompact writes the checkpoint into `session_checkpoints`; SessionStart (`compact`) re-injects at most 1.5k tokens. Target: post-compaction growth falls from 2.58k/call toward 1.59k.
4. **Staged trigger, a week per step: 800k, then 600k, then 450k,** via `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` in settings. Primary metric: units per completed task. Guards: recovery growth, failed runs, repeated reads. Roll back on any regression beyond the noise floor.
5. **In parallel, independent of compaction:** the fixed-prompt audit, the loop guard, read-once. As in the document.

Deferred:
- **Idea 9** (restart from the checkpoint alone): riskiest; revisit after M5.
- **Idea 7** (split sessions at task boundaries): a working habit, not a mechanism.

Copilot is out of scope: its transcripts carry no usage.
