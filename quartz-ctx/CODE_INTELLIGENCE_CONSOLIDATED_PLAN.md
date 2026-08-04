# Code Intelligence — Consolidated Assessment and Plan

Status: Verified. Revision 2 (2026-08-03) — five claims from revision 1 were wrong
and are corrected in §0.
Supersedes the *sequencing* in `MULTILANG_LOSSLESS_PLAN.md`,
`MULTILANG_EXECUTION_BOARD.md`, `CORTEX_EMBEDDED_CODE_INTELLIGENCE_PLAN.md`,
`CORTEX_EMBEDDED_EXECUTION_BOARD.md`. Those remain valid as *destination* documents.

Every claim below was measured on 2026-08-03. Re-verification commands are in §7.

---

## 0) Corrections to revision 1

Revision 1 was written from the live `.cortex/memory.db`, which turned out to contain
stale residue. A clean reindex changed five conclusions:

| # | Revision 1 claimed | Actually |
|---|---|---|
| C1 | 46% of typed units have no methods — "the index is broadly broken" | **Wrong framing.** On a clean index it is 32%, and nearly all of those legitimately have no methods. The real measure: **2 of 76 types lose methods vs quartz-ctx — but they are `Canvas` (115 lost) and `GameObject` (16 lost), 131 methods total.** Concentrated and severe, not broad. |
| C2 | Cortex indexes private symbols; quartz-ctx is public-only; the two are complementary strata to be merged | **Wrong.** Both are `pub`-only (`is_pub`, [compressor.rs:462](../cortex/src/compressor.rs)). They extract nearly the same set — 107 vs 117 items for `quartz/src`. **Phase B needs no two-stratum merge.** |
| C3 | Cortex's 3,682 edges vs graphify's 10,025 shows edge undercounting | **Invalid comparison** — different scope and granularity. Dropped. The real figure: a clean `quartz/src` index yields 422 edges, only 51 `uses` + 5 `implements`. |
| C4 | 1,307 rejected proposals show the drift stage floods the review funnel | **Wrong for current behaviour.** 1,305 were created in one burst on 2026-07-16 04:17 (pre-fix); the run 26 minutes later produced exactly **2**. The digest-aggregation fix works. What is true: the backlog was never purged, the pipeline has not run since, and the meta-analyzer computes "0/1304 approved" over dead rows. |
| C5 | — | **New finding.** Cortex's live index is never pruned: it holds 94 units of cortex's own source and 35 from `air_src`, neither of which is in `.cortex/index-sources.json`. This residue is what inflated the 46% figure. |
| C6 | "One parser bug degraded six tools, including graph edge inference" | **Overstated.** Measured post-fix: edge counts are byte-identical before and after. Cortex stores bare method *names*, and edge inference only harvests uppercase-initial tokens, so methods never produced `uses` edges. The bug degraded `get_item`, `recall` and `semantic_search` — three tools, not six. See §2.1. |

The central finding survived verification and got **stronger**, because it reproduces
on a clean index built with the current binary — it was never staleness.

---

## 1) Verdict

**The four plans start at the wrong end.** They propose multi-language extraction,
LSP semantic enrichers, C#/Java backends, fail-closed protocol gating, a ≥99.9%
recall gate, and a shared-core crate migration — ~50 board tasks — while:

> **Cortex indexed `Canvas` with 0 of its 115 methods, and `GameObject` with 32 of
> 48.** Reproduced on a clean index, current binary. 131 methods — 26% of everything
> cortex knew about `quartz/src` — were silently absent.

The cause was a known bug fixed in *one* of the two parsers. quartz-ctx repaired
cross-file `impl` attachment with a global second pass
([parser.rs:44-65](src/parser.rs)); cortex still flushed pending impls per file
*and* per inline module, discarding every `impl Canvas` living in
`canvas/actions.rs`, `canvas/physics.rs`, and seven other files.

**Status: fixed and verified — see §4 Phase A.**

The second structural finding is about ownership:

> quartz-ctx and cortex each hold *both* halves of the problem, and each does one
> half badly. quartz-ctx compiles **928 lines of hand-written Quartz judgment**
> into its binary. Cortex maintained **a second Rust parser** that had already
> diverged from quartz-ctx's on a correctness bug.

The clean split — and the answer to "make quartz-ctx truly useful to cortex" — is:

| | Owns | Properties |
|---|---|---|
| **quartz-ctx** | **Structure** — what the code *is* | parsed, refreshable, language-agnostic, rebuildable |
| **cortex** | **Judgment** — what we *learned* | DB-backed, session-fed, project-specific, durable |

---

## 2) Evidence

### 2.1 The extraction defect — measured before and after

Clean index of `quartz/src`, cortex binary, no stale rows:

| Measure | Before fix | After fix | quartz-ctx |
|---|---|---|---|
| `Canvas` methods | **0** | **115** | 115 |
| `GameObject` methods | 32 | **48** | 48 |
| Total methods indexed | 509 | **640** | 640 |
| Typed units carrying methods | 52 / 76 | 53 / 76 | — |
| Types losing methods vs quartz-ctx | 2 (3%) | **0** | — |

Only two types were affected — but they are the two an agent touches in almost every
Quartz task, and the loss cascaded through everything reading the store:

- `get_item` and `recall` returned `Canvas` as a 4-field struct with no callable
  surface at all.
- `semantic_search` ([search.rs](../cortex/src/search.rs)) is TF-IDF cosine over the
  same `compressed` text, so `Canvas`'s vector contained none of its method names —
  no query mentioning `pool_acquire` or `enable_crystalline` could reach it.

**Correction (measured post-fix): the graph was NOT affected.** A/B on the same
source, old vs new binary, produced **identical** edge counts
(366 `derived_from` / 51 `uses` / 5 `implements`). The reason is worth recording:
cortex's `methods:` line stores only method *names* (`new | run | add_plugin`), and
`extract_type_tokens` ([graph.rs:240](../cortex/src/graph.rs)) collects only
uppercase-initial tokens. Snake_case method names never produced `uses` edges in the
first place.

**This strengthens the case for B2.** quartz-ctx's `api-graph.json` carries full
method *signatures* with parameter and return types; cortex's own extractor throws
that away and keeps bare names. Wiring the dormant api-graph path is what would
actually populate the type graph — the 51 `uses` edges for all of `quartz/src` are
the ceiling of what name-only extraction can produce.

### 2.2 Name collisions — cortex answers with the wrong type

`get_item("Canvas")` returns `vr::space_soup::canvas::Canvas` (4 methods), **not**
the Quartz `canvas::core::Canvas`. No disambiguation, no candidate list, no warning.

- **367 colliding names**, affecting **789 of 962 live units**.
- Worst: `Error` ×6; `DistanceConstraint`, `SpringConstraint`, `GrappleConstraint`,
  `Aabb`, `CollisionShape`, `Location`, `Request` ×4 each.

Some is the intentional quartz/synful fork, but the resolver picks arbitrarily.
quartz-ctx solved this with `origin` tags and primary-source precedence
([parser.rs:73-83](src/parser.rs)). **Still open — this is task A2.**

### 2.3 The two extractors had already diverged

`CR-04` lists "duplication of extraction logic creeps into Cortex" as a *future*
risk. It was a present fact, and the two had already split on correctness:

| | quartz-ctx | cortex (before A1) |
|---|---|---|
| Rust parser | `src/parser.rs` (505 LOC) | `src/compressor.rs` (487 LOC) |
| Cross-file `impl` attachment | ✅ global pass | ❌ per-file + per-module flush |
| Visibility | `pub` only | `pub` only |
| Source spans (`file:line`) | ❌ **absent** | ❌ absent |
| Tests covering the parser | **0** | **0** |

Two things fall out:

- **Neither extractor records a file path or line number.** `ApiItem`
  ([model.rs:4-28](src/model.rs)) has `module_path` and no span. Agents cannot emit a
  clickable `file.rs:42` from either tool — and `MULTILANG_LOSSLESS_PLAN.md` §3 lists
  "symbol identity and source span" as requirement #1 of *lossless*.
- **The extraction core was untested in both projects.** quartz-ctx's single
  `#[test]` asserts the hardcoded tick-loop table has 13 steps; it does not touch the
  parser. Cortex's 99 tests covered everything except `compressor.rs`. This is why
  the same bug could be fixed in one project and live on in the other.

### 2.4 The two tools extract the same set — the difference is depth

| Source | quartz-ctx items | cortex units |
|---|---|---|
| `quartz/src` | 117 | 107 |
| `arty/synful_quartz/quartz/src` | 174 | 154 |
| `path_forge/src` | 63 | 58 |
| `vr_workspace/space_soup_engine/src` | 67 | 65 |

Same visibility filter, same item kinds. The 10-item gap on `quartz/src` is consts
and type aliases cortex skips (`NONE`, `PLAYER`, `ENEMY`, … — the collision-layer
constants). quartz-ctx's real advantage is **depth**: full signatures, per-method
docs, field docs, trait lists — none of which cortex stores.

**Consequence for the plan:** Phase B is a *replacement*, not a merge. There is no
private-symbol stratum to preserve.

### 2.5 Cortex's live index is never pruned

The 962 live units are not the 11 configured targets. First path segments include
`cortex` (94 units — cortex indexing itself), `air_src` (35), `src` (35) — sources
absent from `.cortex/index-sources.json`. Indexing is `INSERT OR REPLACE` with no
delete for removed sources, so the store accretes indefinitely. This residue is what
made revision 1's 46% figure look alarming.

### 2.6 Agents have already voted with their tool calls

Last two weeks (`called_at >= 2026-07-21`), and consistent with the full 3,155-call
history:

| Tool | Calls (2wk) | Calls (all) | |
|---|---|---|---|
| `compact_output` | 1,980 | 1,957 | hook-driven |
| `get_preferences` | 10 | 278 | PROTOCOL-mandated |
| `get_anti_patterns` | 10 | 148 | PROTOCOL-mandated |
| `get_delta` | 9 | 214 | PROTOCOL-mandated |
| `get_context` | 7 | 166 | PROTOCOL-mandated |
| **`query_graph`** | **3** | **12** | discretionary |
| **`semantic_search`** | **1** | **29** | discretionary |
| **`get_item`** | **1** | **71** | discretionary |

**Cortex is used as a memory and preferences layer; its code-intelligence tools are
effectively unused.** Every one is a discretionary call an agent makes only when it
expects a useful answer. Given §2.1 and §2.2, that was rational.

*(Caveat: this counts cortex calls only — quartz-ctx keeps no telemetry, so this
shows cortex's internal ranking, not a head-to-head against quartz-ctx.)*

This is decisive for the embedded board: `CCTX-0011/0012/0013/0030/0031/0032` add
**eight new `ctx_*` tools** to a surface whose three existing equivalents were called
five times in two weeks. `CR-05` ("tool sprawl reduces adoption") is already
happening. More tools is the wrong lever.

### 2.7 The integration the plans propose is already half-built and dormant

`CORTEX_EMBEDDED_..._PLAN.md` §5 recommends "Strategy 1: quartz-ctx writes canonical
JSON, Cortex reads it," and `CCTX-0010` is "implement read-only loader."

The loader exists:
- quartz-ctx emits `api-graph.json` (`generate --minimal`) — verified working, 117
  items with `methods`, `fields`, `variants`, `traits_impl`, `doc`, `signature`.
- cortex has `ApiGraphItem` ([model.rs:217](../cortex/src/model.rs)),
  `compress_api_graph` ([compressor.rs:59](../cortex/src/compressor.rs)), an
  `--api-graph` CLI flag, and merge logic at
  [main.rs:2159-2163](../cortex/src/main.rs) whose comment reads *"api-graph items
  take precedence (they have richer doc)."*

**Nothing calls it.** No `--api-graph` anywhere in `.cortex/`. CM1 is not a milestone
to build; it is a wire to connect.

Two tables sized for the plans' contracts are also empty:

| Table | Rows | Intended by |
|---|---|---|
| `content_store` (hash, content, ref_count) | **0** | Contract A compact/expand; `CCTX-0031` |
| `call_graph` (caller, callee, edge_type, file, line, weight) | **0** | `CCTX-0030`; `QCTX-0032` |

### 2.8 The learning loop is idle, not flooded

| Signal | Value |
|---|---|
| Proposals total | 1,307 — **1,305 created in one burst on 2026-07-16 04:17** |
| Proposals since that burst | **2** (1 drift digest + 1 meta note), from the 04:43 run |
| Accepted or pending | **0** |
| Pipeline runs since 2026-07-16 | **0** |
| Patterns ever used | **8 of 149 (5%)** |
| `pattern_unit_refs` | 0 |

The drift digest-aggregation fix **works** — one digest per run, as documented. The
problems are different from revision 1's claim: the pre-fix backlog was never purged,
so the meta-analyzer reports "Type 'drift_flag': 0/1304 approved (0%) — consider
adjusting gate thresholds" computed over dead rows, and that self-diagnosis was
itself auto-rejected. And the pipeline has not run in 2.5 weeks.

Routing new telemetry (`CCTX-0021`) into a pipeline that is not running adds nothing.

### 2.9 Three systems, three different worlds

| System | Scope | Nodes/units |
|---|---|---|
| graphify | **20 projects**, incl. cortex (843) and quartz-ctx (183) | 7,186 nodes |
| cortex | 11 configured targets + unpruned residue | 962 units |
| quartz-ctx (VS Code) | 3 roots | ~354 items |
| quartz-ctx (**Claude Code**) | **1 root** | 117 items |

Two live config bugs:

1. **`.mcp.json` gives quartz-ctx only `quartz/src`**, while `.vscode/mcp.json` gives
   it three roots. Confirmed empirically: `get_item("GrappleConstraint")` in this
   session returns `origin: quartz` alone, with no synful variant. Same tool, two
   hosts, different ground truth.
2. The graphify entry in `.vscode/mcp.json` advertises "10,529 nodes, 16,141 edges,
   1,357 communities". Live `graph_stats`: **7,186 / 10,025**.

*(`graph_stats.community_count` reporting 0 is the known documented bug, not new.)*

### 2.10 quartz-ctx's hardcoded knowledge is its one un-generalizable part

| Module | LOC |
|---|---|
| `patterns.rs` | 275 |
| `anti_patterns.rs` | 200 |
| `examples.rs` | 160 |
| `timing.rs` | 154 |
| `behavior.rs` | 139 |
| **Total** | **928 of 4,120 (23%)** |

This duplicates what cortex owns *dynamically* — 128 anti-patterns, 149 patterns, 55
annotations, 9 ADRs — kept current from live sessions by the `[CORTEX-*]` marker
system. The quartz-ctx copy updates only by editing Rust and recompiling, and is
already gated off for non-Quartz engines (`is_quartz`, [mcp.rs:123](src/mcp.rs)).

**Moving those 928 lines into cortex is the single change that most improves both
projects at once:** quartz-ctx becomes genuinely language-agnostic and reusable on
any Rust codebase; cortex becomes sole owner of learned judgment — the role its
marker/closeout machinery was built for.

---

## 3) Keep / cut / defer

### Keep
- Canonical `Symbol` schema with **visibility, source span, stable ID, provenance,
  confidence** (`QCTX-0001`). The span field is a real present gap.
- Contract A: compact-first with **deterministic exact-expand handles**;
  `content_store` is already the right home.
- Coverage and confidence as *reported metadata* (`QCTX-0003`, `CCTX-0002`).
- Strategy 1 (shared artifacts) over Strategy 2 — correct, and cheaper than the
  document realises because it is already built (§2.7).

### Cut
- **`CCTX-0011/0012/0013/0030/0031/0032` as eight new `ctx_*` tools.** Fix the three
  that exist. §2.6 is the argument.
- **`CCTX-0020` / `QCTX-0035` fail-closed protocol mode.** A coverage gate would have
  passed happily while `Canvas` had zero methods — the metric it gates on
  (unit counts, parse success) was green throughout. Fail-closed protects against the
  wrong failure mode; correctness tests (§4 A3) protect against the real one.
- **`CCTX-0040/0041` feature-gated dual-mode shared core.** One crate, no gates.
- **`QCTX-0040/0041` C# and Java/Kotlin.** No consumer in FlowMake.
- **LSP / compiler-metadata enrichers.** Their entire justification is resolving
  cross-file semantics, which for Rust is what A1 already delivers with `syn`.
- **`QCTX-0025` recall/precision/F1 gates and `QCTX-0050` incremental-indexing SLO.**
  No golden suite exists, and a full reparse of `quartz/src` takes ~2 s.

### Defer
- `CCTX-0021` gap-pipeline routing — until the pipeline runs again (§2.8).
- Multi-language extraction — after A–C, via **tree-sitter AST only**, tagged
  `confidence: ast_only`. TS/JS and Python only. No reconciler, no semantic backends.

---

## 4) The plan

### Phase A — Make the existing Rust truth correct

| ID | Task | Status |
|---|---|---|
| **A1** | Global cross-file `impl` attachment in cortex's compressor | ✅ **done, deployed** |
| **A2** | Scope disambiguation on `get_item` | ✅ **done, deployed** |
| **A3** | Regression tests for the extraction core | ✅ **done** |
| **A4** | Reconcile `.mcp.json` with `.vscode/mcp.json`; fix stale graphify description | ✅ **done** |
| **A5** | Prune index residue | ✅ **done, deployed** |

**A1 as implemented** ([compressor.rs](../cortex/src/compressor.rs)):
- `compress_dir` accumulates `PendingImpl`s across the whole tree and calls a new
  `attach_impls` global pass; the per-file and per-`mod` flushes are gone.
- Nine impl blocks collapse into **one deduped `methods:` line**, not nine partial ones.
- `PendingImpl` now records its own `module_path`; when several indexed types share a
  name, the impl attaches to the candidate with the **longest common module-path
  prefix** — so `editor::State` cannot absorb `engine::State`'s methods. (quartz-ctx's
  own global pass takes the first name match and is still vulnerable here.)
- Term vectors are rebuilt for every augmented unit, so semantic search scores against
  the post-attach text.

**A3 as implemented** — 4 unit tests + 1 workspace gate, in the previously untested
`compressor.rs`:
- `impls_in_other_files_attach_to_their_type` — the core regression
- `repeated_impl_blocks_produce_one_deduped_methods_line`
- `same_named_types_attach_by_module_proximity`
- `term_vector_reflects_attached_methods`
- `gate_a_real_quartz_canvas_has_its_methods` (`#[ignore]`, runs against `../quartz/src`,
  asserts `Canvas` ≥ 115 and `GameObject` ≥ 48, and prints coverage metrics)

**Verified:** `cargo test --bin cortex` → 97 passed, 0 failed.
`cargo test --bin cortex -- --ignored gate_a` → passes, reporting
`107 units | 76 typed, 53 with methods (70%) | 640 methods total | Canvas 115`.

**Deployed 2026-08-03** (stop cortex MCP processes → `cargo build` → `cortex.ps1 reindex`;
the binary is locked while the MCP server runs, so the stop is mandatory).

A/B on `quartz/src`, same source, old vs new binary:

| | Before | After |
|---|---|---|
| Methods indexed | 509 | **640** (+131) |
| `Canvas` | 0 | **115** |
| `GameObject` | 32 | **48** |
| Graph edges | 366/51/5 | 366/51/5 (unchanged — see C6) |

Live index across all 11 targets after reindex — 966 units, **2,621 methods**:

| Unit | Methods |
|---|---|
| `canvas::core::Canvas` | **115** (was 0) |
| `synful::canvas::core::Canvas` | **164** (was 0) |
| `object::GameObject` | **48** (was 32) |
| `synful::object::GameObject` | **44** |

Fourteen further types had their text rewritten with identical method counts — the
dedup collapsing multiple partial `methods:` lines into one. No count regressed.

**A2 as implemented** ([tools.rs](../cortex/src/mcp/tools.rs), [mcp/mod.rs](../cortex/src/mcp/mod.rs)):
- New `resolve_candidates(name, scope, units)`. An exact unit id is definitive;
  otherwise every match is returned, ranked.
- **Ranking: primary engine first.** Scoped sources carry their scope on the module
  path, so the shallowest path is the unscoped primary. Ranking on richness alone
  handed `Canvas` to the synful fork purely because it has 164 methods to quartz's
  115 — the opposite of documented precedence. Depth, then richness as a tie-break
  between equally shallow projects, then id for determinism.
- `get_item` gained an optional `scope` argument, and when a name is ambiguous it
  now **lists every alternative with its id, kind and method count** instead of
  silently returning one.
- An unmatched scope errors rather than falling back to another project.

Verified over real MCP stdio against the live index:

| Call | Result |
|---|---|
| `get_item("Canvas")` | `canvas::core::Canvas` + both alternatives listed |
| `get_item("Canvas", scope="synful")` | `synful::canvas::core::Canvas` |
| `get_item("canvas::core::Canvas")` | exact, no ambiguity block |
| `get_item("Canvas", scope="path_forge")` | clean error, no silent fallback |

**A5 as implemented** ([memory.rs](../cortex/src/memory.rs), [main.rs](../cortex/src/main.rs)):
- New `source_root` column on `code_units` (idempotent migration), stamped at index
  time — the provenance field `QCTX-0001` calls for, arriving early because pruning
  needs it.
- New `cortex prune-index --keep <root> [--apply]`. Reports by default; refuses to
  run with no `--keep` roots; deletes orphaned units with their members, nodes and
  edges. 3 tests.

Applied to the live index after a full reindex (DB backed up first):

| | Before | After |
|---|---|---|
| Units | 966 | **547** |
| Orphans removed | — | **419** |

All 419 were confirmed stale before deletion: nothing indexed since 2026-06-29, and
**zero id overlap with live units**. They were the dead `vr::` scope (128, superseded
by `space_soup`/`ss_engine`/…), cortex's own source (94), `air_src` (35), `src` (35),
and old quartz modules since renamed. The dead `vr::` duplicates are exactly what
`get_item("Canvas")` was returning.

**Deployment trap found the hard way:** `response_cache` is keyed on
`compute_index_version`, which hashes only `code_units(id, indexed_at)`. A rebuilt
binary therefore replays the **old** tool output while the index is unchanged — the
verified A2 fix looked broken over MCP until three cached rows were cleared. Full
working deploy sequence:

```
stop cortex processes → cargo build → cortex.ps1 reindex → DELETE FROM response_cache
```

A proper fix mixes a build id or tool-schema hash into the cache key; logged as a
follow-up, not done here.

**Gate A: closed.**

### Phase B — One extractor, two consumers

| ID | Task | Status |
|---|---|---|
| **B2** | Wire the **existing** api-graph path into `reindex` | ✅ **done, deployed** |
| **B1** | Add `visibility` + `span (file, line)` to quartz-ctx's `ApiItem` | ✅ **done** — delivered as G4/G5 |
| B3 | Delete cortex's `CompressVisitor` Rust parsing; depend on quartz-ctx as a library | open |
| B4 | Populate `call_graph` from the extractor | open |

Simpler than revision 1 planned: per §2.4 this is a replacement, not a two-stratum merge.

**B2 as implemented.** Two hazards had to be fixed before the dormant wire could be
switched on — both would have silently corrupted the index:

1. **Scope collision.** quartz-ctx ids carry no scope. Ingesting the synful fork
   unscoped emits `canvas::core::Canvas`, which collides with the primary engine's
   id — and since persistence is `INSERT OR REPLACE`, it would have **silently
   overwritten Quartz's `Canvas` with synful's**. `compress_api_graph` now takes the
   scope and applies it to module path and id.
2. **Kind vocabulary split.** quartz-ctx spells kinds `Struct`/`Enum`/`Function`;
   cortex and every downstream `kind` filter use `struct`/`enum`/`fn`. Ingesting raw
   would have produced a two-dialect index. Added `normalise_api_kind`, applied to
   the unit, the rendered header and the summary.

`.cortex/cortex.ps1 reindex` now generates an api-graph per target before indexing
and passes `--api-graph`. Its context dir is keyed on scope, because `quartz/src` and
`arty/synful_quartz/quartz/src` both slugify to `quartz` and would otherwise overwrite
each other's graph. If quartz-ctx is not built, generation degrades to cortex-only
extraction rather than failing the reindex. Artefacts land in `.cortex/apigraph/`
(gitignored, regenerated every run).

**Id agreement was exact** — every source reported 100% replacement
(107/107 quartz, 154/154 synful, 58/58 path_forge, …), confirming the two extractors
derive identical ids.

Live index, before and after B2:

| | Pre-B2 | Post-B2 |
|---|---|---|
| Units | 547 | **657** (+110 consts/type-aliases cortex's parser skips) |
| `uses` edges | 846 | **1,627** |
| Indexed text | 112,931 chars | **304,121 chars (2.7x)** |
| `canvas::core::Canvas` | 2,031 chars, bare names | **10,202 chars, full signatures + docs** |
| `synful::canvas::core::Canvas` | — | 16,032 chars, 164 methods |

On `quartz/src` alone, A/B against the A1-only index:

| | A1 only | B2 |
|---|---|---|
| `uses` edges | 51 | **371 (7.3x)** |

**This closes C6.** The 51-edge figure was the ceiling of name-only extraction;
quartz-ctx's signatures carry parameter and return types, so the type graph finally
has something to infer from. The graph was the one thing A1 could not improve, and
B2 improved it 7.3x without touching the inference code.

Verified: `cargo test --bin cortex` → 108 passed, 0 failed (3 new tests covering both
hazards); Gate A green; `get_item` over real MCP stdio returns the scoped, signature-rich
records.

### Phase G — Generality: index any project, not just configured Rust roots

Added 2026-08-03 after the stated goal moved to *"quartz-ctx should scrape and
index any future project."* Measured status of what actually blocks that:

| ID | Blocker | Status |
|---|---|---|
| **G1** | Root list hardcoded per host, drifting in three places | ✅ **done** |
| **G2** | No ignore rules — scanning a project root ingests build output | ✅ **done** |
| **G4** | **`pub`-only**: an application exposes little; quartz-ctx returned near-nothing for a typical app | ✅ **done** |
| **G5** | No source spans, so no `file:line` citations | ✅ **done** |
| G3 | **Rust only** (`syn::parse_file`); any non-Rust project yields 0 items | open — this is Phase E |
| G6 | No workspace discovery — you must know each crate's `src` dir | open |

**G1 as implemented.** `serve` gained `--sources-from <manifest>`, reading the same
`{ "targets": [ { "source", "scope" } ] }` shape as `.cortex/index-sources.json`.
One file now drives both the indexer and the API server, so they cannot drift.
Explicit `--source` roots load first and suppress their manifest duplicate
(separator- and case-insensitively), so `quartz/src` stays the primary engine.

A manifest `scope` becomes the origin tag **verbatim** — not slugified. The first
cut ran it through `slugify`, which rewrites `_` to `-` and turned cortex's
`path_forge` scope into origin `path-forge`, breaking the very correspondence the
feature exists for.

Result: quartz-ctx's own MCP server went from **3 roots to all 11**, loading
**657 items — exactly matching cortex's 657 units**, with origins equal to cortex
scopes (`synful`, `path_forge`, `ss_engine`, …).

**G2 as implemented.** `parse_dir` now prunes excluded directories with
`filter_entry` (pruning the tree, not filtering files): `target`, `node_modules`,
`vendor`, `dist`, `build`, `out`, `.git`, `.venv`, `venv`, `__pycache__`,
`.next`, `.svelte-kit` and friends. An explicitly requested root is never excluded,
so `--source ./target` still works when deliberate.

This was found by measurement, not review: pointing quartz-ctx at the
`quartz_forge` **project root** reported **1,239 items** against a true API surface
of 99, because it was reading `target/debug/build/*/out/*.rs` — generated GL
bindings. `MULTILANG_LOSSLESS_PLAN.md` §11 lists "exclude generated and vendor
directories by default" as an operational default; it had never been implemented.

| Source | Before | After | `src` dir |
|---|---|---|---|
| `quartz_forge` (root) | 1,239 | **99** | 99 |
| `cortex` (root) | 733 | **198** | 198 |

**Pointing at a repository root now gives exactly the same answer as pointing at its
`src`** — which is the behaviour "index any future project" requires.

`selfcheck` was reporting a file count from its own unfiltered walk (43 files while
the parser read 22). It now calls `parser::count_source_files`, so the diagnostic
and the parser agree.

**G4 as implemented.** `Visibility` (`Public` / `Crate` / `Restricted` / `Private`)
is now recorded on every item and method rather than used to silently drop them,
and `ParseOptions { include_private }` selects the view. Default stays `pub`-only,
so a library's indexed surface remains the API it actually promises.

The gap this closes, measured across project types:

| Project | Kind | `pub`-only | with private | shown before |
|---|---|---|---|---|
| `quartz/src` | engine / library | 117 | 214 | 55% |
| `cortex/src` | application | 198 | 598 | 33% |
| `quartz_forge/src` | GUI application | 99 | 511 | **19%** |
| `ball_swing_game/src` | game | 764 | 1012 | 75% |

Libraries publish their structure; applications do not. quartz_forge was showing
**19% of itself** — which is why this ranked ahead of multi-language support.

**The policy is per root, not per server.** A manifest target may set
`"include_private": true`, so one server can hold a library at its API surface and
an application at its full structure simultaneously — verified serving `quartz/src`
(117, library view) and `quartz_forge/src` (511, project view) together as 628
items. `mcp::serve` carries the flag so the 5-second auto-reload preserves each
root's view rather than silently reverting apps to `pub`-only.

**G5 as implemented.** Every item and method carries
`SourceSpan { file, line }` — relative to the scanned root, forward slashes, so it
reads identically on every platform. Enabled by turning on proc-macro2's
`span-locations` feature.

The first cut cited the wrong line: a `syn` item span starts at its **first
attribute**, so `#[derive(Clone)] pub struct Canvas` reported line 114 (the derive)
rather than 115 (the declaration). Spans now come from the declaration's
identifier. Verified against source:

| Unit | Span | Line content |
|---|---|---|
| `canvas::core::Canvas` | `canvas/core.rs:115` | `pub struct Canvas {` |
| `object::GameObject` | `object/mod.rs:18` | `pub struct GameObject {` |
| `types::action::Action` | `types/action.rs:14` | `pub enum Action {` |

Both fields flow through to cortex: `ApiGraphItem` gained optional `visibility`
and `span`, and the compressed record now opens with an `at: file:line` line.
**100% of the 657 live units carry a citable span.**

**The honest remaining limit:** G3. quartz-ctx is a *Rust* code-intelligence tool
that now works on any Rust project — library or application, pointed at a repo root,
without config. It is not yet a general one: a TypeScript or Python project still
yields zero items, silently. Until Phase E lands, "any future project" means "any
future Rust project."

### Phase S — API sheets worth publishing

Added 2026-08-04. `generate` already produced a complete *reference* — every type,
variant, method and signature, in 0.4 s. What it lacked was worked syntax,
citable locations, and any signal about what is undocumented.

| ID | Task | Status |
|---|---|---|
| **S1** | Surface rustdoc ` ```rust ` fences as Example blocks | ✅ **done** |
| **S2** | Harvest real call sites from examples, tests and benches | ✅ **done** |
| **S3** | Spans and a doc-coverage report in the sheets | ✅ **done** |

**S1.** `split_doc` separates prose from fenced code blocks and strips rustdoc's
hidden-line `# ` prefix. Prose renders as description, fences render as
**Example**; previously the whole comment was flattened into one blob and the
fences were lost as runnable syntax.

**S2.** New `usage` module mines call sites with `syn` spans, so a multi-line
builder chain arrives intact instead of clipped at a line boundary. Sources are
discovered beside the source root — `examples/`, `tests/`, `benches/`,
`example.rs`, `main.rs` — plus the source tree's own `#[test]` bodies.

**The first version was worse than useless and measurement caught it.** Mining
the implementation tree wholesale produced 72 "documented" items — but `Canvas`'s
examples were its own `Debug` impl body and a trait's default method: the API's
internals presented as usage. Two rules fixed it:

- the implementation tree contributes **only** `#[test]` / `#[cfg(test)]` bodies;
  example and test directories contribute every statement
- a nested item definition (`fn`, `struct`, `impl`) is never a call site

Yield dropped from 72 to 12 for `quartz/src`, and every remaining snippet is a
real call — `Canvas::new(ctx, CanvasMode::Landscape)` cited to
`quartz/example.rs:33`. **A lower number that is true beats a higher one that is
not.** Where a project has tests, the yield is much higher: `cortex/src` gets 50
items from `#[test]` bodies alone.

**S3.** Every item now renders `defined at file:line`, non-public items are
labelled with their visibility, and `INDEX.md` carries a coverage report naming
the specific undocumented items.

Measured on this workspace — and the numbers are the point, since they say where
to spend documentation effort:

| | `quartz/src` | `cortex/src` |
|---|---|---|
| Items with a doc comment | 25 / 117 (21%) | 114 / 199 (57%) |
| Fields/variants/methods documented | 206 / 1006 (20%) | 75 / 455 (16%) |
| Items with worked syntax | 14 / 117 (12%) | 51 / 199 (26%) |

Generation stays fast: **~3.5 s** for `quartz/src` including the usage harvest
(0.4 s without it).

**What still limits sheet quality is the source, not the tool.** 92 of Quartz's
117 public items have no doc comment at all — the sheets now name every one of
them, so the gap is actionable rather than invisible.

### Phase C — Move judgment to where judgment lives

| ID | Task | Status |
|---|---|---|
| **C1** | Migrate the hardcoded knowledge into cortex's DB | ✅ **done** |
| **C2** | Delete the curated modules and the `is_quartz` gate | ✅ **done** |
| C3 | Wire `content_store` for compact/expand handles | open |
| **C4** | State the structure/judgment split once, in `CLAUDE.md` | ✅ **done** |

**C4 mattered more than its size suggests.** After C2, `CLAUDE.md` still routed
agents to nine tools that no longer existed and claimed "curated Quartz-knowledge
tools auto-disable when the engine name isn't Quartz" — a gate that had been
deleted. An operating manual describing a tool surface that is gone misdirects
every future session. It now states the structure/judgment split, records where
the curated knowledge went, and documents the manifest-driven roots,
`--include-private`, and `generate`.

**C1 as implemented.** A temporary `export-knowledge` subcommand dumped every
curated entry as structured JSON, so the migration was mechanical rather than
hand-transcribed. That mattered: a first pass enumerated example keys by hand and
silently missed `get_builder_examples`, which has its own entry point outside
`get_all_examples()`. Using the module's own enumerators caught it.

**65 entries exported → 24 new in cortex, 41 already present.** The 63% overlap is
the duplication thesis confirmed by measurement. Every one of the 41 skips was
audited against its best cortex match before writing — all genuine duplicates
(scores 0.60–1.00, e.g. quartz-ctx's *"Importing from prism directly…"* against
cortex's *"Importing quartz types directly from crystalline or prism crates"*), no
false positives.

| Table | Before | After |
|---|---|---|
| `anti_patterns` | 128 | **140** |
| `patterns` | 149 | **157** |
| `annotations` | 55 | **65** |

Written by direct SQLite INSERT — the FTS mirrors are trigger-maintained, so
search stayed in sync (verified table/FTS counts equal) and the CLI's
single-line-argument constraint never applied. The export JSON is kept at
`migration/curated-knowledge-2026-08-04.json` as provenance.

**C2 as implemented.** Rather than deleting all twelve gated tools, the audit
split them by whether their answer was derivable from parsed source:

*Converted to computed (now work on any Rust project):*
- `get_trait_implementations` — reads parsed `traits_impl`, and computes which
  indexed traits a type does **not** implement by set difference.
- `get_builder_methods` — discovers `<T>Builder`, then classifies methods as
  chainable (`-> Self`) or terminal (`finish`/`build`). On `GameObject` it now
  finds every builder method automatically, where before it returned one
  hand-written example.
- `get_return_type_usage` — was already reading parsed items but relied on a
  3-entry hardcoded borrow table; now derives return type and borrow semantics
  from the signature itself.

*Deleted (pure Quartz judgment, now in cortex):* `get_code_examples`,
`check_anti_patterns`, `validate_physics_config`, `check_lifetime_constraints`,
`suggest_action_for_intent`, `get_tick_loop_order`, `explain_behavior`,
`get_usage_patterns`, `get_engine_constants`.

Files removed: `anti_patterns.rs`, `patterns.rs`, `examples.rs`, `behavior.rs`,
`timing.rs`; `helpers.rs` shrank from 261 lines to just `find_related_apis`
(which also gained ranking — it previously returned unranked matches, so a
doc-comment mention could outrank the type actually named).

**The `is_quartz` gate is gone entirely.** Every remaining tool is computed, so
there is nothing left to gate on engine name.

| | Before C2 | After |
|---|---|---|
| Source lines | 4,120 | **3,267** (−853, −21%) |
| Tools on a **non-Quartz** project | 6 | **9** |
| Tests | 13 | **17** |

**A bug caught by testing the conversion, not by review:** the new borrow
classifier reported `Option <& mut GameObject>` as *"an owned value: no borrow
outlives the call"*. `quote!` renders types with spaces, so the `Option<&mut`
test never matched. That is the exact misinformation that produces double-borrow
panics — the failure mode cortex already has an anti-pattern for. Fixed by
despacing before classification, with three regression tests.

### Phase D — Restart the learning loop

| ID | Task | Status |
|---|---|---|
| **D1** | Purge the pre-fix backlog; stop meta-analysis scoring dead rows | ✅ **done** |
| **D2** | Re-run `consolidate-pipeline` (idle since 2026-07-16) | ✅ **done** |
| **D3** | Retrieval-outcome telemetry | ✅ **diagnosed and addressed** |
| D4 | Route code-intelligence misses into `query_gap_log` (`CCTX-0021`) | open — now unblocked |

**D1.** Purged 1,305 proposals from four runs at 2026-07-16 04:17, keeping the 2
from 04:43 as evidence the digest-aggregation fix works. Checked what depended on
them first: `gate_survival_trend` only looks back 7 days, and `gate_duplicate`
matches `content_hash` across *all* statuses — so a dead rejected row was
permanently blocking that content from ever being proposed again. Purging is
corrective, not merely tidy.

**The backlog was a symptom.** `analyze_threshold_impact` scored *all history*,
so one defective minute set `drift_flag` to 0% approved permanently, and the
analyzer kept proposing threshold changes derived from a bug that had already
been fixed — then auto-rejected its own diagnosis. Now windowed to 30 days, with
three tests: stale rejections age out, genuinely poor recent performance still
alerts, and too few recent samples stays silent.

**D2.** Pipeline re-run after three weeks idle: 20 snapshots → 6 clusters → 3
skill candidates, **0 proposals staged, meta alerts empty**. Two skill candidates
are already approved (`verify-ui-layout-by-rendering`, `workflow-knowledge-lookup`).

**D3 — the diagnosis inverted the assumption.** The telemetry was **not** broken:
35 of 36 outcomes had been applied, and the one unapplied outcome had no pattern
retrievals to score. The bottleneck was upstream:

| | |
|---|---|
| `list_patterns` calls | 119 |
| …that passed a `hint` | **3 (3%)** |
| Pattern touches logged | 3,036 |
| …that counted as *targeted* | 32 |
| Distinct patterns ever targeted | 37 of 160 |

Only hint-matched rows count, and that is **correct** — crediting an untargeted
full-index listing would credit all 160 patterns on every call, recreating the
vacuous signal the design exists to avoid. So the fix is compliance, not scoring.
`CLAUDE.md` states "Always pass `hint`" twice and it was followed 3% of the time,
so the consequence now appears at the call site: a hintless `list_patterns`
response says it recorded no usage signal and why.

**Resisting the tempting fix mattered here.** Counting the 3,036 untargeted
touches would have moved "patterns with usage" from 10 to 160 overnight and made
the metric meaningless.

### Phase E — Multi-language, rescoped
TS/JS + Python via tree-sitter, AST-only, into the same canonical schema; coverage
reported, never gated. Success measure: an agent in `scene_editor_web` gets the same
answer quality it gets in `quartz/src`.

---

## 5) Why this makes quartz-ctx genuinely useful *to cortex*

1. **quartz-ctx becomes cortex's extraction engine, replacing one that was losing 131
   methods** — including every method on the engine's central type. Not an additional
   source: a replacement.
2. **cortex becomes quartz-ctx's knowledge store, replacing hardcoded Rust.** The 928
   curated lines stop being a recompile-to-update liability and become learnable rows.
3. **Each side gives away what it is worse at.** That is why this is worth doing, and
   it is a stronger claim than either original document makes.

## 6) What this changes about the four documents

- `MULTILANG_LOSSLESS_PLAN.md` — Phases 0/1 become Phases A/B here. Phases 2–5 defer
  to E, minus LSP, C#, Java and the numeric gates.
- `MULTILANG_EXECUTION_BOARD.md` — `QCTX-0001/0003` keep; `QCTX-0025/0035/0040/0041/0050`
  cut or defer. Missing entirely: source spans, and parser regression tests.
- `CORTEX_EMBEDDED_CODE_INTELLIGENCE_PLAN.md` — Option C is right; §5 Strategy 1 is
  already implemented and dormant; `CR-04` described as future risk was present fact.
- `CORTEX_EMBEDDED_EXECUTION_BOARD.md` — CM0/CM1 largely exist as code. CM2's
  fail-closed policy is cut (§3). CM3's eight tools collapse to fixing three.

## 7) Re-verification

```bash
cargo test --bin cortex -- --ignored gate_a --nocapture
```

- Canvas contrast: cortex `get_item("Canvas")` vs quartz-ctx `get_item("Canvas")`.
- Per-source counts: `quartz-ctx.exe selfcheck --source <path> --json`.
- Clean-index baseline: `cortex.exe --db <scratch>.db index --source quartz/src --name X`.
- Tool adoption: `select tool,count(*) from mcp_calls where called_at>='2026-07-21' group by 1 order by 2 desc`.
- Proposal cadence: `select created_at,count(*) from proposals group by 1`.
- Empty contract tables: `select count(*) from content_store; select count(*) from call_graph;`
- Index residue: first path segment of `module_path` vs `.cortex/index-sources.json`.

## 8) Next move

Phase A, B2, G1 and G2 are complete and deployed.

Phase A, B2, G1, G2, G4 and G5 are complete and deployed. quartz-ctx now indexes
any Rust project — library or application, from a repo root, with citable
`file:line` on every symbol — and is cortex's extraction engine for all 11 sources.

Recommended order from here: **C1/C2 → G6 → E (tree-sitter)**.

**C1/C2** completes the trade the ADR describes: move quartz-ctx's 928 hardcoded
lines of Quartz judgment into cortex's DB and delete the `is_quartz` gate. Cortex
has now given quartz-ctx nothing and gained an extractor, spans and visibility;
this is the half that pays it back, and it is what makes quartz-ctx's tool surface
uniform on a non-Quartz project.

**G6** (read `Cargo.toml` workspace members and index every crate automatically)
is what turns "point it at a repo" into "point it at a workspace" — the last
config-free step before language support.

**E** stays last. It is the headline feature, but everything above makes
quartz-ctx useful on real projects sooner and at far lower risk.

 quartz-ctx is now cortex's extraction
engine for every configured source: cortex's own parser survives only as the
fallback when quartz-ctx is unbuilt, and as the source of the private-symbol members
that feed `derived_from` edges.

The natural next step is **C1/C2** — moving quartz-ctx's 928 lines of hardcoded
Quartz judgment into cortex's DB and deleting the `is_quartz` gate. That completes
the trade: cortex has already given up nothing and gained an extractor; quartz-ctx
gives up knowledge it cannot keep current and becomes reusable on any Rust codebase.

**B3 is now much cheaper** than when it was written. With ids proven identical and
100% replacement on every source, deleting `CompressVisitor`'s unit extraction is
mostly a deletion — though its `members` output must stay, since `derived_from`
edges and `code_members` depend on it and the api-graph carries no member records.

Follow-ups logged, not done:
- `response_cache` should mix a build id into its key (see the deployment trap in §4).
- quartz-ctx's own global impl pass takes the first name match; it should adopt
  cortex's module-proximity tie-break (A1).
- `call_graph` and `content_store` remain empty (B4, C3).
