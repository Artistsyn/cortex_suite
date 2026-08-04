# cortex

Persistent semantic memory and self-learning layer for Copilot. Compresses your codebase
into dense representations, accumulates knowledge across sessions, runs an 8-stage
consolidation pipeline to surface patterns and proposals, and serves it all as a live MCP
skill — so Copilot spends fewer tokens, asks smarter questions, and gets smarter over time.

## Architecture

```
your source ──► compressor ──► SQLite index (.cortex/memory.db)
                                        │
             patterns, anti-patterns ───┤
             annotations, call log  ───┤
             session snapshots      ───┤
             proposals table        ───┤
                                        │
                                   MCP server (22 tools)
                                        │
                                   Copilot Chat

Self-learning pipeline (runs on staleness or 5+ new sessions):
  1. health-check → 2. cluster-sessions → 3. detect-skills
  → 4. propose-gaps → 5. propose-survival → 6. fidelity-scoring
  → 7. graph-drift → 8. meta-analysis
  → proposals table → human review → meta apply
```

Nothing gets written to memory without your explicit approval.
Meta-proposals can only target `prefs.toml` and `copilot-instructions.md` — never source code.

---

## Setup

```sh
cargo build --release

# 0. First-time workspace bootstrap
cortex bootstrap --repo . --source src --name MyProject
#   Creates: .cortex/cortex.ps1, .cortex/prefs.toml, .vscode/mcp.json

# Validate
.\.cortex\cortex.ps1 mcp-ready -SelfCheckFormat json
.\.cortex\cortex.ps1 smoke -SelfCheckFormat json

# 1. Index your source
cortex index --source src --name MyProject

# 2. Start the MCP server (VS Code picks it up from .vscode/mcp.json)
cortex serve --source src --name MyProject
```

### MCP Readiness (required before PROTOCOL sessions)

```powershell
.\.cortex\cortex.ps1 mcp-ready -SelfCheckFormat json
```

Required MCP baseline tools: `get_delta`, `get_preferences`, `get_anti_patterns`,
`list_patterns`, `get_context`. If any fail, run:

```powershell
.\.cortex\cortex.ps1 doctor --format json
.\.cortex\cortex.ps1 serve
# Reload VS Code window, then re-probe
```

---

## MCP Tools (22 total)

| Tool | Purpose |
|------|---------|
| `semantic_search` | Find anything related to a concept |
| `get_item` | Full details of a type/function |
| `get_context` | Pre-compiled context packet for a task |
| `get_delta` | Changes since last checkpoint |
| `get_preferences` | Style rules and API notes |
| `get_anti_patterns` | All known bug traps |
| `list_patterns` | Approved implementation patterns |
| `recall` | Semantic search across all memory |
| `suggest_pattern` | Queue a pattern for review |
| `query_graph` | Cross-crate dependency queries |
| `simulate_change` | Preview impact of a code change |
| `recurrent_think` | 4-dimensional iterative hypothesis refinement |
| `begin_protocol_session` | Activate PROTOCOL mode + Phase 0 gating |
| `get_session_health` | One-call health: Phase 0 status, gaps, proposals |
| `flush_knowledge_markers` | Extract CORTEX-* tags from session turns |
| `closeout_session` | Complete session closeout (Tier 1/2 commit model) |
| `propose_skill` | Stage a skill candidate for review |
| `get_syntax` / `get_usage_examples` / `get_helper` / `list_all` / `explain_dependency_path` | Code lookup |

Delta controls: `include`, `exclude`, `max_files`, `max_patch_lines`

---

## CLI Commands

### Core

```sh
cortex index --source src            # Index / re-index source
cortex serve --source src            # Start MCP server
cortex watch --source src            # Watch for changes (never auto-approves)
cortex review                        # List pending observations
cortex crystallize <id> --name ...   # Promote observation to pattern
cortex dismiss <id>                  # Discard observation
cortex status                        # DB stats
cortex --format json status --full   # Machine-readable full status
```

### Knowledge Management

```sh
cortex pattern list / add / remove
cortex anti-pattern list / add / remove
cortex annotate list / add / remove
cortex recall <topic>
cortex adr new --title "..." --context "..." --decision "..."
cortex correction --attempted "..." --reason "..." --fix "..."
```

### Self-Learning Pipeline

```sh
# Run full pipeline (health-check → cluster → skills → gaps → survival → fidelity → drift → meta)
.\.cortex\cortex.ps1 consolidate-pipeline

# Run only if > N hours stale (default 8) OR >= 5 new sessions
.\.cortex\cortex.ps1 consolidate-if-stale [-StalenessHours N]

# Individual stages
.\.cortex\cortex.ps1 cluster-sessions    # Cluster session snapshots by tool-sequence similarity
.\.cortex\cortex.ps1 detect-skills       # Draft SKILL.md files for repeated tool patterns
.\.cortex\cortex.ps1 propose-gaps        # Propose prefs notes for hot query gaps
.\.cortex\cortex.ps1 propose-survival    # Flag dying patterns for review

# Review and approval
.\.cortex\cortex.ps1 review-proposals    # Show pending proposals
.\.cortex\cortex.ps1 skill-status        # List skill candidates with confidence scores
.\.cortex\cortex.ps1 skill-approve <n>
.\.cortex\cortex.ps1 skill-reject <n>
```

### Meta-Analysis (Stage 8)

```sh
# Show full analysis: rejection rates, fidelity trends, gap evolution, threshold alerts
.\.cortex\cortex.ps1 meta report

# Stage meta-proposals based on analysis
.\.cortex\cortex.ps1 meta propose

# Apply an approved meta-proposal to its target file (prefs.toml or copilot-instructions.md)
.\.cortex\cortex.ps1 meta apply <id>
.\.cortex\cortex.ps1 meta dry-run <id>   # Preview without writing
```

### Diagnostics

```sh
.\.cortex\cortex.ps1 health-report      # System health: patterns, gaps, proposals, orphans
.\.cortex\cortex.ps1 graph-diff         # Graph drift: community changes since last snapshot
.\.cortex\cortex.ps1 session-orphans    # Sessions without a closeout record
.\.cortex\cortex.ps1 doctor --format json
```

---

## Self-Learning Pipeline Details

### Stage 1: Health check
Writes `.cortex/health-report.json` with pattern survival, pending proposals, orphaned sessions.

### Stage 2: Cluster sessions
TF-IDF cosine similarity over tool sequences. Threshold: 0.55. Results: `.cortex/clusters.json`.

### Stage 3: Detect skills
Clusters with ≥ `skill_candidate_min_occurrences` (default 3) become SKILL.md drafts.

### Stage 4: Gap proposals
`query_gap_log` entries with ≥ 3 misses → trial-gated prefs notes.
Seen count ≥ 5: staged immediately. < 5: 7-day trial period.

### Stage 5: Survival proposals
Patterns with `use_count ≥ 3` and `survival_rate < 40%` flagged for review/anti-pattern conversion.
Verification gate: dedup by content hash, survival trend check.

### Stage 6: Fidelity scoring
Each session snapshot scored against ideal PROTOCOL sequence (closeout=0.25, begin=0.15, others=0.10).

### Stage 7: Graph drift
Compares current `graph.json` against latest snapshot. Communities with drift ≥ 0.3 flagged.
High-drift (≥ 0.5): 3× priority boost. Report at `.cortex/drift-report.json`.

### Stage 8: Meta-analysis
Four analyzers:
- **Rejection rates**: gate-only counting from `rejected-proposals.jsonl`
- **Fidelity trends**: average score, low-fidelity sessions, most missed step
- **Gap evolution**: persistent gaps with no approved/pending proposal
- **Threshold impact**: per-type approval rates, alerts for types < 20% approved

Meta-proposals target only `prefs.toml` and `copilot-instructions.md`, never source code.
Apply via `cortex meta apply <id>` after review.

---

## Verification Gates

All proposals pass 5 gates before entering the proposals table:

1. **Duplicate detection** — content hash must be unique
2. **Rust snippet syntax** — `syn` parse check on any Rust in proposed text
3. **Credibility filter** — skill/pref proposals need sufficient session evidence
4. **Gap trial period** — gap proposals with < 5 misses enter 7-day trial
5. **Survival trend** — dying-pattern proposals require confirmed downward trend

Rejected proposals logged to `.cortex/rejected-proposals.jsonl` (auto-rotated at 100KB).

---

## Session Closeout (KNOWLEDGE COMMITTED)

```markdown
[CORTEX-PATTERN: name="..." intent="..." trust="verified" uses="..."]body[/CORTEX-PATTERN]
[CORTEX-AP: description="..." tags="..."]wrong: ...\ncorrect: ...[/CORTEX-AP]
[CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
[CORTEX-ADR: title="..." tags="..."]Context: ... Decision: ...[/CORTEX-ADR]
[CORTEX-PREFS-NOTE: tags="..."]note text[/CORTEX-PREFS-NOTE]
[CORTEX-SKILL-CANDIDATE: name="..." trigger="..."]summary[/CORTEX-SKILL-CANDIDATE]
```

When the user types **KNOWLEDGE COMMITTED**:
```rust
closeout_session(outcome_type: "build_pass", inline_approve: true)
```
All markers are committed immediately (Tier 1). Without it, markers are staged for later review (Tier 2).

`flush_knowledge_markers` fallback: if VS Code session store is unavailable, the tool
scans recent `mcp_calls` args for embedded markers. "0 markers committed" is normal when
the store is inaccessible — not a failure.

---

## prefs.toml

```toml
[style]
line_length = 100
indent = "4 spaces"
naming = "snake_case functions and variables, PascalCase types and enums"

[project]
name = "YourProject"
language = "Rust"
notes = [
    "MANDATORY PRE-CODE CHECK: before writing any factory/tick/spawn/physics function call get_anti_patterns + get_preferences + list_patterns",
    "MANDATORY MID-TASK CORTEX USAGE: after first approach fails call recall <error_keyword> before retrying",
    "session-end: after any coding session, type KNOWLEDGE COMMITTED to trigger closeout",
]

[enforcement]
protocol_gate_mode = "protocol_session_only"  # or "always"
closeout_warning_enabled = true
closeout_grace_period_hours = 2

[consolidation]
staleness_hours = 8
max_commits_per_run = 5
min_cluster_sessions = 3
skill_candidate_min_occurrences = 3
graph_snapshot_days = 30

[skills]
skills_dir = "agent_customization/skills"
auto_update_skills = true

[memory]
max_mirror_files = 200
mirror_consolidation_threshold = 0.75
```

---

## Windows / PowerShell notes

- All `--description`, `--body`, `--reason`, `--wrong`, `--correct` values must be **single-line**
- Use `;` not `&&` for command chaining (PowerShell 5.1)
- Use ASCII hyphen `-` not em-dash `—` in argument values
- Use single-quoted `'strings'` for static values
- After any cortex command, check `$LASTEXITCODE` — silent failure is possible
- Save `.ps1` files as UTF-8 with BOM to avoid Windows-1252 encoding bugs

---

## Token efficiency

Cortex compresses a 400-line Rust struct to ~8 lines of dense semantic signal.
`get_context` pre-selects only what's relevant, capped at your token budget.
The call log reveals what Copilot reaches for most — informing what to pre-inject.
Over time the pipeline proposes improvements to its own configuration, closing
the loop between session outcomes and future behavior.


## How it works

```
your source --> compressor --> SQLite index
                                    |
         patterns, anti-patterns ---|
         annotations, call log  ---|
                                    |
                               MCP server
                                    |
                               Copilot Chat
```

Nothing gets written to memory without your explicit approval.

---

## Setup

```sh
cargo install --path /path/to/cortex

# 0. First-time workspace bootstrap (creates .cortex/cortex.ps1, .cortex/cortex-reset.ps1, .cortex/FIRST_RUN_SETUP_NOTES.md, .cortex/index-sources.json, .vscode/mcp.json)
cortex bootstrap --repo . --source src --name MyProject

# Optional validation from generated launcher (recommended)
./.cortex/cortex.ps1 setup-mcp
./.cortex/cortex.ps1 migrate-legacy
./.cortex/cortex.ps1 reindex
./.cortex/cortex.ps1 mcp-ready -SelfCheckFormat json
./.cortex/cortex.ps1 smoke -SelfCheckFormat json

# 1. Index your source (and optionally a quartz-ctx api-graph)
cortex index --source src --api-graph docs/quartz-ctx/api-graph.json --name Quartz

# 2. Start the MCP server (VS Code picks it up from .vscode/mcp.json)
cortex serve --source src --api-graph docs/quartz-ctx/api-graph.json --name Quartz
```

The bootstrap command writes a valid direct-binary Cortex MCP entry. It avoids
mixed command/argument family bugs (for example, `cortex.exe` command with
PowerShell `-File` arguments).

### Copilot Chat MCP readiness (required)

Before relying on Cortex in chat, verify the required MCP baseline is callable:

- `get_delta`
- `get_preferences`
- `get_anti_patterns`
- `list_patterns`
- `get_context`

If any required tool is missing/failing, remediate before coding:

```powershell
.\.cortex\cortex.ps1 mcp-ready -SelfCheckFormat json
```

If `mcp-ready` fails, run detailed remediation checks:

```powershell
.\.cortex\cortex.ps1 doctor --format json
.\.cortex\cortex.ps1 -- status --format json --full
.\.cortex\cortex.ps1 serve
```

Then verify `.vscode/mcp.json` has a Cortex server entry and reload VS Code window if needed.

For expanded regression coverage (baseline + extended MCP tool surface and schema), run:

```powershell
.\.cortex\cortex.ps1 smoke -SelfCheckFormat json
```

Do not proceed with non-trivial tasks until the MCP baseline passes (unless user
explicitly approves degraded mode after a blocker report).

---

## Commands

### Indexing

```sh
cortex index --source src
cortex index --source src --api-graph docs/quartz-ctx/api-graph.json --name Quartz
```

Compresses source files into dense semantic units, stores them in `.cortex/memory.db`.
Re-run after significant API changes.

### Serving (MCP)

```sh
cortex serve --source src --name Quartz
```

Loads the index and serves it as a JSON-RPC MCP server over stdio. Copilot calls
it as a live skill. Copilot tools available:

| Tool | What Copilot can ask |
|------|---------------------|
| `semantic_search` | "Find anything related to collision" |
| `get_item` | "Show the full details of `Action`" |
| `get_context` | "Give me context for working on src/player.rs" |
| `get_delta` | "Show changes since last checkpoint, excluding build artifacts" |
| `recall` | "What do we know about gravity?" |
| `list_patterns` | "What patterns are approved?" |
| `get_anti_patterns` | "What should I never do?" |
| `suggest_pattern` | Queue a pattern for your review (never auto-saves) |
| `list_all` | "List all enums in the index" |

Phase 4.2 delta controls:

- `get_delta`: `include`, `exclude`, `max_files`, `max_patch_lines`
- `get_context`: `delta_include`, `delta_exclude`, `delta_max_files`, `delta_max_patch_lines`

### Watching

```sh
cortex watch --source src
```

Observes file changes and queues them as pending observations. Never auto-approves
anything. You review and decide what gets remembered.

### Reviewing

```sh
cortex review
```

Lists pending observations from `watch` or Copilot's `suggest_pattern` calls.

### Crystallizing (your decision only)

```sh
# Promote an observation to an approved pattern
cortex crystallize 3 --name "Grounded sound" \
  --intent "Play a sound when an entity lands" \
  --uses "Action::PlaySound,Condition::Grounded" \
  --tags "audio,physics"

# Discard an observation
cortex dismiss 3
```

### Patterns

```sh
cortex pattern list
cortex pattern add --name "..." --intent "..." --body "..."
cortex pattern remove 2

# Script-safe mode
cortex --format json pattern list
cortex --format json pattern health
```

### Anti-patterns

```sh
cortex anti-pattern list
cortex anti-pattern add \
  --description "Don't hardcode asset paths" \
  --wrong 'Action::PlaySound { path: "sounds/jump.ogg", volume: 1.0 }' \
  --correct "Use a named constant or asset key from the asset index"
cortex anti-pattern remove 1

# Script-safe mode
cortex --format json anti-pattern list
```

### Annotations

Free-form notes Copilot will see when the topic is relevant:

```sh
cortex annotate list
cortex annotate add \
  --topic "SetGravity" \
  --body "Gravity is in pixels/sec². Default is 980.0. Values above 2000 cause tunneling." \
  --tags "physics,gotcha"
cortex annotate remove 1

# Script-safe mode
cortex --format json annotate list
```

### Context packet

Pre-compile context for a task without running the MCP server:

```sh
cortex context "working on player jump mechanics" --token-budget 1500
cortex context "working on player jump mechanics" --delta-exclude flowmango-demo --delta-max-files 8
```

### Status

```sh
cortex status
cortex --format json status --full
```

Shows unit count, pattern count, pending observations, most-called MCP tools,
and query-gap telemetry (`unique`, `seen`, `recurrent`).

### Outcome Evidence Weighting

```sh
# Log outcomes (auto evidence apply runs by default)
cortex outcome --session-id protocol_run_2026_06_08 --outcome-type build_pass

# Disable automatic evidence apply for a specific call
cortex outcome --session-id protocol_run_2026_06_08 --outcome-type test_fail --auto-apply false

# Manually apply any pending evidence for a session
cortex outcome-apply --session-id protocol_run_2026_06_08

# Preview without mutating pattern counters
cortex outcome-apply --session-id protocol_run_2026_06_08 --dry-run
```

Automatic evidence processing is idempotent per outcome row using `outcome_applied_log`.
Only pending outcomes for the session are processed on each run.

Existing databases are migrated in-place at startup.
Legacy session-level markers in `outcome_applied_session` are backfilled into
`outcome_applied_log` automatically so previously applied sessions are preserved
under the new per-outcome standard.

Launcher pathway for legacy DBs:

- On run, `.cortex/cortex.ps1` performs a migration preflight and auto-applies legacy backfills when needed.
- Explicit command: `./.cortex/cortex.ps1 migrate-legacy`
- If preflight cannot run, the launcher prints an AI workflow prompt to run migration and smoke checks.

### Benchmark Harness

```sh
# Syntax lookup latency + shape coverage
cortex benchmark --target syntax --samples 64

# Dependency traversal latency + optional precision corpus
cortex benchmark --target dependency --samples 64 --depth 2
cortex benchmark --target dependency --corpus benches/dependency_cases.json --depth 3
```

The dependency corpus accepts either:

- a JSON array: `[{"from":"A","to":"B"}]`
- or wrapper object: `{"cases":[{"from":"A","to":"B"}]}`

### Benchmark Precision Baselines (Recommended)

To make benchmark metrics actionable, define a stable baseline process:

1. Create and version a dependency corpus file (for example `benches/dependency_cases.json`) with representative `from -> to` paths.
2. Run both benchmark targets 5-10 times on a warm local environment and record median values.
3. Store baseline metrics in CI/docs: dependency precision, dependency p95 latency, syntax p95 latency, syntax coverage.
4. Gate regressions by delta from baseline (recommended starting policy):

Dependency precision: fail if drop > 5 percentage points.
Dependency/syntax p95 latency: fail if increase > 25%.
Syntax coverage: track trend first, then add a floor after catalog enrichment stabilizes.

This baseline workflow requires corpus maintenance and periodic re-baselining when graph topology or symbol extraction changes significantly.

### Workflow Doctor (Phase 4.2)

Production-style smoke validation for automation pipelines:

```sh
# Non-mutating workflow checks (safe default)
cortex doctor workflow --repo . --source src --name Quartz

# JSON output for scripts/CI
cortex --format json doctor workflow --repo . --source src --name Quartz

# Optional mutation roundtrip (adds/reverts/removes a sentinel pattern)
cortex doctor workflow --repo . --source src --mutate-pattern
```

Doctor checks include index presence, delta query health, context packet generation,
status rendering, and query-gap telemetry visibility. It exits non-zero if any
check fails.

Architecture note for v4.1 borrow/runtime guarantees:
`docs/architecture-note-borrow-strategy-v41.md`

---

## quartz-ctx integration

cortex reads `docs/quartz-ctx/api-graph.json` directly - no subprocess, no coupling.
Run `quartz-ctx generate` first, then `cortex index --api-graph docs/quartz-ctx/api-graph.json`.
The api-graph items take precedence over raw source units when both exist for the same type
(api-graph has richer doc comments and pre-extracted variant shapes).

---

## copilot-instructions.md snippet

Add this block to your `.github/copilot-instructions.md`. It teaches the assistant
when and how to use cortex throughout a session — not just at boot.

```markdown
## Cortex (Semantic Memory Layer)

cortex holds project-specific knowledge that is NOT in training data:
bug traps, approved patterns, API facts, architecture decisions, and corrections.
Always consult it before writing code and when blocked during a task.

### PROTOCOL - CORTEX Trigger

If user message contains PROTOCOL - CORTEX -:
- Run MCP readiness gate first: required tools are get_delta, get_preferences,
  get_anti_patterns, list_patterns, get_context
- If any required tool fails, run remediation loop before coding:
  1) `.\.cortex\cortex.ps1 doctor --format json`
  2) `.\.cortex\cortex.ps1 -- status --format json --full`
  3) verify `.vscode/mcp.json` cortex server entry
  4) restart MCP server path (`.\.cortex\cortex.ps1 serve`) and re-probe tools
  5) reload VS Code window and re-probe
- Run baseline retrieval: get_delta → get_preferences → get_anti_patterns → get_context
- Use JSON mode for automation-critical commands: cortex --format json status --full
- Hard rule: do not silently bypass missing required MCP tools for non-trivial tasks.
  Stop and report blocker unless user explicitly approves degraded mode.

### Mandatory Pre-Code Check (no trigger required)

Before writing any factory, tick/update, spawn, pool, or physics-integration function:
1. `get_anti_patterns` — check all known traps for this project
2. `get_preferences` — load current style rules and API notes
3. `list_patterns` — find approved patterns for the task category

Skip only for trivial changes: renaming a constant, fixing a typo, adding a comment.

### Mid-Task Cortex Checkpoints

Cortex is a co-author, not a boot-time shelf. Consult it at every "I'm not sure" moment:

| Situation | Tool to call |
|---|---|
| First approach failed | `recall <error_keyword>` before trying a second approach |
| Unfamiliar compiler error | `semantic_search <error description>` before reading source |
| A type/module behaves unexpectedly | `get_item <typename>` before reading docs |
| About to add a new integration point | `simulate_change <unit>` to preview impact first |
| Code compiles but behavior is wrong | `recall <behavior_keyword>` — may be a known runtime trap |
| Choosing between two approaches | `list_patterns` + `get_anti_patterns` to see if one is vetted |

**Blocked rule:** After two failed attempts at the same problem, STOP and run
`recall <topic>` before a third. If cortex has nothing, note the gap for crystallization.

### Tagging Quality (for semantic findability)

New cortex entries must be findable by concept, not just exact API name:
- Tags: API name + behavior + domain + colloquial term
  e.g., GrappleConstraint → tags: grapple,hook,rope,constraint,swing,GrappleConstraint
- Include error code if applicable: E0583,file-not-found (not just module-resolution)
- First sentence of description = what goes wrong, not what the feature is
- Body text: include both the official name AND plain-words description
- Use `semantic_search` to look up entries — it uses embedding similarity,
  so conceptual descriptions find relevant entries even with wrong API names

### Session-End (Mandatory)

After every session where code was written or a bug was fixed:
1. Run post-session: `.\.cortex\cortex.ps1 post-session` (or your launcher equivalent)
2. Add new bugs as anti-patterns; working implementations as patterns
3. Update prefs notes if a new API fact was discovered
```

### Recommended initial prefs.toml

Create `.cortex/prefs.toml` in your project root (or run `cortex init` via the launcher):

```toml
[style]
line_length = 100
indent = "4 spaces"
naming = "snake_case functions and variables, PascalCase types and enums"

[project]
name = "YourProject"
language = "Rust"
notes = [
    "MANDATORY PRE-CODE CHECK (no PROTOCOL required): before writing any factory/tick/spawn/physics function call get_anti_patterns + get_preferences + list_patterns",
    "MANDATORY MID-TASK CORTEX USAGE: after first approach fails call recall <error_keyword> before retrying. After two failed attempts STOP and call recall or semantic_search before a third.",
    "session-end mandatory: after any coding session run post-session then annotate new bugs as anti-patterns and working implementations as patterns",
]
```

### Windows / PowerShell CLI notes

When passing strings to `cortex.exe` from PowerShell:
- All `--description`, `--body`, `--reason`, `--wrong`, `--correct` values must be **single-line**
  — multiline string variables pass each newline as a separate argument to the exe
- Use `;` not `&&` for command chaining (PowerShell 5.1 does not support `&&`)
- Use ASCII hyphen `-` not em-dash `—` in argument values
- Use single-quoted `'strings'` for static values; double-quoted strings expand `$vars`
- After any cortex command, check `$LASTEXITCODE` — silent failure is possible

---

## Token efficiency

cortex compresses a 400-line Rust struct to ~8 lines of dense semantic signal.
The `get_context` tool pre-selects only what's relevant to the current task,
capping at your token budget. Over time, the call log reveals what Copilot
reaches for most - which informs what to pre-inject and what to annotate.
