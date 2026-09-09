# Cortex Suite — Ecosystem Research Landscape

> **Purpose:** Broad survey of the open-source AI ecosystem for ideas applicable to
> cortex_suite's five subsystems: memory, self-learning, auto-skill authoring, lossless
> token savings, and code intelligence (quartz-ctx). Every item is grounded in source
> files read during research; theoretical-only systems are excluded.
>
> **Source research:** Three parallel deep-read passes covering 15+ open-source systems.
> Verified from actual source files, not READMEs alone.

---

## Part A — Memory Systems

### A1. mem0 (`mem0ai/mem0`) — extract-and-vectorize with MD5 audit log

**Architecture:** V3 pipeline (as of April 2026) is "ADD-only". Every conversation turn
is passed through a fact-extraction LLM, embedded, and inserted into a vector store.
The mutation history is tracked in a parallel SQLite audit table.

**SQLite schema** (exact, from `mem0/memory/storage.py`, line-verified):

```sql
CREATE TABLE history (
    id           TEXT PRIMARY KEY,  -- uuid4
    memory_id    TEXT,
    old_memory   TEXT,              -- NULL on ADD
    new_memory   TEXT,              -- NULL on DELETE
    event        TEXT,              -- "ADD", "UPDATE", "DELETE"
    created_at   DATETIME,
    updated_at   DATETIME,
    is_deleted   INTEGER,           -- 0/1
    actor_id     TEXT,
    role         TEXT               -- "user"/"assistant"
);

CREATE TABLE messages (
    id            TEXT PRIMARY KEY,
    session_scope TEXT,             -- "user_id=x&agent_id=y" deterministic key
    role          TEXT,
    content       TEXT,
    name          TEXT,
    created_at    DATETIME
);
-- messages are a last-10 rolling ring-buffer (oldest evicted)
```

**Extraction pipeline** (single-pass, `main.py:_add_to_vector_store` lines 916–1206):
1. Fetch last 10 session messages from SQLite
2. Semantic search for top-10 existing memories (vector store)
3. LLM call with `ADDITIVE_EXTRACTION_PROMPT` → `{"memory": [{"text": "..."}]}`
4. Batch-embed all extracted texts
5. MD5 hash dedup — cross-batch and cross-existing-memories
6. Batch-insert into vector store with payload:
   `{data, text_lemmatized, hash, created_at, updated_at, user_id, agent_id, run_id, expiration_date}`
7. Entity extraction (spaCy NER) → semantic dedup at 0.95 threshold → entity sub-collection

**What cortex can take:**

1. **MD5 hash dedup** for markers: before inserting a new Pattern or Annotation from
   a closeout pass, compute `md5(name + body)` and skip if the hash already exists. This
   prevents silent re-insertion of knowledge the system already has. O(1) check with a
   `UNIQUE` column on `hash TEXT` — no LLM call needed.

2. **Audit/history table for memory mutations.** cortex currently has no record of
   *how* a pattern changed over time. Adding a `pattern_history` table mirroring mem0's
   `history` schema (event=ADD/UPDATE/DELETE, old_value, new_value, actor_id) gives full
   provenance for every pattern, anti-pattern, and annotation. Pairs with UPGRADE_PLAN
   item 4 (supersession tracking).

3. **Session-scope ring-buffer** for the last N messages fed into closeout. cortex's
   session tracking tables could be bounded: keep only the last 20 `mcp_calls` rows per
   session, truncating older rows on insert, instead of growing unboundedly.

---

### A2. Memoripy (`caspianmoon/memoripy`) — richest type system found in the survey

**What it is:** An append-only structured record engine with explicit `MemoryRecord` kinds
(`fact`, `preference`, `procedure`, `decision`, `constraint`, etc.), full version lineage,
and multi-lane RRF retrieval. No LLM-driven extraction — records are inserted explicitly.

**`MemoryRecord` fields** (exact, from `memoripy/types.py:MemoryRecord` lines 387–485):

```
kind: "fact"|"preference"|"episodic_summary"|"procedure"|"policy"|"commitment"|
      "decision"|"belief"|"artifact"|"state"|"constraint"

state: "active"|"dormant"|"pending"|"superseded"|"quarantined"|"deleted"

trust_level: "authoritative"|"user_stated"|"observed"|"derived"|
             "untrusted_external"|"quarantined"

durability: "ephemeral"|"session"|"durable"|"pinned"

layer: "semantic"|"episodic"|"procedural"

# Counters (usage tracking):
access_count, retrieval_count, included_in_context_count
used_in_answer_count, confirmed_by_user_count
associated_success_count, corrected_count, rejected_count, caused_failure_count

# Temporal
observed_at, recorded_at, valid_from, valid_to: ISO8601 | None

# Versioning
version_ids: list[str]     # full lineage
superseded_by_version_id: str | None
contradicted_by: list[str]

# Provenance
evidence_ids: list[str]
admission_reason_codes: list[str]
```

**Retrieval — 8-lane RRF** (from `memoripy/retrieval.py:rank_records()` lines 60–254):

| Lane | Method |
|---|---|
| `lexical` | BM25 (k1=1.5, b=0.75) on `search_text`, normalized to [0,1] |
| `semantic` | Cosine on stored embedding vs query embedding |
| `exact` | Key exact match (1.0) / substring (0.92) / token overlap ≥0.7 (0.65–0.9) |
| `entity` | Named entity overlap: `len(intersection) / len(query_entities)` |
| `temporal` | Recency boost + bi-temporal `as_of` query support |
| `authority` | `trust_score * 0.7 + durability_score * 0.3` |
| `general` | Recency + usage count utility |
| `policy` | Kind matching for query intent type |

Trust scores (hardcoded): `authoritative=1.0, user_stated=0.85, observed=0.65, derived=0.4,
untrusted_external=0.15, quarantined=0.0`

Durability scores: `pinned=1.0, durable=0.75, session=0.45, ephemeral=0.15`

Post-RRF penalties: dormancy (`-0.002` if `state == dormant AND exact < 0.85`),
contradiction (`-min(len(contradicted_by) * 0.0015, 0.0045)`)

Scope fallback: `exact_run(1.0) > user_agent(0.92) > user(0.82) > project(0.7) >
organization(0.6) > namespace(0.55) > global(0.4)` — climbs hierarchy until
`minimum_scope_results` is satisfied.

**Bi-temporal versioning** (`_materialize_records()` lines 314–371): with `as_of`
parameter, walks `version_ids`, filters to `valid_from ≤ as_of < valid_to`, returns
most recent valid version. Historical records returned as `{record_id}@{version_id}`.

**What cortex can take:**

1. **The `kind` enum** for patterns maps cleanly to cortex's current type split:
   `fact` ≈ annotation, `procedure` ≈ pattern (how-to), `constraint` ≈ anti-pattern,
   `policy` ≈ ADR, `decision` ≈ ADR. Adding a `kind` column to the `patterns` table
   lets context pack assembly select by kind — load `procedure` kinds when planning code,
   load `constraint` kinds as a warning layer.

2. **The authority scoring formula** (`trust * 0.7 + durability * 0.3`) is a direct
   improvement over cortex's single `credibility` float. Map: `authoritative=1.0` for
   manually promoted patterns, `observed=0.65` for LLM-crystallized patterns,
   `derived=0.4` for auto-detected skill candidates. The `durability` axis maps to
   cortex's `survival_rate`.

3. **Usage counters** (`included_in_context_count`, `used_in_answer_count`,
   `confirmed_by_user_count`) give a training signal for pattern curation. Currently
   cortex tracks `usage_count` only; splitting it into these three sub-counters lets
   the crystallizer distinguish "surfaced often but never confirmed" (low value) from
   "surfaced and confirmed" (high value).

4. **Admission reason codes** (`admission_reason_codes: list[str]`). For cortex, this
   is a list of session IDs or `[CORTEX-PATTERN]` tag sources that caused the pattern
   to be created. Already implied by `session_origin` but not stored as a list of
   contributing sources. Adding this column enables attribution queries: "which
   patterns were derived only from session X?"

---

### A3. Letta / MemGPT (`letta-ai/letta`) — tiered memory paging model

**Status:** `letta-ai/letta` V1 archive (sha `56ba9c25`). Active V2 (`letta-code`) not
public yet as of research date.

**Three-tier model:**

**Tier 1 — Core Memory** (always in context):
Rendered as XML blocks into every system prompt:
```xml
<memory_blocks>
  <human>
    <description>Information about the user...</description>
    <metadata>chars_current=245, chars_limit=5000</metadata>
    <value>Name: John. Prefers dark mode...</value>
  </human>
  <persona>...</persona>
</memory_blocks>
```
Block struct: `{id, label, value, description, limit: int, read_only: bool}`. Char-limited
per-block. `ContextWindowOverview` model tracks token budget for all tiers simultaneously.

**Tier 2 — Recall Memory** (paginated conversation history):
Agent calls `conversation_search(query, page)` — explicitly paginates, must request
page 0, 1, 2… The agent *knows* about pagination; it is a protocol, not an
implementation detail.

**Tier 3 — Archival Memory** (semantic search on external store):
Agent calls `archival_memory_search(query, page)` and `archival_memory_insert(content)`.
pgvector in the server; exposed as agent tools.

**What cortex can take:**

1. **The char-limited named block as a first-class type.** cortex's context pack is
   currently a single blob assembled by `render_packet()`. Letta's approach — divide the
   context into explicitly named, char-limited blocks that can be independently filled
   and tracked — maps directly onto: `[block: persona]` (agent role), `[block: session]`
   (current objective), `[block: patterns]` (verified patterns), `[block: untrusted]`
   (evidence boundary). Each block has an independent char/token budget.

2. **`ContextWindowOverview` as a structured log.** Tracking `num_tokens_per_block`
   for each named block lets `planner.rs` emit a budget report as a structured metadata
   object (JSON) alongside the context pack — so the LLM can see "patterns: 1,240 / 2,000
   tokens" and self-regulate retrieval requests.

3. **Agent-driven paging as a skill.** Currently `list_memory_handles` + `expand_memory`
   (UPGRADE_PLAN item 7) is a two-call staged retrieval. Letta's model goes further:
   the agent itself requests page 2 when it knows page 1 was insufficient. This is the
   async direction for cortex's recall tools — teach the agent that recall returns pages
   and it can explicitly request more.

---

### A4. cognee (`topoteretes/cognee`) — multi-hop graph + vector hybrid

**Architecture:** Dual-store (graph DB + vector DB + SQLite relational). `cognify()`
pipeline extracts entities and relations via LLM, stores them in a knowledge graph
(KuzuDB/Neo4j), and embeds chunks for vector search.

**Key capability over vector-only:** Multi-hop traversal. After semantic similarity
finds entity A, the graph expands to "what other entities/facts are connected to A?"
This surfaces indirect relationships that vector similarity cannot.

**Supported backends** (relevant for Rust SQLite):
- **Turso/LibSQL** (`databases/vector/turso/TursoVectorAdapter`) — SQLite-compatible
  ANN search via `libsql_experimental`. This is the closest existing code to "SQLite +
  vector search in production."
- **KuzuDB** (`databases/graph/kuzu/`) — embedded graph DB, no server, file-based.

**What cortex can take:**

cortex already has `graph_nodes` and `graph_edges` tables but they are not used in the
primary retrieval path. cognee's pipeline shows the value: after finding top-K patterns
by BM25+cosine, do a one-hop graph expansion to surface related code units, ADRs, or
anti-patterns that the seed result references. Implementation: after `hybrid_search()`
returns pattern IDs, run a single SQL query on `graph_edges WHERE source_id IN (...)` to
retrieve all directly connected nodes. Add those as a "related context" section in the
context pack, below the UNTRUSTED EVIDENCE BOUNDARY if they are lower-confidence.

---

### A5. Graphiti / Zep (`getzep/graphiti`) — bi-temporal fact edges

**Core concept:** Every fact is an edge with two time windows:
- `valid_at` — when the fact became true in the world
- `invalid_at` — when the fact was superseded (NULL if still true)
- `created_at` — when we learned it (database time)

`EntityEdge` schema (from `graphiti_core/edges.py` lines 263–298):
```python
class EntityEdge(Edge):
    name: str              # relation name: "WORKS_AT", "DEPENDS_ON"
    fact: str              # human-readable triple: "AuthModule depends on UserStore"
    fact_embedding: list[float]
    episodes: list[str]   # provenance: session IDs that produced this edge
    expired_at: datetime  # when superseded (soft delete)
    valid_at: datetime    # T1: when fact became true
    invalid_at: datetime  # T2: when fact stopped being true
```

**Ingestion pipeline** (`graphiti_core/graphiti.py`):
```
add_episode(content, valid_at) →
  1. Extract entities (LLM)
  2. Deduplicate against existing graph (semantic + name match)
  3. For new facts: find conflicting existing edges → set invalid_at
  4. Create new edges with valid_at
  5. Embed fact strings, persist
```

**SQLite mapping** (directly portable):
```sql
CREATE TABLE fact_edges (
    id            TEXT PRIMARY KEY,
    source_id     TEXT NOT NULL REFERENCES graph_nodes(id),
    target_id     TEXT NOT NULL REFERENCES graph_nodes(id),
    relation      TEXT NOT NULL,     -- "DEPENDS_ON", "SUPERSEDES", etc.
    fact          TEXT NOT NULL,     -- human-readable triple
    fact_vec      BLOB,              -- float32 embedding (sqlite-vec)
    valid_at      REAL,              -- Unix timestamp, NULL = always
    invalid_at    REAL,              -- NULL = currently true
    created_at    REAL NOT NULL,
    episode_ids   TEXT DEFAULT '[]'  -- JSON array of session IDs
);
CREATE INDEX fact_edges_current ON fact_edges(source_id) WHERE invalid_at IS NULL;
```

**What cortex can take:**

This schema is a direct upgrade to cortex's `graph_edges` table. The current
`graph_edges` table has no temporal validity. Adding `valid_at` and `invalid_at` to it
makes it a bi-temporal knowledge graph without needing a separate graph database. The
`invalid_at IS NULL` partial index is the fast path for all current-fact queries.
Pairs with UPGRADE_PLAN item 4 (supersession tracking).

---

### A-Cross. Cross-system comparison

| System | Dedup | Versioning | Graph | BM25 | Trust/Authority |
|---|---|---|---|---|---|
| mem0 v3 | MD5 hash | Add-only | Entity flat | Yes (lemmatized BM25) | None |
| Memoripy | Admission codes | Full lineage (supersede/merge) | No | 8-lane RRF | 6-level trust + 4-level durability |
| Letta V1 | None | None | None | None | None |
| cognee | Entity dedup | Node upsert | Multi-hop | No | None |
| Graphiti | Entity resolution | Soft invalidation (never delete) | Central | Yes | Episode provenance |

---

## Part B — Self-Learning Systems and Auto-Skill Authoring

### B1. Voyager (MineDojo/Voyager) — best-in-class executable skill library

**What it is:** A Minecraft lifelong-learning agent. The skill library subsystem is the
most directly applicable thing in this entire research pass — it is the only system that
produces, stores, retrieves, and deduplicates reusable executable code skills in a
durable way.

**Skill storage structure** (verified from `voyager/agents/skill.py:1-120`):

```python
# Three-layer persistence:
# 1. In-memory dict — primary index
self.skills: Dict[str, Dict] = {
    "craftWoodenPickaxe": {
        "code":        "async function craftWoodenPickaxe(bot) { ... }",
        "description": "async function craftWoodenPickaxe(bot) {\n    // Crafts a wooden pickaxe..."
    }
}
# 2. Vector DB — Chroma + OpenAI embeddings, indexed by description text
self.vectordb = Chroma(collection_name="skill_vectordb", ...)
# 3. Disk — craftWoodenPickaxe.js (code), craftWoodenPickaxe.txt (description)
```

Invariant enforced at all times: `vectordb._collection.count() == len(self.skills)`

**Skill description generation (the indexing secret):**
The LLM is asked to write ≤6 sentences that describe *what* the function does, not
repeat its name. These descriptions (not the code) are embedded for retrieval. When
the agent is given a task query like "How to mine iron ore?", it searches descriptions
— the code is what gets injected as context.

**Retrieval:** `similarity_search_with_score(task_context, k=5)` — top-5 by cosine
similarity on LLM-generated skill descriptions.

**Graduation gate:** A `CriticAgent` (GPT-4) checks task success after execution.
`success=True` from a single execution promotes a skill. One success is the threshold.

**Deduplication:** Name-based only. If two functions have different names but the same
behavior, both get stored. The old code file is kept on disk as `SkillNameV2.js`;
the vector DB entry is overwritten with the new embedding.

**What cortex_suite can take:**

1. **Description-based indexing, not code-based.** cortex_suite's `skills` are currently
   represented as a trigger + summary in `skill_candidates`. Voyager shows that the
   *description* (what the skill does) should be the embedding/search surface, while the
   *body* (how to execute) is what gets retrieved. These should be separate fields.

2. **Code artifact storage per skill.** Skills currently live as markdown files in
   `.cortex/proposals/skill_*.md`. Voyager stores the actual executable artifact
   separately from the index. For cortex skills (which are agent instruction sequences,
   not code), the "artifact" is the full instruction sequence — store it in a column,
   not only in a markdown file on disk.

3. **The invariant check.** Enforcing `vectordb.count() == len(skills)` prevents silent
   drift between the search index and the actual skill store — a bug cortex has already
   encountered with FTS getting out of sync with the base tables.

---

### B2. JARVIS/HuggingGPT (microsoft/JARVIS) — dependency-graph task decomposition

**What it is:** Routes tasks to pre-existing HuggingFace models via an LLM-generated
dependency graph. Not a skill library, but the task decomposition approach is relevant.

**Task representation** (verified from `hugginggpt/server/awesome_chat.py`):

```json
[
  {"task": "image-classification", "id": 0, "dep": [-1], "args": {"image": "..."}},
  {"task": "text-generation",      "id": 1, "dep": [0],  "args": {"text": "<GENERATED>-0"}}
]
```

`<GENERATED>-N` is a dependency reference resolved at runtime from task N's output.

**Model selection:** Static JSONL catalog grouped by task type → top 10 candidates →
HTTP availability check → GPT selects best by description+likes.

**What cortex_suite can take:**

The `<GENERATED>-N` dependency syntax is a minimal, parseable way to chain outputs
between steps in a skill. cortex_suite skills currently have a flat tool_sequence list
with no inter-step data flow. Adding a simple dependency reference like this would let
skills express "pass the output of step 1 to step 2" — enabling real composed workflows.

---

### B3. AutoGen (microsoft/autogen) — reflection loop and bounded context

**What it is:** A multi-agent conversation framework. Two findings are directly applicable.

**Reflection after tool use** (verified from `_assistant_agent.py`):
```python
reflect_on_tool_use: bool  # default True
# When True: after receiving tool result, a second model inference runs with
# the full conversation + result, producing a natural-language synthesis.
```

This is exactly cortex's `recurrent_think` pattern but made structural. The agent is
forced to reason about what the tool returned before moving on, rather than potentially
ignoring it.

**BufferedChatCompletionContext:** AutoGen's context management includes a `maxlen`-
bounded chat history. When the buffer is full, the oldest messages are dropped. This
is the same insight as cortex's token budget, applied to conversation history.

**Society of Mind compression:** A nested team's full conversation is compressed into
a summary for the outer team. This is architectural context compression — not token-level
compression but conversation-level summarization at team boundaries.

**What cortex_suite can take:**

The reflection-after-tool-use pattern is essentially what `recurrent_think` does but
it should also happen automatically after `recall` and `get_context` calls — the agent
should be encouraged to narrate what it found relevant before proceeding. This could
be surfaced as a prefs.toml note or as a protocol_gate enforcement.

---

### B4. Self-RAG (AkariAsai/self-rag) — inline confidence scoring

**What it is:** A fine-tuned language model that emits special tokens during generation
to self-evaluate retrieval necessity and evidence grounding. Not directly deployable
without fine-tuning, but the scoring design is instructive.

**Special tokens** (verified from `retrieval_lm/utils.py`):

| Token | Meaning |
|---|---|
| `[Retrieval]` vs `[No Retrieval]` | Should the model retrieve? |
| `[Relevant]` vs `[Irrelevant]` | Is the retrieved passage on-topic? |
| `[Fully supported]` vs `[Partially supported]` vs `[No support]` | Does the passage ground the claim? |
| `[Utility:1]` through `[Utility:5]` | How useful is this passage to the task? |

**Scoring formula** (verified from `run_short_form.py`):

```
final_score = w_rel * (Relevant / sum_rel)
            + w_sup * (Fully_supported / gt_sum + 0.5 * Partially_supported / gt_sum)
            + w_use * sum(utility_weight[i] * Utility_i / ut_sum)
# defaults: w_rel=1.0, w_sup=1.0, w_use=0.5
# utility_weights: [-1, -0.5, 0, 0.5, 1] for utility 1–5
```

**What cortex_suite can take:**

The three-axis scoring (relevance + grounding + utility) maps directly onto what
`recall` and `get_context` should do when they return results. Currently cortex
returns items ranked by TF-IDF cosine similarity alone. Adding explicit grounding and
utility signals — even as rough heuristics, not fine-tuned model tokens — would improve
context pack quality:

- **Relevance:** current cosine/BM25 score (already computed)
- **Grounding:** does this item reference a type/function name that appears in the
  current hint? (computable from the `uses` field on patterns and `unit_refs`)
- **Utility:** `credibility * survival_rate` (already computed per pattern)

A weighted combination of these three, matching the Self-RAG weights, gives a principled
ranking for context pack assembly that goes beyond single-axis cosine similarity.

---

### B5. Eureka (eureka-research/Eureka) — evolutionary candidate generation

**What it is:** Automatically writes RL reward functions via population-based evolutionary
selection. Generates N candidates, runs them all, keeps the best, feeds its performance
back into the next round of generation.

**Key design for cortex:** Eureka solves the skill proliferation problem that cortex is
at risk of through **tournament selection** — generate N, keep 1, discard N-1. The
survived candidate's performance *text* (tensorboard stats as natural language) is fed
back to the generator as the prompt for the next round. The conversation IS the memory
of what worked and what didn't.

**What cortex_suite can take:**

1. **N-candidate generation for skill drafts.** Currently `propose_skill` generates one
   draft. Eureka's approach: generate 3-5 variants of a skill draft, auto-test them
   against the session corpus, promote the best. The "test" for a skill draft is:
   does applying it to the sessions that produced it improve their `recall` hit rate?

2. **Performance feedback in natural language as the next-round prompt.** When a skill
   draft fails the gate check (`verify.rs`), the rejection reason should be fed back
   into the next consolidation run's context for that cluster — not just discarded.

---

## Part C — Lossless Token Compression Systems

### C1. LLMLingua / LLMLingua-2 (Microsoft) — best production option

**GitHub:** `microsoft/LLMLingua` — MIT License.

**Algorithm comparison:**

| | Original LLMLingua | **LLMLingua-2** |
|---|---|---|
| Model type | Causal LLM (Llama-2-7B, 7B params) | Encoder (XLM-RoBERTa-L or mBERT, 125–560M) |
| Method | Per-token perplexity from causal LM | Binary token classification (keep/drop) |
| Speed | Slow (autoregressive forward passes) | **3–6× faster** (single forward pass) |
| Hardware | Requires GPU for 7B model | **CPU-capable** |
| Task-awareness | Yes (question-conditioned in LongLLMLingua) | Task-agnostic (trained on GPT-4 distillation) |
| Compression | 3–5× typical, 20× max reported | Similar ratios |
| Training data | None needed (use any causal LM) | GPT-4 distillation on MeetingBank |

**Original LLMLingua pipeline** (from `llmlingua/prompt_compressor.py`):
```
compress_prompt()
  ├─ control_context_budget()    ← rank & drop entire segments by PPL
  ├─ control_sentence_budget()   ← drop sentences within segments
  └─ iterative_compress_prompt() ← token-level greedy in 200-token chunks
```

`LongLLMLingua` mode: perplexity is conditioned on the question — tokens that are
surprising *given the question* are kept (fixes "lost in the middle" problem).

**LLMLingua-2 training:**
GPT-4 labels each token "preserve/drop" (can the original be reconstructed from the
compressed version?). XLM-RoBERTa fine-tuned on these binary labels. Special `[NEW0]`
through `[NEW99]` vocab tokens force-preserve specific tokens (e.g., code delimiters).

**API:**
```python
from llmlingua import PromptCompressor

# LLMLingua-2 (recommended — CPU-capable):
llm_lingua = PromptCompressor(
    model_name="microsoft/llmlingua-2-bert-base-multilingual-cased-meetingbank",
    use_llmlingua2=True,
)
result = llm_lingua.compress_prompt(
    context=["retrieved doc 1", "doc 2"],
    instruction="You are a helpful assistant",
    question="What is X?",
    rate=0.5,           # keep 50% of tokens
    force_tokens=["```", "\n"],  # always preserve
)
# result: {"compressed_prompt": str, "origin_tokens": int, "compressed_tokens": int, "ratio": "2.1x"}
```

**Production readiness:** ✅ LangChain integration. ✅ LlamaIndex integration.
✅ Microsoft Prompt Flow. ✅ CPU-capable with LLMLingua-2.

**What cortex can take:**

cortex's `compact_output` in `compressor.rs` currently uses a lossless gzip
content-addressed store — it doesn't reduce token count, it reduces storage and
wire size between server and tool invocation. These are *complementary*, not competing:

- **LLMLingua-2 is appropriate for the context pack**, not for stored memory.
  `render_packet()` in `planner.rs` assembles a context blob that is injected into the
  LLM's context window. The LLM's context window is the resource being saved. Running
  LLMLingua-2 over the assembled context pack (after assembly, before returning it to
  the tool caller) could reduce token consumption by 40–50% with minimal semantic loss.

- **Architecture:** add a `compress_context_pack` flag to `build_context_packet()`.
  When true, serialize the assembled packet to a string, POST it to a sidecar Python
  process or call a Rust binding, return the compressed string. The sidecar can be a
  FastAPI server wrapping LLMLingua-2 running once at startup.

- **The `force_tokens` list** should include all CORTEX-tag boundaries (`[CORTEX-`,
  `[/CORTEX-`, `===`), so knowledge marker tags are never truncated mid-structure.

---

### C2. Selective Context (`liyucheng09/Selective_Context`) — GPT-2 self-information

**Algorithm:** Per-token self-information (`-log P(token | context)`) scored by GPT-2
(117M), then threshold by percentile to drop low-information phrases. Phrase-level
granularity (spaCy noun chunks) is the default.

```python
sc = SelectiveContext(model_type='gpt2', lang='en')
context, masked = sc(text, reduce_ratio=0.35, reduce_level='phrase')
```

**Assessment:** Simpler and faster than original LLMLingua (GPT-2 vs 7B model), but
*strictly unconditional* — it doesn't know the question. Good for summarization-style
cleanup of verbose context (boilerplate, repetition), poor for query-focused RAG
compression. **For cortex: lower ROI than LLMLingua-2 but zero GPU dependency.**

Useful for one specific cortex use case: compressing the `compressed_blob` blobs in the
content store before writing them. Currently blobs are gzip of raw tool output; applying
Selective Context first, then gzip, would reduce the stored artifact size at the cost of
information fidelity — only worth it for very long outputs (build logs, large diffs).

---

### C3. RECOMP (`carriex/recomp`) — query-focused RAG compressor

**Architecture:** A dual-encoder trained to select the most relevant sentences from
retrieved documents, given the query. Sits between retriever and generator in a RAG
pipeline. Outputs empty string if no retrieved passage is relevant ("selective
augmentation" — avoids injecting noise).

**Two compressors:**
- **Extractive**: dual-encoder scoring sentences vs query → select top-k
- **Abstractive**: Flan-T5 that generates a synthetic query-focused summary of multiple documents

**Results:** 6% compression rate (keeps 6% of original tokens) with minimal QA accuracy
loss. Generalizes across LLMs.

**Assessment for cortex:** RECOMP is the right design pattern for `recall` output
compression — before returning search results to the LLM, score each result sentence
against the current task/hint and drop sentences with low dual-encoder scores. However,
the pre-trained RECOMP models are trained on NaturalQuestions/TriviaQA, not code/agent
data. Using the architecture without fine-tuning would likely hurt code contexts.
**Verdict: adopt the pattern (score sentences before returning), not the pre-trained models.**

---

### C4. StreamingLLM (MIT Han Lab) — NOT compression

StreamingLLM is a **streaming inference** technique (KV cache sliding window with
attention sinks), not a context compression technique. It does not help fit more
information into the context window — it enables infinite-length conversations without
OOM errors by evicting middle context.

```python
class StartRecentKVCache:
    def __init__(self, start_size=4, recent_size=512):
        # Keeps 4 "sink" tokens + last 512 tokens; everything else evicted
```

**For cortex:** Not applicable. cortex controls what goes into the context pack, not
the LLM's KV cache internals. The concept that is applicable is the "sink" token idea:
always preserve a fixed "anchor" at the start of the context pack (the session objective)
even when the pack is compressed or truncated — analogous to never evicting sink tokens.

---

## Part D — Code Intelligence: quartz-ctx Upgrades

### D1. tree-sitter `tags` crate — drop-in upgrade for quartz-ctx

**GitHub:** `tree-sitter/tree-sitter` — MIT License. 100+ language grammars.

**What `crates/tags` provides** (from `crates/tags/src/tags.rs`):

```rust
pub struct Tag {
    pub range: Range<usize>,       // byte range in source
    pub name_range: Range<usize>,  // range of the identifier only
    pub line_range: Range<usize>,  // line numbers
    pub span: Range<Point>,        // (row, col) start/end
    pub docs: Option<String>,      // adjacent doc comment, pre-extracted
    pub is_definition: bool,       // definition vs. reference
    pub syntax_type_id: u32,       // maps to "function", "class", "method", etc.
}
```

Extraction is query-file-driven. A `.scm` file for each language specifies what counts
as a definition or reference:
```scheme
; Rust example:
(function_item name: (identifier) @name) @definition.function
(impl_item type: (type_identifier) @name) @definition.class
(call_expression function: (identifier) @name) @reference.call
```

`TagsConfiguration::new(language, tags_query, locals_query)` compiles the query.
`TagsContext.generate_tags(config, source, cancellation)` runs it, returning an iterator
of `Tag` objects. The `docs` field is extracted from adjacent comment nodes
automatically by the query configuration.

**Coverage:** Languages with existing `.scm` tag queries include: Rust, Python,
JavaScript, TypeScript, Go, C, C++, Java, C#, Kotlin, Swift, Ruby, PHP, Scala,
Haskell, OCaml, Erlang, Elixir, Bash, SQL, and more. All grammars:
https://github.com/tree-sitter (each language grammar is a separate repo).

**Local variable scoping:**
`@local.scope`, `@local.definition`, `@local.reference` captures enable rename-safe
variable tracking within a file — quartz-ctx could use this to suppress false-positive
call edges for shadowed variable names.

**Incremental re-parsing:** O(edit-size) re-parse on text edits. If quartz-ctx ever
needs to watch files for changes (live indexing), tree-sitter supports it natively.

**What quartz-ctx can take:**

quartz-ctx currently uses `syn` (Rust AST, full type resolution path) and tree-sitter
(9 other languages). The `tags` crate is the part of tree-sitter that quartz-ctx is
*not yet using*. The current tree-sitter path in quartz-ctx extracts named nodes via
custom queries — but `tags` provides a standardized, per-language extraction format
with doc-comment extraction baked in.

**Concrete upgrade:** Switch quartz-ctx's non-Rust extraction from custom node queries
to `TagsContext.generate_tags()`. The output `Tag` type maps directly to `ApiItem`:
- `name_range` → `name`
- `syntax_type_id` → `kind` (function/method/class)
- `span` → `SourceSpan`
- `docs` → `doc_comment` (new field — currently not extracted)
- `is_definition` → filter out references

**Doc comment extraction is free.** This alone — pre-extracted doc comments alongside
every function definition — is a significant win for LLM context quality.

---

### D2. rust-analyzer as a library — type-resolved Rust confidence

**GitHub:** `rust-lang/rust-analyzer` — MIT/Apache-2.0.

**Entry point** (explicitly documented as external API, `crates/load-cargo/src/lib.rs`):
```rust
// "Note, don't remove any public api from this. This API is consumed by external tools."
pub fn load_workspace_at(
    root: &Path,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig { load_out_dirs_from_check, with_proc_macro_server, prefill_caches, num_worker_threads },
    progress: &dyn Fn(String),
) -> anyhow::Result<(RootDatabase, vfs::Vfs, Option<ProcMacroClient>)>
```

**What you get from the `ide` crate:**

```rust
mod file_structure;    // per-file symbol tree (functions, impls, types)
mod static_index;      // bulk semantic index across whole codebase
mod goto_definition;   // resolve any identifier to its definition location
mod references;        // find all references to a symbol
mod hover;             // type information + doc comment for any position
mod inlay_hints;       // inferred types at every expression
```

`Analysis::file_structure(file_id)` returns a flat list of `StructureNode` objects —
every function, impl, type, constant in the file with their nesting and names. This is
the most directly useful API for quartz-ctx: one call per file, returns the complete
symbol tree with type-resolved information.

**Practical constraints:**
- **API stability:** Semver-exempt. Must pin to a specific git SHA.
- **Load cost:** Several seconds + hundreds of MB RAM to load a full Cargo workspace.
  Not per-request — suited for a one-time background index pass.
- **Availability:** Only for Rust codebases. No help for Python/Go/etc.

**Confidence upgrade:**
quartz-ctx currently assigns `Confidence::NameResolved` to tree-sitter-parsed Rust
and `Confidence::Resolved` to `syn`-parsed Rust. rust-analyzer would enable
`Confidence::TypeResolved` — the highest possible confidence, with actual type
information. This is what IDEs use. The tradeoff: it requires running as a background
indexer rather than on-demand.

**Recommended integration:** A `--deep-index` mode in quartz-ctx that uses
rust-analyzer for Rust files when a Cargo workspace is present. Fast mode (current)
uses `syn`; deep mode uses rust-analyzer for type-resolved extraction.

---

### D3. Semgrep — structured cross-language extraction

**GitHub:** `semgrep/semgrep` — LGPL-2.1 (OSS engine). 30+ languages.

**What Semgrep extracts** (JSON output via `semgrep --json`):

```bash
# Extract all public functions with their signatures:
semgrep --json --pattern "pub fn $FUNC($ARGS)" --lang rust src/
# Returns: {file, line, match: "pub fn process_query(input: &str)", metavars: {$FUNC: "process_query"}}

# Extract all API call sites:
semgrep --json --pattern "$OBJ.$METHOD(...)" --lang python src/

# Extract class hierarchies:
semgrep --json --pattern "class $C($BASE):" --lang python src/
```

Metavariable captures (`$FUNC`, `$ARGS`, `$BASE`) are returned as structured data —
no post-processing regex needed.

**Autofix capability:** Semgrep can transform code via `fix:` patterns. This means it
can be used for automated refactoring in addition to extraction — e.g., adding a
`#[cortex_trace]` attribute to all public functions for instrumentation.

**Limitations:** Intra-file only (OSS edition). No type inference. Pattern matching
is syntactic — `$X.foo()` matches regardless of `$X`'s type.

**What quartz-ctx can take:**

Semgrep fills the gap between tree-sitter (structural extraction) and rust-analyzer
(type resolution). For complex multi-pattern extraction — "find all functions that
both take `&mut State` as a parameter AND call `self.db.execute()`" — Semgrep's
`pattern-and` / `pattern-inside` / `pattern-not` composition is expressive and
doesn't require writing a custom query per language.

**Concrete use:** A `semgrep_extract` backend in quartz-ctx for languages where
complex structural patterns matter. The CLI is mature, actively maintained, and fast.
The `--json` output is stable.

---

### D4. Code Embeddings — semantic code search

**Current quartz-ctx situation:** quartz-ctx extracts API items (function names,
signatures, call edges) as structured data but does not embed them for semantic search.
All matching between call sites and definitions is name-based.

**Best local options (verified, 2025):**

| Model | Size | Context | Strength |
|---|---|---|---|
| `microsoft/codebert-base` | 110M | 512 tokens | Code ↔ NL (docstring → function) |
| `nomic-ai/nomic-embed-code` | ~130M | Function-sized | Code ↔ Code similarity |
| `jinaai/jina-embeddings-v2-base-code` | 137M | 8K tokens | Long file context |
| `voyage-code-2` (API) | Hosted | — | High benchmark scores |

**Practical hybrid pipeline:**
```python
model = SentenceTransformer("nomic-ai/nomic-embed-code")
embeddings = model.encode(function_bodies)  # (N, 768)
# Index: FAISS IndexFlatIP for inner-product (= cosine on normalized vecs)
# Hybrid: FAISS for semantic + BM25 (tantivy) for lexical → RRF merge
```

**What cortex + quartz-ctx can take:**

1. **quartz-ctx:** Embed function bodies (not just names) at index time. Store
   embedding BLOB alongside each `ApiItem`. Feed into cortex's `code_units` with a
   vector column. Semantic search across the extracted API surface — "find functions
   that do JSON parsing" even when named `deserialize_payload`.

2. **cortex `code_units`:** The `compressed` BLOB column currently holds lossless-gzip
   of the unit body. Add a `vec BLOB` column (float32 embedding) alongside it.
   sqlite-vec extension enables ANN search on this column without a separate vector DB.

3. **doc comment embedding vs code embedding:** CodeBERT is pre-trained on
   code+docstring pairs (bimodal). If doc comments are extracted (see tree-sitter
   `tags` section), embedding the combined `docstring + signature` string gives better
   retrieval than embedding raw code.

---

### D5. tower-lsp — build a quartz-ctx language server

**GitHub:** `ebkalderon/tower-lsp` (original, slower maintenance) and
`tower-lsp-community/tower-lsp-server` (community fork, more active).

**What it provides:** Async Rust framework for writing LSP servers. Handles JSON-RPC
protocol, message framing, async dispatch. Exposes a `LanguageServer` trait.

**Most relevant use case for quartz-ctx:**

quartz-ctx is currently a one-shot batch extractor (run, produce JSON, exit). An LSP
server mode would enable:
- **Streaming extraction:** language client sends `textDocument/didOpen` events as
  files are opened; quartz-ctx index builds incrementally
- **Live call graph updates:** when a function is edited, only that file is re-extracted
- **IDE integration:** VS Code / Neovim can query quartz-ctx for cortex-specific
  semantic tokens or inlay hints ("this function appears in N call sites")

This is a longer-term direction, not an immediate upgrade. The immediate path:
add a `--watch` mode to quartz-ctx that uses `notify` crate file system watching +
incremental tree-sitter re-parsing.

**LSP as extractor (alternative use):** Drive an existing language server (rust-analyzer,
gopls, typescript-language-server) as an LSP *client* to extract code intelligence.
The protocol: send `textDocument/documentSymbol` → get all symbols; send
`textDocument/definition` → resolve a symbol. No mature Rust LSP client library exists
yet; this requires manual JSON-RPC stdio handling. **More overhead than tree-sitter
for most use cases — only worthwhile when type information is required.**

---

## Synthesis: All Priority Additions

The items below are ranked by the **impact/effort ratio** across all four research areas.
Each row maps to specific source files in cortex_suite or quartz-ctx. Items from
`UPGRADE_PLAN.md` (rta-smriti-brain research) are not repeated here — treat the two
documents as complementary.

### Tier 1 — High impact, low-medium effort

| Item | Source system | Target files | Effort | Impact |
|---|---|---|---|---|
| **MD5 hash dedup on patterns/annotations** — add `hash TEXT UNIQUE` column; skip re-insert on duplicate. Zero-cost O(1) check. | mem0 | `memory.rs`, `crystallizer.rs` | Low | High |
| **tree-sitter `tags` crate for non-Rust extraction** — switch from custom queries to `TagsContext.generate_tags()`; gets `docs` field (doc comments) for free | tree-sitter | `quartz-ctx/src/lang.rs` | Low | High |
| **Memoripy `kind` enum on `patterns`** — `procedure`/`constraint`/`policy`/`fact` column on `patterns` table; context pack assembly selects by kind | Memoripy | `model.rs`, `memory.rs`, `planner.rs` | Low | High |
| **Authority score formula** — replace single `credibility` float with `trust_score * 0.7 + survival_rate * 0.3` in recall ranking | Memoripy | `recall_match.rs`, `planner.rs` | Low | Medium |
| **Usage counter split** — add `included_in_context_count`, `confirmed_count`, `corrected_count` to `patterns`; drives crystallizer curation | Memoripy | `memory.rs`, `crystallizer.rs` | Low | High |
| **Bi-temporal `valid_at`/`invalid_at` on `graph_edges`** — enables AS-OF queries and soft supersession without extra tables | Graphiti | `memory.rs`, `model.rs` | Low | Medium |
| **Description-as-index for skill_candidates** — separate `description TEXT` (what it does) from `body TEXT` (how to execute); embed/search on description | Voyager | `memory.rs`, `skills.rs` | Low | Medium |
| **Three-axis recall scoring** — relevance (cosine) + grounding (unit_refs overlap) + utility (credibility × survival) weighted sum | Self-RAG | `planner.rs`, `recall_match.rs` | Medium | High |

### Tier 2 — Medium impact, medium effort

| Item | Source system | Target files | Effort | Impact |
|---|---|---|---|---|
| **`pattern_history` audit table** — mem0-style ADD/UPDATE/DELETE event log per pattern; full provenance | mem0 | `memory.rs`, `model.rs`, `crystallizer.rs` | Medium | Medium |
| **Named char/token-limited blocks** — divide `render_packet()` output into named sections (`persona`, `session`, `patterns`, `untrusted`) with per-block token budget | Letta | `planner.rs`, `mcp/tools.rs` | Medium | High |
| **`ContextWindowOverview` metadata** — return structured block budget JSON alongside the context pack so the LLM can self-regulate recall requests | Letta | `planner.rs`, `mcp/tools.rs` | Low | Medium |
| **One-hop graph expansion after search** — after `hybrid_search()` returns pattern IDs, query `graph_edges WHERE source_id IN (...)` and append connected nodes as "related context" | cognee | `planner.rs`, `search.rs` | Medium | Medium |
| **Rejection feedback into next consolidation** — when `verify.rs` rejects a skill draft, store the rejection reason and feed it into the next consolidation run context for that cluster | Eureka | `consolidator2.rs`, `verify.rs` | Low | Medium |
| **Dependency reference syntax in `tool_sequence`** — add `<OUTPUT>-N` references to skill steps to express inter-step data flow | JARVIS | `model.rs`, `skills.rs`, `mcp/tools.rs` | Medium | Medium |
| **LLMLingua-2 context pack compression** — after `render_packet()` assembles the full pack, optionally compress via LLMLingua-2 sidecar (Python FastAPI) at `rate=0.5`; preserve all `[CORTEX-` boundaries via `force_tokens` | LLMLingua-2 | `planner.rs`, `mcp/tools.rs` | Medium | High |

### Tier 3 — High impact, high effort

| Item | Source system | Target files | Effort | Impact |
|---|---|---|---|---|
| **N-candidate skill draft generation with auto-test** — generate 3-5 skill variants, score by recall hit improvement on source sessions, promote best | Eureka | `skills.rs`, `consolidator2.rs` | High | High |
| **Code embedding column on `code_units`** — add `vec BLOB` (float32 embedding via `nomic-embed-code` or `codebert-base`) to `code_units`; add sqlite-vec ANN search to `search.rs` | CodeBERT/Nomic | `memory.rs`, `search.rs`, `planner.rs` | High | High |
| **quartz-ctx `--deep-index` mode** — use rust-analyzer `load-cargo` for type-resolved extraction on Rust workspaces; assign `Confidence::TypeResolved` | rust-analyzer | `quartz-ctx/src/lang.rs`, `model.rs` | High | Medium |
| **Memoripy 8-lane RRF** — full multi-signal scoring (BM25 + cosine + exact + entity + temporal + authority + utility) in `search.rs`; replaces single-axis cosine | Memoripy | `search.rs`, `recall_match.rs`, `planner.rs` | High | High |

---

## Implementation Sequencing

Order that avoids schema conflicts and builds on each prior step:

```
1. MD5 hash dedup (trivial schema change, immediate win)
2. Usage counter split (additive columns, no migration breakage)
3. Memoripy `kind` + authority score (additive, improves search immediately)
4. tree-sitter tags crate upgrade (quartz-ctx only, isolated)
5. Named block context assembly + ContextWindowOverview (planner change, no schema)
6. Three-axis recall scoring (recall_match + planner, no schema)
7. bi-temporal graph_edges (schema migration, pairs with UPGRADE_PLAN item 4)
8. pattern_history audit table (new table, no breaking change)
9. One-hop graph expansion (search + planner, after bi-temporal is in)
10. LLMLingua-2 compression sidecar (optional flag, zero risk to existing path)
11. Code embedding column + sqlite-vec (requires sqlite-vec extension loaded)
12. N-candidate skill draft generation (requires stable skill pipeline first)
13. Memoripy 8-lane RRF (requires embedding column from step 11)
14. rust-analyzer deep-index mode (quartz-ctx only, isolated from cortex)
```

---

## Answers to Specific Questions

**Q: Can rust-analyzer be used as a library to give quartz-ctx type-resolved confidence
for Rust beyond what syn alone provides?**

Yes. `load-cargo/src/lib.rs` is explicitly documented as an external API. The
`Analysis::file_structure()` call returns a type-checked symbol tree per file. The API
is unstable (must pin to git SHA) and has a multi-second load cost — suitable for a
`--deep-index` background pass, not on-demand extraction. Recommended: make it an
opt-in flag, not the default path.

**Q: Would LLMLingua-2 improve `compact_output` or is cortex's lossless approach
intentionally different?**

They serve different purposes and are complementary. `compact_output` (gzip in
`cache.rs`) compresses stored blobs for disk efficiency — it's lossless and correct to
keep lossless for stored artifacts. LLMLingua-2 compression would be applied to the
*assembled context pack* (output of `render_packet()`) before it is returned to the LLM
as tool output — this is a different layer. Apply LLMLingua-2 as an optional post-pass
in `planner.rs` after the pack is assembled; leave `cache.rs` gzip unchanged.

**Q: Does mem0's extraction pipeline suggest improvements to cortex's
closeout/markers approach?**

Two specific improvements: (1) MD5 hash dedup before insertion (currently cortex can
re-insert identical patterns from repeated sessions); (2) The `ADDITIVE_EXTRACTION_PROMPT`
in mem0 is structured as a few-shot fact extractor targeting specific categories. cortex's
`[CORTEX-PATTERN]` tags are author-driven rather than LLM-extracted. The mem0 approach
suggests adding a light extraction pass over session transcripts to find implicit patterns
the author didn't explicitly tag — this is the `propose_gaps` step in `consolidator2.rs`
but could be made more systematic with a category-structured prompt.

**Q: Does Letta's archival memory / paging model suggest a tiered memory architecture
for cortex?**

Yes. The named-block model maps directly to cortex's context pack sections. The most
actionable item is the `ContextWindowOverview` metadata object — returning token budget
per block alongside the pack lets the LLM self-regulate (it can see it has used 1,800/2,000
pattern tokens and decide to stop requesting more patterns). The paging model (agent
explicitly requests page 2) is the right long-term direction for `list_memory_handles` +
`expand_memory` — teach the agent that recall is paginated and it controls depth.
