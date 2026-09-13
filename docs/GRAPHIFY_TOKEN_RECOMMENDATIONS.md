# Graphify + token-optimizer recommendations for cortex_suite

This file is intended to be handed to an implementing agent.

## Executive summary

- **Graphify:** update cortex_suite's graphify integration assumptions before upgrading `graphify-rs`.
- **token-optimizer-mcp:** do **not** integrate wholesale; selectively adapt the low-risk token-saving ideas that match cortex's current design.

## Recommendation 1: keep cortex's explicit graph output path

Keep using:

- `.graphify-output/graph.json`
- `.graphify-output/snapshots/`

Do **not** switch cortex to graphify's newer default output directory such as `graphify-rs-out/`.

### Why

Cortex currently hardcodes the in-repo graph contract in:

- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/closeout.rs`
- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/consolidator2.rs`
- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/main.rs`

Keeping the explicit `--output .graphify-output` contract preserves existing drift-analysis behavior and avoids silent breakage.

## Recommendation 2: re-check the graphify rebuild command against current upstream

Upstream `graphify-rs` has moved beyond the version documented in this repository. The implementing agent should verify the current CLI behavior before changing runtime integration.

### Highest-priority compatibility check

Re-check whether `--update` is still valid on the current `graphify-rs` release.

If upstream has removed `--update`, update cortex_suite's rebuild command accordingly in:

- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/closeout.rs`
- `/home/runner/work/cortex_suite/cortex_suite/docs/GRAPHIFY.md`
- `/home/runner/work/cortex_suite/cortex_suite/README.md`
- `/home/runner/work/cortex_suite/cortex_suite/SETUP_HANDOFF.md`

### Likely target command shape

Prefer an explicit command in this shape:

```bash
graphify-rs build --path . --code-only --format json --output .graphify-output
```

If incremental detection is automatic in the chosen upstream version, do not keep a stale `--update` flag just because older docs used it.

## Recommendation 3: refresh repository docs to match tested upstream behavior

The implementing agent should update the repository's graphify guidance only after confirming the actual installed CLI behavior of the target release.

### Doc areas to refresh

- `/home/runner/work/cortex_suite/cortex_suite/docs/GRAPHIFY.md`
- `/home/runner/work/cortex_suite/cortex_suite/README.md`
- `/home/runner/work/cortex_suite/cortex_suite/SETUP_HANDOFF.md`

### What to refresh

- the verified upstream version
- whether `--update` still exists
- whether default output behavior changed upstream
- the fact that cortex_suite still intentionally requires `--output .graphify-output`
- whether `serve` auto-build behavior is now available upstream

## Recommendation 4: do not adopt token-optimizer-mcp as a full dependency

Do **not** import its full proxy, hook-enforcement, or lossy-compression model into cortex_suite as a first step.

### Why

- it is more invasive than cortex's current architecture
- it depends on host/client lifecycle surfaces that vary across tools
- it introduces behavior changes outside cortex's current MCP/data flow
- parts of its approach can write intermediate content to disk, which increases operational risk

## Recommendation 5: selectively adapt the low-risk ideas from token-optimizer-mcp

These are the best candidates for adaptation.

### A. Strongly recommended: repeated-read suppression

Extend cortex's existing session-aware content reuse instead of replacing it.

Relevant current code:

- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/cache.rs`

Target outcome:

- if content was already sent this session, return a compact reference or delta instead of replaying the full body
- preserve the current bias toward explicit, inspectable behavior

### B. Strongly recommended: question-aware progressive disclosure

Adapt this for large tool outputs, but keep it **lossless-by-default**.

Relevant current code:

- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/output_filter.rs`

Target outcome:

- show the most relevant preview first
- provide explicit expansion handles or file paths for omitted material
- never silently truncate diagnostics or remove information needed for debugging

### C. Worth adding: prompt-cache hygiene diagnostics

Add reporting that identifies instructions or generated text that frequently invalidate prompt-cacheable prefixes.

Relevant likely touchpoints:

- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/main.rs`
- `/home/runner/work/cortex_suite/cortex_suite/cortex/src/model.rs`

Target outcome:

- show which repeated context blocks are stable versus churn-heavy
- identify avoidable cache-busting text such as timestamps or frequently rewritten boilerplate

## Recommendation 6: avoid lossy context compression unless it is explicitly optional

If the implementing agent experiments with more aggressive token reduction, keep it:

- opt-in
- measurable
- reversible
- separated from cortex's current lossless storage/compaction behavior

Good boundary:

- optional compression on the final assembled context pack

Bad boundary:

- rewriting durable stored knowledge
- silently dropping detail from diagnostics
- changing default behavior without measurement

## Recommended implementation order

1. Validate the current upstream `graphify-rs` CLI against cortex's rebuild command.
2. Update cortex's rebuild invocation if `--update` is obsolete.
3. Refresh graphify docs to match the tested command and version.
4. Improve repeated-read suppression in cortex's existing session-aware cache/render path.
5. Add lossless progressive disclosure for large outputs.
6. Add prompt-cache hygiene diagnostics.
7. Leave proxy-based or lossy token-optimizer ideas for a later, explicitly experimental phase.

## Acceptance criteria for the implementing agent

- cortex still reads and snapshots graphs from `.graphify-output/`
- graph rebuild succeeds with the chosen tested `graphify-rs` version
- drift analysis still works after the graphify update
- docs match the actually tested command, not upstream assumptions
- any token-saving change preserves context quality and does not silently hide diagnostics
- no new default behavior depends on client-specific hooks unless explicitly documented and optional
