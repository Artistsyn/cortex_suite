# quartz-ctx Multi-Language Lossless Plan

Status: Draft for evaluation
Date: 2026-08-03
Scope: quartz-ctx evolution from Rust-focused API helper to language-agnostic, lossless, token-efficient code intelligence service.

## 1) Outcome

Build a local-only, multi-language quartz-ctx that gives agents:
- Near-complete symbol and relationship coverage across supported languages.
- No silent misses for critical flows.
- Compact-first responses with exact drill-down detail.
- Explicit confidence, provenance, and coverage in every critical response.

Primary V1 languages:
- Rust (existing baseline)
- TypeScript/JavaScript
- Python

Secondary V2 languages:
- C#
- Java/Kotlin

## 2) Why This Design

### Problem today
- Current parser is Rust/syn-specific, so cross-language projects are out of scope.
- Existing tools are useful but do not enforce a strict coverage contract.
- Token efficiency exists, but a formal compact-plus-lossless contract is not systematized.

### Design choice
Use a hybrid extraction stack:
1. AST extraction per language for breadth and speed.
2. Semantic extraction (LSP/compiler metadata) for resolved truth.
3. Reconciliation layer to produce one canonical symbol graph.

Why this works:
- AST alone is fast but misses resolved semantics.
- Semantic alone can be environment-sensitive and slower.
- Hybrid gives both completeness and correctness.

## 3) Explicit Definitions

### Lossless (operational definition)
For indexed files and enabled language backends, quartz-ctx stores retrievable canonical data for:
- Symbol identity and source span.
- Signature and structural fields.
- Visibility and ownership context.
- Key relationships (calls, imports, type-use, inheritance/impl/override, instantiation).
- Documentation text or doc references.
- Provenance (which extractor produced what).

Lossless in this plan means no irreversible dropping of canonical detail in stored data. Compact responses may summarize, but must include exact expansion handles.

### Coverage
Coverage is measured per language and per repository slice:
- Symbol recall estimate against golden baselines.
- Unresolved references count.
- Extractor parse/enrichment failures.
- Relationship completeness score.

### Confidence
Confidence is derived from:
- Multi-source agreement.
- Extractor health.
- Resolution depth.
- Known conflict markers.

## 4) Product Behavior Contract

### Contract A: Compact-first, exact-expand
- Default responses are token-efficient packets.
- Every compact packet must provide deterministic expansion handles.
- Expansion tools return full canonical details without lossy reformatting.

### Contract B: No silent misses
- Critical tools return coverage and confidence headers.
- If minimum confidence/coverage threshold is not met, tools fail closed in protocol mode and report missing coverage explicitly.

### Contract C: Public/private classification
- Index public and private symbols.
- Preserve visibility tags and show how public APIs depend on private internals.

## 5) Architecture

## 5.1 Pipeline
1. Source discovery and language partitioning.
2. Language AST extractors produce raw symbol records.
3. Semantic enrichers attach resolved typing and cross-file links.
4. Reconciler merges records into canonical graph with disagreement markers.
5. Coverage engine computes per-language and global quality metrics.
6. MCP tools expose compact packets and lossless detail expansion.

## 5.2 Canonical entities
- Symbol
- Relationship edge
- Document fragment
- Extractor evidence record
- Coverage report
- Confidence profile

## 5.3 Storage strategy
- Local-only persistent storage.
- Content-addressed blobs for repeated detail blocks.
- Stable symbol IDs for deterministic references.

## 6) Token Efficiency Strategy

1. Compact packet composer with budget profiles.
2. Deduplicate repeated docs/signatures via content hashes.
3. Session-aware suppression of unchanged blocks.
4. Deterministic relevance ranking and bounded edge fan-out.
5. Structured JSON payloads first, markdown render second.

Guardrail:
- Never truncate canonical data in storage.
- Truncation is presentation-only and reversible via expansion handles.

## 7) Reliability and Testing Strategy

### Golden suites
Create representative repos and fixtures for each language.

### Gates
- API recall target: >= 99.9%
- Precision target: >= 99.5%
- Relationship F1 target: >= 98%
- Critical misses: 0 before default-on rollout

### Runtime checks
- Tool-level assertions for missing coverage.
- Deterministic output regression tests.
- Cache invalidation and index-version consistency tests.

## 8) Integration Strategy with Cortex and Graphify

### Cortex
- quartz-ctx produces high-confidence context payloads.
- Cortex ingests only threshold-passing candidates.
- Stage memory candidates automatically; final commit remains explicit.

### Graphify
- Export canonical symbol-edge graph for graphify ingestion.
- Preserve stable IDs and edge types for drift and path analysis.

## 9) Implementation Plan (Phased)

## Phase 0: Scaffolding and contracts
Goal: establish interfaces and schemas without breaking existing Rust flow.

Deliverables:
- Extractor trait contracts.
- Canonical symbol and edge schema.
- Coverage and confidence schema.
- New MCP tool contracts documented.

## Phase 1: Rust migration to new core
Goal: move current Rust parser into plugin architecture.

Deliverables:
- Rust extractor plugin.
- Rust semantic enricher baseline.
- Reconciler integration.
- Coverage report generation.

## Phase 2: TypeScript/JavaScript + Python
Goal: first multi-language production candidate.

Deliverables:
- TS/JS AST + semantic adapter.
- Python AST + semantic adapter.
- Unified reconciliation across 3 languages.
- Tool output confidence and coverage in responses.

## Phase 3: Tool surface upgrade
Goal: make agent workflows default to quartz-ctx because it is cheaper and safer.

Deliverables:
- New tools: get_coverage_report, get_symbol, get_relationships, get_blob.
- Upgraded get_api_context with compact handles.
- Fail-closed mode for protocol sessions.

## Phase 4: C# + Java/Kotlin
Goal: broaden applicability for typical enterprise/game toolchains.

Deliverables:
- C# extractor + semantic integration.
- Java/Kotlin extractor + semantic integration.
- Language health dashboard support.

## Phase 5: hardening and rollout
Goal: strict quality gates and stable adoption.

Deliverables:
- Golden suite pass.
- Performance profile pass.
- Documentation and migration notes.
- Rollout feature flags and fallback behavior.

## 10) File-Level Change Intent (line-by-line implementation guidance)

This section maps intent to existing files and expected edits.

### src/model.rs
- Add language-agnostic Symbol model.
- Add Visibility enum.
- Add stable IDs and source span fields.
- Add provenance and confidence structures.
- Add canonical Relationship type list.

Rationale:
One normalized schema is required before adding more parsers.

### src/parser.rs
- Convert Rust-centric entry points into orchestrator and extractor registry.
- Keep existing Rust parser logic as RustExtractor implementation.
- Add result envelope including extractor diagnostics.

Rationale:
Allows adding languages without reworking every downstream tool.

### src/main.rs
- Add CLI flags for language enablement, coverage thresholds, and fail policy.
- Add mode switches for compact profiles and strict protocol behavior.

Rationale:
Operational controls are needed for deterministic deployments.

### src/mcp.rs
- Add tool outputs that include confidence and coverage summaries.
- Add new tools for symbol/relationships/blob/coverage retrieval.
- Enforce fail-closed behavior for critical tools in protocol mode.

Rationale:
Reliability and trust must be visible at query time, not inferred.

### src/render/json.rs
- Emit canonical JSON contracts with stable ordering.
- Add compact packet schema and expansion handles.

Rationale:
Structured JSON is lower token and better for downstream automation.

### src/render/context.rs
- Build budget-aware packet selection.
- Add deterministic clipping behavior with expansion references.

Rationale:
Token efficiency should be predictable and lossless.

### src/helpers.rs
- Add scoring helpers for confidence and reconciliation diagnostics.
- Add invariant checks for symbol identity and edge integrity.

Rationale:
Detecting and surfacing uncertainty prevents silent errors.

### Cargo.toml
- Add feature-gated language backends and parser/semantic dependencies.

Rationale:
Keeps local setups configurable while expanding capability.

### README.md and USAGE_GUIDE.md
- Add multi-language architecture docs.
- Document strict coverage behavior and troubleshooting.

Rationale:
Adoption depends on clarity of guarantees and limitations.

## 11) Operational Defaults

Recommended defaults:
- Include private + public symbols.
- Exclude generated and vendor directories by default.
- Include examples and tests but mark as non-production stratum.
- Fail closed in protocol mode when below coverage threshold.
- Local-only storage and processing.

## 12) Risks and Mitigations

Risk: semantic backends unavailable on some machines.
Mitigation: capability report + explicit degraded mode labeling.

Risk: performance overhead from hybrid extraction.
Mitigation: incremental indexing, content hash caching, selective re-enrichment.

Risk: schema drift across languages.
Mitigation: canonical schema tests and compatibility snapshots.

Risk: agents ignore tools.
Mitigation: one-call context advantage, protocol ordering policy, and quality headers.

## 13) Decision Gates

Gate A (end Phase 1)
- Rust path fully migrated to canonical model and no feature regressions.

Gate B (end Phase 2)
- TS/JS and Python reach minimum quality thresholds on golden suites.

Gate C (end Phase 3)
- New tool surface stable; compact packet savings measured and reproducible.

Gate D (end Phase 5)
- Multi-language quality and performance SLOs pass; rollout approved.

## 14) Recommendation

Start with Phase 0 and Phase 1 immediately to avoid architecture debt. Do not add new language extractors until canonical schema, coverage reports, and fail policy are in place.
