# quartz-ctx Multi-Language Execution Board

Status: Ready for execution planning review
Date: 2026-08-03

## Board usage
- Each task has a stable ID.
- Complete tasks in dependency order.
- Do not start blocked tasks early.
- Treat acceptance criteria as pass/fail gates.

Legend:
- Priority: P0 critical, P1 high, P2 medium
- Type: Arch, Core, Tooling, Test, Docs, Ops

## Milestones

1. M0: Contracts and scaffolding complete
2. M1: Rust path migrated to canonical architecture
3. M2: TS/JS and Python production candidate
4. M3: Tool surface upgrade and strict reliability behavior
5. M4: C# and Java/Kotlin integration
6. M5: hardening, performance, and rollout readiness

## Execution Table

| ID | Milestone | Priority | Type | Task | Depends On | Target Files | Acceptance Criteria |
|---|---|---|---|---|---|---|---|
| QCTX-0001 | M0 | P0 | Arch | Define canonical symbol schema | - | src/model.rs | Schema compiles; includes visibility, span, provenance, confidence |
| QCTX-0002 | M0 | P0 | Arch | Define canonical relationship edge taxonomy | QCTX-0001 | src/model.rs | Edge enums/types cover calls/imports/type-use/inheritance/override |
| QCTX-0003 | M0 | P0 | Arch | Add coverage report schema | QCTX-0001 | src/model.rs | Coverage structure includes unresolved refs and extractor failures |
| QCTX-0004 | M0 | P0 | Core | Introduce extractor trait and registry | QCTX-0001 | src/parser.rs | Registry supports pluggable language extractors |
| QCTX-0005 | M0 | P0 | Core | Introduce reconciler contract | QCTX-0001,QCTX-0002 | src/parser.rs,src/helpers.rs | Reconciler API accepts multi-source evidence and outputs canonical records |
| QCTX-0006 | M0 | P0 | Tooling | Add CLI flags for language, threshold, fail policy | QCTX-0004 | src/main.rs | CLI parses and routes new options with defaults |
| QCTX-0007 | M0 | P0 | Test | Add schema snapshot tests | QCTX-0001,QCTX-0002,QCTX-0003 | src/model.rs | Snapshots pass and are deterministic |
| QCTX-0010 | M1 | P0 | Core | Migrate Rust parser into RustExtractor plugin | QCTX-0004 | src/parser.rs | Existing Rust extraction behavior retained under plugin architecture |
| QCTX-0011 | M1 | P0 | Core | Add Rust semantic enricher baseline | QCTX-0010,QCTX-0005 | src/parser.rs,src/helpers.rs | Enricher resolves key symbol bindings and method ownership cases |
| QCTX-0012 | M1 | P0 | Core | Wire reconciler into Rust flow | QCTX-0011 | src/parser.rs | Canonical output produced with provenance fields populated |
| QCTX-0013 | M1 | P0 | Tooling | Add coverage computation for Rust path | QCTX-0012,QCTX-0003 | src/parser.rs,src/helpers.rs | Coverage report emitted with no silent parse failures |
| QCTX-0014 | M1 | P0 | Tooling | Add confidence scoring baseline | QCTX-0012 | src/helpers.rs | Confidence values present and bounded with documented rubric |
| QCTX-0015 | M1 | P1 | Test | Rust regression suite against current behavior | QCTX-0010,QCTX-0012 | src/parser.rs | Existing Rust-focused tool expectations still pass |
| QCTX-0020 | M2 | P0 | Core | Implement TS/JS AST extractor | QCTX-0004 | src/parser.rs,Cargo.toml | Extractor returns canonical symbol candidates for TS/JS fixtures |
| QCTX-0021 | M2 | P0 | Core | Implement TS/JS semantic adapter | QCTX-0020,QCTX-0005 | src/parser.rs | Semantic links resolved for imports, calls, and type references |
| QCTX-0022 | M2 | P0 | Core | Implement Python AST extractor | QCTX-0004 | src/parser.rs,Cargo.toml | Extractor returns canonical symbol candidates for Python fixtures |
| QCTX-0023 | M2 | P0 | Core | Implement Python semantic adapter | QCTX-0022,QCTX-0005 | src/parser.rs | Semantic links resolved for imports, calls, and class relationships |
| QCTX-0024 | M2 | P0 | Core | Reconciliation across Rust+TS/JS+Python | QCTX-0021,QCTX-0023 | src/parser.rs,src/helpers.rs | Conflicts surfaced with disagreement markers |
| QCTX-0025 | M2 | P0 | Test | Golden suite: recall/precision/F1 gates | QCTX-0024 | src/parser.rs | Meets thresholds: recall >=99.9, precision >=99.5, rel F1 >=98 |
| QCTX-0026 | M2 | P1 | Ops | Language health diagnostics output | QCTX-0024,QCTX-0003 | src/mcp.rs,src/main.rs | Per-language health visible with actionable warnings |
| QCTX-0030 | M3 | P0 | Tooling | Add get_coverage_report MCP tool | QCTX-0013,QCTX-0026 | src/mcp.rs | Tool returns current coverage + unresolved hot spots |
| QCTX-0031 | M3 | P0 | Tooling | Add get_symbol MCP tool | QCTX-0012 | src/mcp.rs | Returns canonical symbol by ID/fq-name with provenance |
| QCTX-0032 | M3 | P0 | Tooling | Add get_relationships MCP tool | QCTX-0012,QCTX-0002 | src/mcp.rs | Returns inbound/outbound edges with typed relations |
| QCTX-0033 | M3 | P0 | Tooling | Add get_blob MCP tool for full expansion | QCTX-0012 | src/mcp.rs,src/render/json.rs | Compact packets can be expanded losslessly |
| QCTX-0034 | M3 | P0 | Tooling | Upgrade get_api_context compact-handle format | QCTX-0033 | src/mcp.rs,src/render/context.rs | Response size reduced while full data remains retrievable |
| QCTX-0035 | M3 | P0 | Tooling | Enforce fail-closed protocol mode behavior | QCTX-0030,QCTX-0034 | src/mcp.rs | Critical calls blocked under low coverage/confidence |
| QCTX-0036 | M3 | P1 | Test | Token savings benchmark and reproducibility test | QCTX-0034 | src/render/context.rs | Measured reduction documented and repeatable on fixture set |
| QCTX-0040 | M4 | P1 | Core | Implement C# extractor + semantic integration | QCTX-0004,QCTX-0005 | src/parser.rs,Cargo.toml | C# fixtures pass base coverage and relation checks |
| QCTX-0041 | M4 | P1 | Core | Implement Java/Kotlin extractor + semantic integration | QCTX-0004,QCTX-0005 | src/parser.rs,Cargo.toml | Java/Kotlin fixtures pass base coverage and relation checks |
| QCTX-0042 | M4 | P1 | Test | Multi-language stress suite | QCTX-0040,QCTX-0041 | src/parser.rs | Mixed-language monorepo fixtures produce stable outputs |
| QCTX-0050 | M5 | P0 | Ops | Incremental indexing and cache correctness | QCTX-0024,QCTX-0034 | src/parser.rs,src/main.rs | Incremental refresh under target and no stale-symbol leaks |
| QCTX-0051 | M5 | P0 | Test | Critical-miss zero gate | QCTX-0025,QCTX-0042 | src/parser.rs,src/mcp.rs | No known critical misses in release candidate suites |
| QCTX-0052 | M5 | P1 | Docs | Update README with architecture and trust contract | QCTX-0035 | README.md | Docs clearly define lossless and failure semantics |
| QCTX-0053 | M5 | P1 | Docs | Update usage guide with protocol workflows | QCTX-0035 | USAGE_GUIDE.md | Includes compact-expand and fail-closed examples |
| QCTX-0054 | M5 | P1 | Ops | Rollout flag strategy and migration notes | QCTX-0051 | src/main.rs,README.md | Safe toggle path for teams upgrading from Rust-only mode |

## Gate Checklist

### Gate A (M1 complete)
- [ ] QCTX-0010
- [ ] QCTX-0011
- [ ] QCTX-0012
- [ ] QCTX-0013
- [ ] QCTX-0014
- [ ] QCTX-0015

Pass condition:
- Rust path is on canonical architecture with no material behavior regression.

### Gate B (M2 complete)
- [ ] QCTX-0020
- [ ] QCTX-0021
- [ ] QCTX-0022
- [ ] QCTX-0023
- [ ] QCTX-0024
- [ ] QCTX-0025
- [ ] QCTX-0026

Pass condition:
- First multi-language candidate passes quality thresholds.

### Gate C (M3 complete)
- [ ] QCTX-0030
- [ ] QCTX-0031
- [ ] QCTX-0032
- [ ] QCTX-0033
- [ ] QCTX-0034
- [ ] QCTX-0035
- [ ] QCTX-0036

Pass condition:
- Tool surface supports compact-first plus exact expansion, with strict reliability behavior.

### Gate D (M5 release readiness)
- [ ] QCTX-0040
- [ ] QCTX-0041
- [ ] QCTX-0042
- [ ] QCTX-0050
- [ ] QCTX-0051
- [ ] QCTX-0052
- [ ] QCTX-0053
- [ ] QCTX-0054

Pass condition:
- Quality, performance, and documentation gates all pass for broad rollout.

## Operational Sequence (recommended)

1. Execute all M0 tasks before adding any new language backend.
2. Complete M1 Rust migration and lock regression tests.
3. Implement M2 TS/JS and Python in parallel where possible.
4. Deliver M3 MCP surface and strict protocol behavior before widening adoption.
5. Add M4 languages after M3 stabilizes.
6. Finalize M5 hardening and rollout controls.

## Risk Board

| Risk ID | Risk | Impact | Mitigation | Trigger |
|---|---|---|---|---|
| R-01 | Semantic adapter unavailable on contributor machine | High | Capability report + degraded mode labeling + strict protocol fail-closed | Missing backend health check |
| R-02 | Token optimization accidentally hides necessary detail | High | Expansion handle contract + lossless blob retrieval tests | Context packet regression failures |
| R-03 | Schema churn across languages | Medium | Versioned canonical schema + snapshot tests | Failing compatibility snapshots |
| R-04 | Performance regressions in large repos | High | Incremental indexing + cache validation benchmarks | Query latency over SLO |

## Definition of Done

Project-level done criteria:
- Multi-language extraction (at least Rust+TS/JS+Python) passes quality gates.
- Compact-first responses show measurable token reduction with deterministic expansion.
- No critical silent misses under protocol mode.
- Documentation and migration paths are complete and reviewable.
