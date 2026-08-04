# Cortex-Embedded Code Intelligence Plan

Status: Draft for evaluation
Date: 2026-08-03
Scope: Evaluate and plan a route where the multi-language, lossless code intelligence capabilities are added directly into Cortex tooling for tighter self-learning integration.

## 1) Executive Assessment

Short answer: this is a good idea if implemented as a hybrid architecture, not a hard rewrite.

Recommended direction:
- Keep quartz-ctx as the extraction and normalization engine.
- Add a new Cortex tool layer that consumes the same canonical outputs and exposes memory-aware workflows.
- Avoid duplicating parser/extractor logic independently in Cortex.

Why this is the best route:
- It preserves a single source of truth for extraction logic.
- It leverages Cortex strengths: protocol gating, self-learning, query-gap logs, compression telemetry, and memory workflows.
- It reduces agent context-switching by making high-value code intelligence available where agents already operate.

## 2) Option Assessment

### Option A: Keep quartz-ctx separate only
Pros:
- Minimal disruption.
- Clean separation of concerns.

Cons:
- Weaker coupling to Cortex learning loop.
- More agent coordination overhead.

Assessment:
- Good baseline, but leaves value on the table for self-learning.

### Option B: Full merge into Cortex (replace quartz-ctx runtime path)
Pros:
- Single MCP endpoint for everything.
- Unified telemetry and gating.

Cons:
- High migration risk.
- Parser stack complexity expands Cortex blast radius.
- Harder to test and ship safely.

Assessment:
- Possible, but too risky as first move.

### Option C: Hybrid integration (recommended)
Pros:
- Best of both worlds.
- Reuse extraction core while adding Cortex-native intelligence tools.
- Safer rollout with feature flags and compatibility layers.

Cons:
- Requires careful API contracts between projects.

Assessment:
- Preferred architecture and implementation path.

## 3) Target Architecture

Core principle:
- One canonical multi-language extraction core.
- Two access layers:
  - quartz-ctx MCP for API-centric direct workflows.
  - Cortex MCP for protocol/self-learning workflows that consume the same canonical data.

### Components
1. Extraction Core
- Language parsers, semantic enrichers, reconciler, coverage/confidence engine.

2. Canonical Artifacts
- Symbol graph, relationship edges, provenance, confidence, coverage reports.

3. Cortex Integration Layer
- New Cortex tools that read canonical artifacts and enrich responses with pattern memory and protocol awareness.

4. Memory and Learning Hooks
- Query gap tracking, confidence misses, unresolved symbol stats, and adoption telemetry feed into Cortex proposals.

## 4) What Cortex Should Gain

Add a new family of code-intelligence tools in Cortex (names illustrative):
- ctx_index_workspace
- ctx_get_language_health
- ctx_get_coverage_report
- ctx_get_symbol
- ctx_get_relationships
- ctx_get_api_context
- ctx_expand_blob
- ctx_find_conflicts

Tool behaviors:
- Return explicit confidence and coverage metadata.
- Fail closed in protocol mode when quality thresholds are below policy.
- Log misses and uncertainty into Cortex query-gap and proposal pipelines.

## 5) Data Model and Storage Plan

Two viable strategies:

### Strategy 1: Shared artifact files (recommended initial)
- quartz-ctx writes canonical JSON artifacts.
- Cortex reads artifacts via stable schema.

Pros:
- Loose coupling and easy rollback.
- Independent release cadence.

Cons:
- Artifact synchronization complexity.

### Strategy 2: Shared library crate
- Move extractor core into a reusable library crate.
- quartz-ctx binary and Cortex both depend on it.

Pros:
- Strong type-level contract, less serialization glue.

Cons:
- Tighter dependency management and release coordination.

Recommended sequence:
- Start with Strategy 1 for speed and safety.
- Move to Strategy 2 once schema stabilizes.

## 6) Why This Improves Self-Learning

Cortex already has strong mechanisms that amplify this feature set:
- Protocol gating and sequence compliance.
- Session-aware caching and dedup.
- Query-gap logging and proposal pipelines.
- Lossless compaction telemetry.

By integrating code intelligence tools into Cortex:
- Gaps become measurable and actionable automatically.
- Recurrent misses can drive new patterns, anti-patterns, and preference notes.
- Agent behavior can be nudged toward reliable tool usage with explicit confidence and coverage.

## 7) Reliability Contract

For Cortex-integrated code intelligence tools:
- No silent partial results on critical calls.
- Include language coverage and confidence in output.
- Mark disagreement across extractors when reconciliation is uncertain.
- Provide deterministic expansion paths for compact packets.

Protocol-mode policy:
- If coverage or confidence below threshold, fail closed and report remediation path.

## 8) Risks and Mitigations

Risk 1: Duplicate logic divergence between quartz-ctx and Cortex.
- Mitigation: single canonical schema and extraction source; avoid hand-maintained parallel parsers.

Risk 2: Performance degradation in Cortex server.
- Mitigation: offline/indexed artifacts, incremental refresh, bounded query APIs.

Risk 3: Tool sprawl reducing clarity.
- Mitigation: grouped tool namespace and one-call context entrypoint.

Risk 4: Partial backend availability per machine.
- Mitigation: language health diagnostics and explicit degraded mode signals.

## 9) Implementation Roadmap

## Phase C0: Contracts and compatibility
Goal:
- Define schema contracts and integration boundaries.

Deliverables:
- Canonical artifact schema versioning.
- Cortex tool input/output contracts.
- Threshold policy definitions for fail-closed behavior.

## Phase C1: Read-only Cortex integration
Goal:
- Cortex reads quartz-ctx canonical artifacts.

Deliverables:
- ctx_get_api_context and ctx_get_symbol baseline tools.
- Coverage and confidence surfaced in output.
- Query-gap logs for missing symbols and low-confidence areas.

## Phase C2: Protocol and memory coupling
Goal:
- Promote high-trust usage in protocol workflows.

Deliverables:
- Protocol ordering guidance updates.
- Fail-closed policy activation for critical tools.
- Learning hooks for repeated misses and conflict trends.

## Phase C3: Deep feature parity
Goal:
- Add relationship and expansion tooling.

Deliverables:
- ctx_get_relationships, ctx_expand_blob, ctx_find_conflicts.
- Deterministic compact-plus-expand contract.

## Phase C4: Shared-core consolidation
Goal:
- Reduce duplication and harden long-term maintenance.

Deliverables:
- Optional shared extraction library integration.
- Unified benchmarks and regression suite.

## 10) File-Level Planning Targets

Primary Cortex targets:
- cortex/src/mcp/tools.rs
- cortex/src/mcp/mod.rs
- cortex/src/model.rs
- cortex/src/planner.rs
- cortex/src/memory.rs

Primary quartz-ctx targets:
- quartz-ctx/src/model.rs
- quartz-ctx/src/parser.rs
- quartz-ctx/src/render/json.rs
- quartz-ctx/src/mcp.rs

Shared docs:
- quartz-ctx/MULTILANG_LOSSLESS_PLAN.md
- quartz-ctx/MULTILANG_EXECUTION_BOARD.md
- New integration runbook in Cortex docs.

## 11) Gate Criteria

Gate C-A:
- Cortex can read canonical artifacts and answer ctx_get_symbol with confidence/coverage metadata.

Gate C-B:
- Protocol-mode fail-closed behavior works for low-confidence critical queries.

Gate C-C:
- Query-gap and telemetry hooks prove learning loop value with real session data.

Gate C-D:
- Performance and reliability targets pass on representative repos.

## 12) Recommendation

Proceed with the hybrid Cortex-integrated route.

Do not replace quartz-ctx extraction logic immediately. Instead:
1. Build Cortex read-path tools on top of quartz-ctx canonical artifacts.
2. Validate reliability and agent adoption gains.
3. Consolidate into shared-core architecture after schema and performance stabilize.

This sequence gives you strategic upside with lower migration risk and a clear rollback path.
