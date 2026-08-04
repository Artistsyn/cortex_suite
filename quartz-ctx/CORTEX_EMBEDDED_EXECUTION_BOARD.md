# Cortex-Embedded Code Intelligence Execution Board

Status: Ready for execution planning review
Date: 2026-08-03
Related strategy document: CORTEX_EMBEDDED_CODE_INTELLIGENCE_PLAN.md

## Board usage
- Each task has a stable ID.
- Complete in dependency order unless explicitly marked parallel-safe.
- Treat acceptance criteria as pass/fail gates.
- Do not enable protocol fail-closed policy before minimum tool coverage is in place.

Legend:
- Priority: P0 critical, P1 high, P2 medium
- Type: Arch, Core, Tooling, Data, Test, Docs, Ops

## Milestones

1. CM0: Integration contracts and schema boundaries
2. CM1: Cortex read-path for canonical artifacts
3. CM2: Protocol + self-learning coupling
4. CM3: Deep tool parity (relationships and expansion)
5. CM4: Optional shared-core consolidation and rollout hardening

## Execution Table

| ID | Milestone | Priority | Type | Task | Depends On | Target Files | Acceptance Criteria |
|---|---|---|---|---|---|---|---|
| CCTX-0001 | CM0 | P0 | Arch | Define canonical artifact schema versions for Cortex consumption | - | quartz-ctx/src/render/json.rs, cortex/src/model.rs | Schema version field and compatibility rules documented and testable |
| CCTX-0002 | CM0 | P0 | Arch | Define contract for confidence and coverage metadata in every critical response | CCTX-0001 | cortex/src/mcp/tools.rs, cortex/src/model.rs | Tool response envelope includes confidence + coverage payload |
| CCTX-0003 | CM0 | P0 | Arch | Define fail policy thresholds per tool category | CCTX-0002 | cortex/src/mcp/mod.rs, cortex/src/mcp/tools.rs | Threshold table exists and is enforced behind feature flag |
| CCTX-0004 | CM0 | P0 | Data | Define artifact discovery and freshness checks | CCTX-0001 | cortex/src/mcp/tools.rs, cortex/src/memory.rs | Cortex can locate artifacts and reject stale versions |
| CCTX-0005 | CM0 | P1 | Test | Contract snapshot tests for artifact schema and response envelope | CCTX-0001,CCTX-0002 | cortex/src/model.rs | Deterministic snapshots pass |
| CCTX-0010 | CM1 | P0 | Core | Implement read-only loader for quartz-ctx canonical artifacts | CCTX-0004 | cortex/src/mcp/tools.rs, cortex/src/model.rs | Loader parses valid artifacts and reports clear errors for invalid/stale |
| CCTX-0011 | CM1 | P0 | Tooling | Add ctx_get_symbol tool | CCTX-0010,CCTX-0002 | cortex/src/mcp/tools.rs, cortex/src/mcp/mod.rs | Returns canonical symbol by ID/fq-name with confidence + provenance |
| CCTX-0012 | CM1 | P0 | Tooling | Add ctx_get_api_context tool (compact-first packet) | CCTX-0010,CCTX-0002 | cortex/src/mcp/tools.rs, cortex/src/planner.rs | Budgeted context packets produced with deterministic ranking |
| CCTX-0013 | CM1 | P0 | Tooling | Add ctx_get_coverage_report and ctx_get_language_health tools | CCTX-0010,CCTX-0002 | cortex/src/mcp/tools.rs, cortex/src/mcp/mod.rs | Coverage and backend health surfaced with actionable diagnostics |
| CCTX-0014 | CM1 | P1 | Ops | Cache integration for artifact-backed responses (index-version aware) | CCTX-0010 | cortex/src/mcp/mod.rs, cortex/src/cache.rs | Cache hit path works; stale cache invalidation verified |
| CCTX-0015 | CM1 | P1 | Test | Baseline integration tests for read-path tools | CCTX-0011,CCTX-0012,CCTX-0013 | cortex/src/mcp/tools.rs | All new tools return expected structures on fixture artifacts |
| CCTX-0020 | CM2 | P0 | Tooling | Add protocol-gated fail-closed behavior for critical ctx_* tools | CCTX-0003,CCTX-0013 | cortex/src/mcp/mod.rs, cortex/src/mcp/tools.rs | Low confidence/coverage blocks critical calls in protocol mode |
| CCTX-0021 | CM2 | P0 | Data | Log ctx_* query misses and uncertainty into gap pipeline | CCTX-0011,CCTX-0013 | cortex/src/mcp/tools.rs, cortex/src/memory.rs | Query gaps include symbol misses, stale artifacts, low-confidence incidents |
| CCTX-0022 | CM2 | P0 | Data | Add telemetry counters for compact packet savings and expansion rate | CCTX-0012 | cortex/src/mcp/tools.rs, cortex/src/memory.rs | Token/size savings and expansion usage metrics recorded |
| CCTX-0023 | CM2 | P1 | Docs | Update protocol guidance for ctx_* tool ordering | CCTX-0020 | .github/copilot-instructions.md, cortex/README.md | Guidance includes when to call ctx_get_coverage_report before critical usage |
| CCTX-0024 | CM2 | P1 | Test | Regression tests for fail-closed and degraded mode messaging | CCTX-0020 | cortex/src/mcp/mod.rs, cortex/src/mcp/tools.rs | Clear, deterministic remediation messages under threshold failures |
| CCTX-0030 | CM3 | P0 | Tooling | Add ctx_get_relationships tool | CCTX-0010,CCTX-0001 | cortex/src/mcp/tools.rs, cortex/src/model.rs | Inbound/outbound typed edges returned with source provenance |
| CCTX-0031 | CM3 | P0 | Tooling | Add ctx_expand_blob tool for exact lossless expansion | CCTX-0012,CCTX-0001 | cortex/src/mcp/tools.rs, cortex/src/cache.rs | Compact packet handles expand to full canonical data without loss |
| CCTX-0032 | CM3 | P0 | Tooling | Add ctx_find_conflicts tool for extractor disagreement visibility | CCTX-0010,CCTX-0002 | cortex/src/mcp/tools.rs | Conflicting symbol/edge evidence listed with confidence deltas |
| CCTX-0033 | CM3 | P1 | Test | Compact-expand determinism test suite | CCTX-0031 | cortex/src/mcp/tools.rs, cortex/src/planner.rs | Same handle always maps to same canonical payload for same index version |
| CCTX-0034 | CM3 | P1 | Ops | Performance benchmark for ctx_* tool latency and payload size | CCTX-0030,CCTX-0031 | cortex/src/mcp/tools.rs | Meets latency budget and predictable packet size envelopes |
| CCTX-0040 | CM4 | P1 | Arch | Evaluate shared-core crate migration feasibility | CCTX-0034 | quartz-ctx/Cargo.toml, cortex/Cargo.toml | Decision memo with risk and migration path |
| CCTX-0041 | CM4 | P1 | Core | Implement optional shared-core crate adapter (feature-gated) | CCTX-0040 | quartz-ctx/src/parser.rs, cortex/src/mcp/tools.rs | Both artifact mode and shared-core mode compile and run |
| CCTX-0042 | CM4 | P0 | Test | End-to-end golden suite across representative multi-language repos | CCTX-0034 | cortex/src/mcp/tools.rs, quartz-ctx/src/parser.rs | Recall, precision, and relationship targets pass |
| CCTX-0043 | CM4 | P1 | Docs | Final rollout docs and migration guide | CCTX-0042 | cortex/README.md, quartz-ctx/README.md, quartz-ctx/USAGE_GUIDE.md | Upgrade path and fallback mode clearly documented |
| CCTX-0044 | CM4 | P0 | Ops | Rollout flags and safety fallback policy | CCTX-0042 | cortex/src/mcp/mod.rs, cortex/src/mcp/tools.rs | Safe toggle path with rapid rollback capability validated |

## Gate Checklist

### Gate CM-A (CM1 complete)
- [ ] CCTX-0010
- [ ] CCTX-0011
- [ ] CCTX-0012
- [ ] CCTX-0013
- [ ] CCTX-0014
- [ ] CCTX-0015

Pass condition:
- Cortex can consume canonical artifacts and serve baseline ctx_* tools with confidence and coverage metadata.

### Gate CM-B (CM2 complete)
- [ ] CCTX-0020
- [ ] CCTX-0021
- [ ] CCTX-0022
- [ ] CCTX-0023
- [ ] CCTX-0024

Pass condition:
- Protocol reliability policy is active and learning-loop data capture is functioning.

### Gate CM-C (CM3 complete)
- [ ] CCTX-0030
- [ ] CCTX-0031
- [ ] CCTX-0032
- [ ] CCTX-0033
- [ ] CCTX-0034

Pass condition:
- Deep parity tools (relationships/conflicts/expansion) are stable and deterministic.

### Gate CM-D (CM4 release readiness)
- [ ] CCTX-0040
- [ ] CCTX-0041
- [ ] CCTX-0042
- [ ] CCTX-0043
- [ ] CCTX-0044

Pass condition:
- Quality thresholds, rollout controls, and migration docs are complete.

## Operational Sequence (recommended)

1. Complete CM0 schema and policy contracts first.
2. Deliver CM1 read-path tools before any fail-closed activation.
3. Enable CM2 protocol coupling only after language health and coverage tools are stable.
4. Build CM3 deep tools and verify compact-expand determinism.
5. Use CM4 to decide whether shared-core consolidation is worth the complexity.

## Parallelization Lanes

Lane A (Schema/Model)
- CCTX-0001, CCTX-0002, CCTX-0005

Lane B (Read Path)
- CCTX-0004, CCTX-0010, CCTX-0014

Lane C (Tool Surface)
- CCTX-0011, CCTX-0012, CCTX-0013, CCTX-0030, CCTX-0031, CCTX-0032

Lane D (Protocol/Learning)
- CCTX-0020, CCTX-0021, CCTX-0022, CCTX-0023, CCTX-0024

Lane E (Quality/Rollout)
- CCTX-0033, CCTX-0034, CCTX-0042, CCTX-0043, CCTX-0044

## Risk Board

| Risk ID | Risk | Impact | Mitigation | Trigger |
|---|---|---|---|---|
| CR-01 | Artifact/schema drift between quartz-ctx and Cortex | High | Versioned schema + strict compatibility checks + snapshot tests | Loader rejection spikes |
| CR-02 | Protocol fail-closed blocks too aggressively | Medium | Tiered threshold profiles and explicit remediation messages | Rising blocked-call rate without quality gain |
| CR-03 | Performance regressions from artifact parsing | High | Caching + incremental freshness checks + bounded payloads | Latency budget violations |
| CR-04 | Duplication of extraction logic creeps into Cortex | High | Keep Cortex read-only initially; shared-core decision gate at CM4 | Divergent extraction behavior across tools |
| CR-05 | Tool sprawl reduces adoption | Medium | One-call ctx_get_api_context default path + clear docs | Low usage of new tools despite availability |

## Definition of Done

Project-level done criteria:
- Cortex ctx_* tools provide reliable, confidence-aware, coverage-aware multi-language context.
- Protocol mode prevents silent low-quality output for critical flows.
- Self-learning loop receives high-signal gap and uncertainty telemetry.
- Compact-first responses remain lossless through deterministic expansion handles.
- Rollout controls and fallback behavior are validated and documented.
