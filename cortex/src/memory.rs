/// SQLite-backed persistent memory store.
///
/// Tables:
///   code_units          — compressed, indexed source items with term vectors
///   code_members        — fields, variants, methods (linked to units)
///   patterns            — approved code patterns (always manually approved)
///   anti_patterns       — known bad approaches to inject as negative examples
///   annotations         — free-form notes Copilot will see
///   mcp_calls           — log of every tool call Copilot makes
///   pending_observations — file changes waiting for Syn's review
///   content_store        — content-addressed gzip blob store (cache layer)
///   response_cache       — tool response cache keyed by (tool+args+index_version)
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::*;

// ── First-run prefs.toml template ─────────────────────────────────────────────
const PREFS_TEMPLATE: &str = r#"[style]
line_length = 100
indent = "4 spaces"
naming = "snake_case functions and variables, PascalCase types and enums"
error_handling = "use Result<T, E>; no unwrap() in engine code; ? operator preferred"
comments = "/// doc comments on all public API; inline only for non-obvious logic"

[project]
name = "MyProject"
language = "Rust"
notes = [
    "MANDATORY PRE-CODE CHECK (no PROTOCOL required): before writing any factory/tick/spawn/physics function call get_anti_patterns + get_preferences + list_patterns",
    "MANDATORY MID-TASK CORTEX USAGE: after first approach fails call recall <error_keyword> before retrying. After two failed attempts STOP and call recall or semantic_search before a third.",
    "session-end mandatory: when task verified complete, present Task Complete Summary and ask user to type KNOWLEDGE COMMITTED to trigger closeout_session(inline_approve=true)",
    "compact_output (MCP) losslessly strips only provably-redundant command output (build/download progress, per-test '... ok' lines == cargo -q, duplicate lines). Every error/warning/panic/failure is kept verbatim with its file:line, and the full original is tee'd to .cortex/tee/. It is post-processing of output you ALREADY ran — it is NOT a replacement for reading files or seeing diagnostics, and it never drops actionable content.",
    "Claude Code: the compact_output PostToolUse(Bash) hook AUTO-INSTALLS on the first cortex serve of a Claude Code project (into .claude/settings.local.json — personal/git-ignored, install-once via a .cortex/.claude-hooks-installed sentinel; removing the hook is respected and not re-added). Set CORTEX_NO_AUTO_HOOKS=1 to disable, or run 'cortex hooks-init --shared' to commit it for teammates. VS Code Copilot has NO output-rewriting hook mechanism — it cannot auto-compact; call the compact_output MCP tool directly instead (it is exposed via .vscode/mcp.json).",
]

[enforcement]
# "protocol_session_only" (default) or "always"
protocol_gate_mode = "protocol_session_only"
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
"#;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(db_path: &Path) -> Result<Self> {
        let is_new = !db_path.exists();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("could not open db: {}", db_path.display()))?;
        let store = Self { conn };
        store.migrate()?;
        if is_new {
            store.first_run_init(db_path)?;
        }
        Ok(store)
    }

    /// Called once when the DB file is created for the first time.
    /// Seeds core workflow anti-patterns, MCP tool annotations, and writes a prefs.toml template.
    fn first_run_init(&self, db_path: &Path) -> Result<()> {
        eprintln!("[cortex] First run — seeding workflow memory and creating prefs.toml");

        // ── Workflow anti-patterns ────────────────────────────────────────────
        let aps: &[(&str, &str, &str, &[&str])] = &[
            (
                "Skipping cortex recall after the first approach fails — proceeding to a second attempt without checking memory costs a full debug cycle when the answer is already recorded",
                "First approach failed; immediately try different approach without checking cortex",
                "First approach failed -> recall <error_keyword> or semantic_search <description> -> if nothing found, THEN try next approach and note the gap for crystallization",
                &["workflow", "meta", "cortex", "recall", "blocked", "debug-cycle"],
            ),
            (
                "Passing a multiline PowerShell string variable to an external exe CLI flag — each newline becomes a separate positional argument causing unexpected argument errors",
                "& cortex.exe --body $multiLineVar  // each line of var is a separate arg to the exe",
                "& cortex.exe --body 'Single line. No newlines.'  // all cortex CLI flag values must be single-line",
                &["powershell", "cli", "external-exe", "multiline", "cortex-cli", "string-args"],
            ),
            (
                "Using && for command chaining in PowerShell 5.1 — not a valid operator, causes parse errors; use semicolon or explicit LASTEXITCODE check",
                "cortex.exe status && cargo build  // && is bash syntax, not valid in PS 5.1",
                "cortex.exe status; cargo build  // semicolon for sequential; if ($LASTEXITCODE -eq 0) for conditional",
                &["powershell", "command-chaining", "syntax", "ps5", "bash-habit"],
            ),
            (
                "Em-dash unicode char in external CLI arg values from PowerShell — arg parsers may treat em-dash as flag separator, splitting the following word as a separate positional arg",
                "cortex.exe --body 'result is great - no issues'  // em-dash before a word: parser may read that word as a flag",
                "cortex.exe --body 'result is great - no issues'  // use ASCII hyphen-minus in all CLI arg values",
                &["powershell", "cli", "em-dash", "unicode", "string-args", "cortex-cli"],
            ),
        ];

        for (desc, wrong, correct, tags) in aps {
            let ap = AntiPattern {
                id: None,
                description: (*desc).to_string(),
                wrong: (*wrong).to_string(),
                correct: (*correct).to_string(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                added_at: Utc::now(),
            };
            self.insert_anti_pattern(&ap)?;
        }
        eprintln!("[cortex]   seeded {} workflow anti-patterns", aps.len());

        // ── MCP tool annotations ──────────────────────────────────────────────
        // These teach Copilot the exact params and usage for each cortex MCP tool.
        let annotations: &[(&str, &str, &[&str])] = &[
            (
                "MCP: semantic_search",
                "Params: query str required, limit int default=5. TF-IDF semantic plus keyword search across all indexed units. Returns top N units by relevance with compressed summaries. Use for finding which module handles a concept, discovering implementors of a trait, or locating code patterns. limit=3 for quick lookup, limit=8+ for exhaustive. Session cache deduplicates repeated content.",
                &["cortex", "mcp", "tools", "semantic_search"],
            ),
            (
                "MCP: get_item",
                "Params: name str required, case-sensitive exact match. Returns full compressed source for one indexed unit. Best for reading a specific struct/enum/trait when you know its exact name. Returns kind, module_path, and full compressed text. Fails with 'no item named X' on mismatch — use semantic_search first to find the exact name.",
                &["cortex", "mcp", "tools", "get_item"],
            ),
            (
                "MCP: get_context",
                "Params: hint str required, token_budget int default=2000, delta_include str, delta_exclude str, delta_max_files int default=8, delta_max_patch_lines int default=40. Builds context packet: relevant units + patterns + anti-patterns + annotations + git delta. Best single call to start a task. Use delta_exclude to filter noise like 'assets'. Raise token_budget to 4000 for complex tasks.",
                &["cortex", "mcp", "tools", "get_context"],
            ),
            (
                "MCP: get_delta",
                "Params: include str, exclude str, max_files int default=128, max_patch_lines int default=40, since str git-ref. Returns git diff as compressed entries: change type + path + summary + patch lines. Omit 'since' for working-tree HEAD diff. Use since='HEAD~5' for commit range. Use exclude='assets' to filter binary noise. Returns 'No git deltas found' if clean.",
                &["cortex", "mcp", "tools", "get_delta"],
            ),
            (
                "MCP: query_graph",
                "Params: name str required exact unit ID, depth int default=1. BFS traversal from node. Returns edges as 'source -[relation]-> target'. Relation types: Pairs, Conflicts, Owns, Uses, Calls, Implements, DerivedFrom. depth=1 direct neighbors, depth=2 two-hop impact, depth=3+ full blast radius. Returns 'No graph node found for X' if missing. Use before refactoring widely-used types.",
                &["cortex", "mcp", "tools", "query_graph"],
            ),
            (
                "MCP: get_preferences",
                "Params: none. Returns active prefs.toml summary loaded at server startup. Contains project-level coding rules, style constraints, import conventions. Read once per session. File location: .cortex/prefs.toml relative to repo root passed to 'cortex serve'. Returns 'No preferences configured' if missing.",
                &["cortex", "mcp", "tools", "get_preferences"],
            ),
            (
                "MCP: recurrent_think",
                "Params: task str required, hypothesis str, loop int, depth_mode str auto/shallow/deep default=auto, max_loops int default=6 max=16. Iterative hypothesis refinement. First call: provide task only to seed. Each loop: provide refined hypothesis, get critiques plus next_prompt plus confidence. Halt at confidence>=92% or max_loops. 'shallow' forces 2 loops, 'deep' allows up to 16. Scratchpad persisted in SQLite between calls.",
                &["cortex", "mcp", "tools", "recurrent_think"],
            ),
            (
                "MCP: simulate_change",
                "Params: item str required exact name, change str default='unspecified change', depth int default=1. Predicts impact of changing 'item'. Returns risk Low/Medium/High, affected modules, recommended actions. depth=1 direct deps, depth=2+ cascade. Use before modifying widely-used types. High risk = stop and confirm with user.",
                &["cortex", "mcp", "tools", "simulate_change"],
            ),
            (
                "MCP: recall",
                "Params: topic str required. Consolidated lookup across ALL memory layers: indexed units, patterns, anti-patterns, annotations. Best single call for 'what do we know about X'. Increments pattern use_count on match which affects survival_rate. Returns 'Nothing found' if no match — add an annotation in that case.",
                &["cortex", "mcp", "tools", "recall"],
            ),
            (
                "MCP: list_patterns",
                "Params: none. Returns all approved patterns with: name, intent, body, uses, survival_rate. Patterns with survival_rate<0.4 show a warning marker. Patterns with use_count=0 may be stale. Call at task start for a domain to see all relevant approved patterns at once rather than multiple recall calls. survival_rate = use_count / (use_count + reverted_count).",
                &["cortex", "mcp", "tools", "list_patterns"],
            ),
            (
                "MCP: get_anti_patterns",
                "Params: none. Returns ALL anti-patterns as wrong/correct pairs. ALWAYS call before generating code in a new domain. Call this at session start alongside get_preferences and list_patterns for the mandatory pre-code check.",
                &["cortex", "mcp", "tools", "get_anti_patterns"],
            ),
            (
                "MCP: suggest_pattern",
                "Params: name str, intent str, body str, uses array of str. Queues pattern as pending observation — does NOT auto-approve. Human must run 'cortex review' then 'cortex crystallize ID'. Use after verifying a pattern works in real code. Governance: suggest freely, approve deliberately.",
                &["cortex", "mcp", "tools", "suggest_pattern"],
            ),
            (
                "MCP: list_all",
                "Params: kind str optional enum/struct/trait/fn/type/const. Lists all indexed units filtered by kind, grouped by kind. Good for discovery when you don't know a type name. kind='enum' shows all enums. kind='struct' shows all structs. Includes scoped units (e.g. synful::) when indexed.",
                &["cortex", "mcp", "tools", "list_all"],
            ),
            (
                "MCP: compact_output",
                "Params: command str required, stdout str optional, stderr str optional. LOSSLESS command-output compaction: removes only provably-redundant lines (cargo build/download progress, per-test '... ok' lines == cargo -q, consecutive duplicate lines) and keeps EVERY error/warning/note/panic/failure verbatim with file:line. Full original tee'd to .cortex/tee/ whenever anything is dropped. Pass BOTH stdout and stderr — cargo/rustc write diagnostics to stderr. Does not execute anything (pure post-processing). Below ~800 chars it returns input untouched. Install as an automatic PostToolUse(Bash) hook via 'cortex hooks-init'.",
                &["cortex", "mcp", "tools", "compression"],
            ),
        ];

        for (topic, body, tags) in annotations {
            let ann = Annotation {
                id: None,
                topic: (*topic).to_string(),
                body: (*body).to_string(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                added_at: Utc::now(),
            };
            self.insert_annotation(&ann)?;
        }
        eprintln!("[cortex]   seeded {} MCP tool annotations", annotations.len());

        // ── prefs.toml template ───────────────────────────────────────────────
        if let Some(dir) = db_path.parent() {
            let prefs_path = dir.join("prefs.toml");
            if !prefs_path.exists() {
                std::fs::write(&prefs_path, PREFS_TEMPLATE)
                    .with_context(|| format!("could not write prefs.toml: {}", prefs_path.display()))?;
                eprintln!("[cortex]   created prefs.toml — edit [project].name and add your API notes");
            }
        }

        eprintln!("[cortex] First-run setup complete. See README.md for the recommended copilot-instructions.md snippet.");
        Ok(())
    }

    /// Expose the connection for cache operations.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS code_units (
                id          TEXT PRIMARY KEY,
                kind        TEXT NOT NULL,
                name        TEXT NOT NULL,
                module_path TEXT NOT NULL,
                summary     TEXT NOT NULL,
                compressed  TEXT NOT NULL,
                term_vector TEXT NOT NULL,  -- JSON array of [term, weight] pairs
                indexed_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS code_members (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                parent_id   TEXT NOT NULL REFERENCES code_units(id) ON DELETE CASCADE,
                kind        TEXT NOT NULL,
                name        TEXT NOT NULL,
                type_sig    TEXT NOT NULL,
                doc         TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_members_parent ON code_members(parent_id);
            CREATE INDEX IF NOT EXISTS idx_units_name ON code_units(name);
            CREATE INDEX IF NOT EXISTS idx_units_kind ON code_units(kind);

            CREATE TABLE IF NOT EXISTS patterns (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL,
                intent      TEXT NOT NULL,
                body        TEXT NOT NULL,
                uses        TEXT NOT NULL,  -- JSON array
                tags        TEXT NOT NULL,  -- JSON array
                approved_at TEXT NOT NULL,
                use_count   INTEGER NOT NULL DEFAULT 0,
                reverted_count INTEGER NOT NULL DEFAULT 0,
                survival_rate REAL NOT NULL DEFAULT 1.0
            );

            CREATE TABLE IF NOT EXISTS anti_patterns (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                description TEXT NOT NULL,
                wrong       TEXT NOT NULL,
                correct     TEXT NOT NULL,
                tags        TEXT NOT NULL,  -- JSON array
                added_at    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS annotations (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                topic       TEXT NOT NULL,
                body        TEXT NOT NULL,
                tags        TEXT NOT NULL,  -- JSON array
                added_at    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS mcp_calls (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                tool        TEXT NOT NULL,
                args        TEXT NOT NULL,
                called_at   TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pending_observations (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                path        TEXT NOT NULL,
                summary     TEXT NOT NULL,
                diff_hint   TEXT NOT NULL,
                observed_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS graph_nodes (
                id          TEXT PRIMARY KEY,
                kind        TEXT NOT NULL,
                name        TEXT NOT NULL,
                module_path TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS graph_edges (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id     TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
                to_id       TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
                relation    TEXT NOT NULL,
                weight      REAL NOT NULL DEFAULT 1.0,
                source      TEXT NOT NULL,
                UNIQUE (from_id, to_id, relation)
            );

            CREATE INDEX IF NOT EXISTS idx_edges_from ON graph_edges(from_id);
            CREATE INDEX IF NOT EXISTS idx_edges_to ON graph_edges(to_id);

            CREATE TABLE IF NOT EXISTS scratchpads (
                id          TEXT PRIMARY KEY,
                task        TEXT NOT NULL,
                state_json  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_scratchpads_updated ON scratchpads(updated_at);
        ")?;
        // Cache tables (managed by cache module)
        crate::cache::migrate(&self.conn)?;

        // Backfill Phase 4 pattern-evolution columns for existing DBs.
        self.ensure_pattern_evolution_columns()?;

        // FTS5 tables, new schema tables, and drift columns (idempotent).
        self.ensure_fts_and_new_tables()?;

        Ok(())
    }

    fn ensure_fts_and_new_tables(&self) -> Result<()> {
        // FTS5 content-indexed virtual tables for BM25 keyword search.
        self.conn.execute_batch("
            CREATE VIRTUAL TABLE IF NOT EXISTS pattern_fts USING fts5(
                name, intent, body, tags,
                content = 'patterns',
                content_rowid = 'id',
                tokenize = 'porter unicode61'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS anti_pattern_fts USING fts5(
                description, wrong, correct, tags,
                content = 'anti_patterns',
                content_rowid = 'id',
                tokenize = 'porter unicode61'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS annotation_fts USING fts5(
                topic, body, tags,
                content = 'annotations',
                content_rowid = 'id',
                tokenize = 'porter unicode61'
            );
        ")?;

        // FTS sync triggers — each trigger is its own execute_batch call.
        let triggers = [
            "CREATE TRIGGER IF NOT EXISTS trg_pat_fts_ins AFTER INSERT ON patterns BEGIN
                INSERT INTO pattern_fts(rowid, name, intent, body, tags)
                VALUES (NEW.id, NEW.name, NEW.intent, NEW.body, NEW.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_pat_fts_del AFTER DELETE ON patterns BEGIN
                INSERT INTO pattern_fts(pattern_fts, rowid, name, intent, body, tags)
                VALUES ('delete', OLD.id, OLD.name, OLD.intent, OLD.body, OLD.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_pat_fts_upd AFTER UPDATE ON patterns BEGIN
                INSERT INTO pattern_fts(pattern_fts, rowid, name, intent, body, tags)
                VALUES ('delete', OLD.id, OLD.name, OLD.intent, OLD.body, OLD.tags);
                INSERT INTO pattern_fts(rowid, name, intent, body, tags)
                VALUES (NEW.id, NEW.name, NEW.intent, NEW.body, NEW.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ap_fts_ins AFTER INSERT ON anti_patterns BEGIN
                INSERT INTO anti_pattern_fts(rowid, description, wrong, correct, tags)
                VALUES (NEW.id, NEW.description, NEW.wrong, NEW.correct, NEW.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ap_fts_del AFTER DELETE ON anti_patterns BEGIN
                INSERT INTO anti_pattern_fts(anti_pattern_fts, rowid, description, wrong, correct, tags)
                VALUES ('delete', OLD.id, OLD.description, OLD.wrong, OLD.correct, OLD.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ap_fts_upd AFTER UPDATE ON anti_patterns BEGIN
                INSERT INTO anti_pattern_fts(anti_pattern_fts, rowid, description, wrong, correct, tags)
                VALUES ('delete', OLD.id, OLD.description, OLD.wrong, OLD.correct, OLD.tags);
                INSERT INTO anti_pattern_fts(rowid, description, wrong, correct, tags)
                VALUES (NEW.id, NEW.description, NEW.wrong, NEW.correct, NEW.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ann_fts_ins AFTER INSERT ON annotations BEGIN
                INSERT INTO annotation_fts(rowid, topic, body, tags)
                VALUES (NEW.id, NEW.topic, NEW.body, NEW.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ann_fts_del AFTER DELETE ON annotations BEGIN
                INSERT INTO annotation_fts(annotation_fts, rowid, topic, body, tags)
                VALUES ('delete', OLD.id, OLD.topic, OLD.body, OLD.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_ann_fts_upd AFTER UPDATE ON annotations BEGIN
                INSERT INTO annotation_fts(annotation_fts, rowid, topic, body, tags)
                VALUES ('delete', OLD.id, OLD.topic, OLD.body, OLD.tags);
                INSERT INTO annotation_fts(rowid, topic, body, tags)
                VALUES (NEW.id, NEW.topic, NEW.body, NEW.tags);
            END",
        ];
        for t in &triggers {
            self.conn.execute_batch(t)?;
        }

        // New data tables.
        self.conn.execute_batch("
            CREATE TABLE IF NOT EXISTS pattern_merge_log (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                kept_id          INTEGER NOT NULL,
                merged_id        INTEGER NOT NULL,
                similarity_score REAL NOT NULL,
                merge_reason     TEXT NOT NULL,
                merged_at        TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS self_corrections (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                attempted        TEXT NOT NULL,
                failure_reason   TEXT NOT NULL,
                correction       TEXT NOT NULL,
                tags             TEXT NOT NULL DEFAULT '[]',
                occurrence_count INTEGER NOT NULL DEFAULT 1,
                first_seen_at    TEXT NOT NULL,
                last_seen_at     TEXT NOT NULL,
                UNIQUE(attempted, failure_reason)
            );
            CREATE INDEX IF NOT EXISTS idx_corrections_last ON self_corrections(last_seen_at);

            CREATE TABLE IF NOT EXISTS adrs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                adr_number    INTEGER NOT NULL UNIQUE,
                title         TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'accepted',
                context       TEXT NOT NULL,
                decision      TEXT NOT NULL,
                reasoning     TEXT NOT NULL,
                alternatives  TEXT NOT NULL DEFAULT '',
                consequences  TEXT NOT NULL DEFAULT '',
                concept_tags  TEXT NOT NULL DEFAULT '[]',
                superseded_by INTEGER REFERENCES adrs(id),
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_adr_status ON adrs(status);
            CREATE INDEX IF NOT EXISTS idx_adr_number ON adrs(adr_number);

            CREATE TABLE IF NOT EXISTS adr_tag_index (
                tag    TEXT NOT NULL,
                adr_id INTEGER NOT NULL REFERENCES adrs(id) ON DELETE CASCADE,
                PRIMARY KEY(tag, adr_id)
            );
            CREATE INDEX IF NOT EXISTS idx_adr_tag ON adr_tag_index(tag);

            CREATE TABLE IF NOT EXISTS pattern_unit_refs (
                pattern_id INTEGER NOT NULL REFERENCES patterns(id) ON DELETE CASCADE,
                unit_id    TEXT NOT NULL,
                PRIMARY KEY(pattern_id, unit_id)
            );
            CREATE INDEX IF NOT EXISTS idx_pur_unit ON pattern_unit_refs(unit_id);

            CREATE TABLE IF NOT EXISTS symbol_catalog (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol_name     TEXT    NOT NULL UNIQUE,
                kind            TEXT    NOT NULL,
                module_path     TEXT    NOT NULL,
                signature       TEXT,
                return_type     TEXT,
                fields_json     TEXT,
                methods_json    TEXT,
                variants_json   TEXT,
                helper_tags     TEXT    NOT NULL DEFAULT '',
                source_tier     TEXT    NOT NULL DEFAULT 'index',
                last_seen_at    INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_sc_kind ON symbol_catalog(kind);
            CREATE INDEX IF NOT EXISTS idx_sc_module ON symbol_catalog(module_path);

            CREATE TABLE IF NOT EXISTS symbol_examples (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol_name     TEXT    NOT NULL,
                file_path       TEXT    NOT NULL,
                line_number     INTEGER,
                example_snippet TEXT    NOT NULL,
                source_tier     TEXT    NOT NULL DEFAULT 'production_fn',
                created_at      INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_se_symbol ON symbol_examples(symbol_name);

            CREATE TABLE IF NOT EXISTS session_retrieval_log (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id    TEXT    NOT NULL,
                entry_table   TEXT    NOT NULL,
                entry_id      INTEGER NOT NULL,
                tool_name     TEXT    NOT NULL,
                retrieved_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_srl_session ON session_retrieval_log(session_id);
            CREATE INDEX IF NOT EXISTS idx_srl_tool ON session_retrieval_log(tool_name);

            CREATE TABLE IF NOT EXISTS outcome_log (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id    TEXT    NOT NULL,
                outcome_type  TEXT    NOT NULL,
                error_text    TEXT,
                diff_symbols  TEXT,
                created_at    INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_ol_session ON outcome_log(session_id);
            CREATE INDEX IF NOT EXISTS idx_ol_type ON outcome_log(outcome_type);

            CREATE TABLE IF NOT EXISTS outcome_applied_session (
                session_id    TEXT PRIMARY KEY,
                applied_at    INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE TABLE IF NOT EXISTS outcome_applied_log (
                outcome_id    INTEGER PRIMARY KEY REFERENCES outcome_log(id) ON DELETE CASCADE,
                session_id    TEXT    NOT NULL,
                applied_at    INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_oal_session ON outcome_applied_log(session_id);

            CREATE TABLE IF NOT EXISTS query_gap_log (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                tool_name     TEXT    NOT NULL,
                query_text    TEXT    NOT NULL,
                session_id    TEXT,
                seen_count    INTEGER NOT NULL DEFAULT 1,
                last_reason   TEXT,
                first_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
                last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(tool_name, query_text)
            );
            CREATE INDEX IF NOT EXISTS idx_qgl_seen ON query_gap_log(seen_count DESC);
            CREATE INDEX IF NOT EXISTS idx_qgl_last_seen ON query_gap_log(last_seen_at DESC);

            CREATE TABLE IF NOT EXISTS call_graph (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                caller      TEXT    NOT NULL,
                callee      TEXT    NOT NULL,
                edge_type   TEXT    NOT NULL DEFAULT 'direct',
                file_path   TEXT,
                line_number INTEGER,
                weight      REAL    NOT NULL DEFAULT 1.0,
                source      TEXT    NOT NULL DEFAULT 'inferred',
                UNIQUE(caller, callee, edge_type)
            );
            CREATE INDEX IF NOT EXISTS idx_cg_caller ON call_graph(caller);
            CREATE INDEX IF NOT EXISTS idx_cg_callee ON call_graph(callee);
        ")?;

        // Phase 0A: self-learning loop tables (idempotent).
        self.conn.execute_batch("
            -- Protocol session tracking (persists across MCP server restarts).
            CREATE TABLE IF NOT EXISTS protocol_sessions (
                session_key                TEXT PRIMARY KEY,
                started_at                 INTEGER NOT NULL DEFAULT (unixepoch()),
                protocol_mode              INTEGER NOT NULL DEFAULT 0,
                delta_retrieved            INTEGER NOT NULL DEFAULT 0,
                preferences_loaded         INTEGER NOT NULL DEFAULT 0,
                anti_patterns_loaded       INTEGER NOT NULL DEFAULT 0,
                context_loaded             INTEGER NOT NULL DEFAULT 0,
                bootstrap_complete         INTEGER NOT NULL DEFAULT 0,
                closeout_run               INTEGER NOT NULL DEFAULT 0,
                outcome_type               TEXT,
                closed_at                  INTEGER,
                knowledge_markers_flushed  INTEGER NOT NULL DEFAULT 0,
                inline_approved            INTEGER NOT NULL DEFAULT 0,
                graph_snapshot_written     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_ps_started ON protocol_sessions(started_at DESC);

            -- Knowledge markers: extracted CORTEX-* tags from session turns.
            CREATE TABLE IF NOT EXISTS knowledge_markers (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key  TEXT NOT NULL,
                marker_type  TEXT NOT NULL,
                name         TEXT,
                intent       TEXT,
                body         TEXT NOT NULL,
                tags         TEXT NOT NULL DEFAULT '[]',
                trust_level  TEXT NOT NULL DEFAULT 'annotated',
                raw_tag      TEXT NOT NULL DEFAULT '',
                extracted_at INTEGER NOT NULL DEFAULT (unixepoch()),
                promoted     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_km_session ON knowledge_markers(session_key);
            CREATE INDEX IF NOT EXISTS idx_km_type ON knowledge_markers(marker_type);
            CREATE INDEX IF NOT EXISTS idx_km_promoted ON knowledge_markers(promoted);

            -- Skill candidates detected from repeated mcp_calls sequences.
            CREATE TABLE IF NOT EXISTS skill_candidates (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                name             TEXT NOT NULL,
                trigger_hint     TEXT NOT NULL DEFAULT '',
                tool_sequence    TEXT NOT NULL DEFAULT '[]',
                session_keys     TEXT NOT NULL DEFAULT '[]',
                occurrence_count INTEGER NOT NULL DEFAULT 1,
                confidence       REAL NOT NULL DEFAULT 0.0,
                draft_path       TEXT,
                status           TEXT NOT NULL DEFAULT 'candidate',
                first_seen_at    INTEGER NOT NULL DEFAULT (unixepoch()),
                last_seen_at     INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(name)
            );
            CREATE INDEX IF NOT EXISTS idx_sc_status ON skill_candidates(status);

            -- Consolidation proposals (Tier 2: cross-session, reviewed via review-proposals).
            CREATE TABLE IF NOT EXISTS proposals (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                proposal_type TEXT NOT NULL,
                content_hash  TEXT NOT NULL UNIQUE,
                target_file   TEXT NOT NULL,
                section       TEXT,
                proposed_text TEXT NOT NULL,
                evidence      TEXT NOT NULL DEFAULT '{}',
                status        TEXT NOT NULL DEFAULT 'pending',
                gate_signals  TEXT NOT NULL DEFAULT '{}',
                created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
                reviewed_at   INTEGER,
                committed_at  INTEGER,
                rejected_at   INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_prop_status ON proposals(status);
            CREATE INDEX IF NOT EXISTS idx_prop_type ON proposals(proposal_type);
            CREATE INDEX IF NOT EXISTS idx_prop_hash ON proposals(content_hash);

            -- Session snapshots index for consolidation pipeline.
            CREATE TABLE IF NOT EXISTS session_snapshots (
                session_key       TEXT PRIMARY KEY,
                outcome_type      TEXT,
                domain_tags       TEXT NOT NULL DEFAULT '[]',
                tool_sequence     TEXT NOT NULL DEFAULT '[]',
                marker_counts     TEXT NOT NULL DEFAULT '{}',
                user_message_hash TEXT,
                snapshot_path     TEXT NOT NULL DEFAULT '',
                created_at        INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_ss_outcome ON session_snapshots(outcome_type);

            CREATE TABLE IF NOT EXISTS compression_savings (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key    TEXT NOT NULL,
                command        TEXT NOT NULL,
                original_chars INTEGER NOT NULL,
                filtered_chars INTEGER NOT NULL,
                ratio          REAL NOT NULL,
                saved_at       INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_savings_session ON compression_savings(session_key);
        ")?;

        // Drift-detection columns on code_units (idempotent).
        self.ensure_unit_drift_columns()?;

        // Phase 0A: session tracking columns (idempotent ALTER TABLE).
        self.ensure_session_tracking_columns()?;

        // Backfill legacy session-level outcome markers into per-outcome ledger.
        // This preserves prior evidence application semantics and prevents re-application.
        let migrated_outcome_rows = self.backfill_legacy_outcome_application()?;
        if migrated_outcome_rows > 0 {
            eprintln!(
                "[cortex] legacy outcome application markers detected; migrated {} row(s) to outcome_applied_log",
                migrated_outcome_rows
            );
            eprintln!(
                "[cortex] migration verification: run .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json"
            );
        }

        // Rebuild FTS index from existing data (safe to call repeatedly — replaces stale entries).
        self.rebuild_fts()?;

        Ok(())
    }

    fn backfill_legacy_outcome_application(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO outcome_applied_log (outcome_id, session_id, applied_at)
             SELECT o.id, o.session_id, COALESCE(s.applied_at, o.created_at, unixepoch())
             FROM outcome_log o
             JOIN outcome_applied_session s ON s.session_id = o.session_id",
            [],
        )?;
        Ok(changed)
    }

    fn ensure_unit_drift_columns(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(code_units)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cols = std::collections::HashSet::new();
        for c in rows { cols.insert(c?); }

        if !cols.contains("previous_compressed") {
            self.conn.execute(
                "ALTER TABLE code_units ADD COLUMN previous_compressed TEXT",
                [],
            )?;
        }
        if !cols.contains("signature_changed_at") {
            self.conn.execute(
                "ALTER TABLE code_units ADD COLUMN signature_changed_at TEXT",
                [],
            )?;
        }
        // Provenance: which source root produced this unit. Indexing is
        // INSERT OR REPLACE with no delete, so units from sources that were
        // renamed or dropped from index-sources.json lingered forever — the live
        // index was serving 94 units of cortex's own source and 35 from `air_src`,
        // neither of them configured. NULL means "indexed before provenance
        // existed", which after one full reindex is exactly the orphan set.
        if !cols.contains("source_root") {
            self.conn.execute(
                "ALTER TABLE code_units ADD COLUMN source_root TEXT",
                [],
            )?;
        }
        Ok(())
    }

    /// Units grouped by the source root that produced them.
    /// `None` covers rows indexed before provenance stamping existed.
    pub fn units_by_source(&self) -> Result<Vec<(Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_root, COUNT(*) FROM code_units GROUP BY source_root ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete units that no configured source root claims.
    ///
    /// `keep` is the set of source roots from the current index configuration.
    /// Rows whose `source_root` is NULL or absent from `keep` are removed, along
    /// with their members and graph nodes. Returns the number of units deleted.
    pub fn prune_orphan_units(&self, keep: &[String]) -> Result<usize> {
        let placeholders = if keep.is_empty() {
            "''".to_string()
        } else {
            keep.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        };
        let sql = format!(
            "SELECT id FROM code_units \
             WHERE source_root IS NULL OR source_root NOT IN ({placeholders})"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            keep.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for id in &ids {
            self.conn.execute("DELETE FROM code_members WHERE parent_id = ?1", params![id])?;
            self.conn.execute("DELETE FROM graph_edges WHERE from_id = ?1 OR to_id = ?1", params![id])?;
            self.conn.execute("DELETE FROM graph_nodes WHERE id = ?1", params![id])?;
            self.conn.execute("DELETE FROM code_units WHERE id = ?1", params![id])?;
        }
        Ok(ids.len())
    }

    /// Phase 0A: add logical_session_key to mcp_calls and credibility to patterns.
    fn ensure_session_tracking_columns(&self) -> Result<()> {
        // mcp_calls: logical_session_key groups calls within a 2-hour inactivity window.
        let mut stmt = self.conn.prepare("PRAGMA table_info(mcp_calls)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cols = std::collections::HashSet::new();
        for c in rows { cols.insert(c?); }

        if !cols.contains("logical_session_key") {
            self.conn.execute(
                "ALTER TABLE mcp_calls ADD COLUMN logical_session_key TEXT",
                [],
            )?;
        }

        // patterns: credibility = min(use_count, 10) / 10.0
        // Provides Bayesian-style trust signal: survival_rate=1.0 is uninformative at use_count=0.
        let mut stmt = self.conn.prepare("PRAGMA table_info(patterns)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cols2 = std::collections::HashSet::new();
        for c in rows { cols2.insert(c?); }

        if !cols2.contains("credibility") {
            self.conn.execute(
                "ALTER TABLE patterns ADD COLUMN credibility REAL NOT NULL DEFAULT 0.0",
                [],
            )?;
            // Backfill credibility for existing patterns.
            self.conn.execute(
                "UPDATE patterns SET credibility = CAST(MIN(use_count, 10) AS REAL) / 10.0",
                [],
            )?;
        }

        Ok(())
    }

    /// Rebuild FTS5 indexes from source tables. Safe to call repeatedly.
    pub fn rebuild_fts(&self) -> Result<()> {
        // 'rebuild' re-reads the content table and regenerates the inverted index.
        self.conn.execute_batch("
            INSERT INTO pattern_fts(pattern_fts) VALUES('rebuild');
            INSERT INTO anti_pattern_fts(anti_pattern_fts) VALUES('rebuild');
            INSERT INTO annotation_fts(annotation_fts) VALUES('rebuild');
        ")?;
        Ok(())
    }

    fn ensure_pattern_evolution_columns(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(patterns)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cols = std::collections::HashSet::new();
        for c in rows {
            cols.insert(c?);
        }

        if !cols.contains("reverted_count") {
            self.conn.execute(
                "ALTER TABLE patterns ADD COLUMN reverted_count INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !cols.contains("survival_rate") {
            self.conn.execute(
                "ALTER TABLE patterns ADD COLUMN survival_rate REAL NOT NULL DEFAULT 1.0",
                [],
            )?;
        }

        self.conn.execute(
            "UPDATE patterns
             SET survival_rate = CASE
                WHEN use_count <= 0 AND reverted_count > 0 THEN 0.0
                WHEN use_count <= 0 THEN 1.0
                ELSE MAX(0.0, CAST(use_count - reverted_count AS REAL) / CAST(use_count AS REAL))
             END",
            [],
        )?;
        Ok(())
    }

    // ── Code units ────────────────────────────────────────────────────────────

    pub fn upsert_unit(&self, unit: &CodeUnit) -> Result<()> {
        self.upsert_unit_from(unit, None)
    }

    /// Upsert a unit, recording which source root produced it.
    pub fn upsert_unit_from(&self, unit: &CodeUnit, source_root: Option<&str>) -> Result<()> {
        let tv_json = serde_json::to_string(&unit.term_vector)?;
        let now = chrono::Utc::now().to_rfc3339();

        // Detect signature drift: read existing compressed before replacing.
        let existing: Option<(String, Option<String>)> = self.conn.query_row(
            "SELECT compressed, previous_compressed FROM code_units WHERE id = ?1",
            params![&unit.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;

        let (prev_compressed, signature_changed_at): (Option<String>, Option<String>) =
            if let Some((old_comp, _prev)) = existing {
                if old_comp != unit.compressed {
                    (Some(old_comp), Some(now.clone()))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        self.conn.execute(
            "INSERT OR REPLACE INTO code_units
             (id, kind, name, module_path, summary, compressed, term_vector, indexed_at,
              previous_compressed, signature_changed_at, source_root)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                unit.id, unit.kind, unit.name, unit.module_path,
                unit.summary, unit.compressed, tv_json, now,
                prev_compressed,
                signature_changed_at,
                source_root,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_member(&self, m: &CodeMember) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO code_members (parent_id, kind, name, type_sig, doc)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![m.parent_id, m.kind, m.name, m.type_sig, m.doc],
        )?;
        Ok(())
    }

    pub fn get_unit(&self, name: &str) -> Result<Option<CodeUnit>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, module_path, summary, compressed, term_vector, indexed_at
             FROM code_units WHERE name = ?1 LIMIT 1"
        )?;
        let mut rows = stmt.query_map(params![name], row_to_unit)?;
        Ok(rows.next().transpose()?)
    }

    pub fn all_units(&self) -> Result<Vec<CodeUnit>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, module_path, summary, compressed, term_vector, indexed_at
             FROM code_units ORDER BY kind, name"
        )?;
        let rows = stmt.query_map([], row_to_unit)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn units_by_kind(&self, kind: &str) -> Result<Vec<CodeUnit>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, module_path, summary, compressed, term_vector, indexed_at
             FROM code_units WHERE kind = ?1 ORDER BY name"
        )?;
        let rows = stmt.query_map(params![kind], row_to_unit)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn members_of(&self, parent_id: &str) -> Result<Vec<CodeMember>> {
        let mut stmt = self.conn.prepare(
            "SELECT parent_id, kind, name, type_sig, doc FROM code_members WHERE parent_id = ?1"
        )?;
        let rows = stmt.query_map(params![parent_id], |row| {
            Ok(CodeMember {
                parent_id: row.get(0)?,
                kind:      row.get(1)?,
                name:      row.get(2)?,
                type_sig:  row.get(3)?,
                doc:       row.get(4)?,
            })
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn unit_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM code_units", [], |r| r.get(0))?)
    }

    // ── Patterns ──────────────────────────────────────────────────────────────

    pub fn insert_pattern(&self, p: &Pattern) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO patterns
             (name, intent, body, uses, tags, approved_at, use_count, reverted_count, survival_rate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 1.0)",
            params![
                p.name, p.intent, p.body,
                serde_json::to_string(&p.uses)?,
                serde_json::to_string(&p.tags)?,
                p.approved_at.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_patterns(&self) -> Result<Vec<Pattern>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, intent, body, uses, tags, approved_at, use_count,
                    reverted_count, survival_rate
             FROM patterns ORDER BY survival_rate DESC, use_count DESC, approved_at DESC"
        )?;
        let rows = stmt.query_map([], row_to_pattern)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn pattern_used(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE patterns SET use_count = use_count + 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn delete_pattern(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM patterns WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn pattern_reverted(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE patterns
             SET reverted_count = reverted_count + 1
             WHERE id = ?1",
            params![id],
        )?;
        self.recompute_pattern_survival(id)
    }

    pub fn recompute_pattern_survival(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE patterns
             SET survival_rate = CASE
                WHEN use_count <= 0 AND reverted_count > 0 THEN 0.0
                WHEN use_count <= 0 THEN 1.0
                ELSE MAX(0.0, CAST(use_count - reverted_count AS REAL) / CAST(use_count AS REAL))
             END
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn pattern_health_rows(&self) -> Result<Vec<(i64, String, i64, i64, f32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, use_count, reverted_count, survival_rate
             FROM patterns
             ORDER BY survival_rate DESC, use_count DESC, approved_at DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, f32>(4)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn graph_counts(&self) -> Result<(i64, i64, i64, i64)> {
        let nodes: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_nodes", [], |r| r.get(0))?;
        let edges: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |r| r.get(0))?;
        let inferred: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE source = 'inferred'",
                [],
                |r| r.get(0),
            )?;
        let manual: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE source = 'manual'",
                [],
                |r| r.get(0),
            )?;
        Ok((nodes, edges, inferred, manual))
    }

    pub fn scratchpad_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM scratchpads", [], |r| r.get(0))
            .map_err(Into::into)
    }

    pub fn hot_tools_recent(&self, recent_limit: usize, top_n: usize) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool, COUNT(*) as n FROM (
                SELECT tool FROM mcp_calls ORDER BY id DESC LIMIT ?1
             ) recent
             GROUP BY tool
             ORDER BY n DESC
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![recent_limit as i64, top_n as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── Anti-patterns ─────────────────────────────────────────────────────────

    pub fn insert_anti_pattern(&self, ap: &AntiPattern) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO anti_patterns (description, wrong, correct, tags, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                ap.description, ap.wrong, ap.correct,
                serde_json::to_string(&ap.tags)?,
                ap.added_at.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_anti_patterns(&self) -> Result<Vec<AntiPattern>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, description, wrong, correct, tags, added_at FROM anti_patterns"
        )?;
        let rows = stmt.query_map([], row_to_anti_pattern)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn delete_anti_pattern(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM anti_patterns WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Annotations ───────────────────────────────────────────────────────────

    pub fn insert_annotation(&self, a: &Annotation) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO annotations (topic, body, tags, added_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                a.topic, a.body,
                serde_json::to_string(&a.tags)?,
                a.added_at.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_annotations(&self) -> Result<Vec<Annotation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, topic, body, tags, added_at FROM annotations ORDER BY added_at DESC"
        )?;
        let rows = stmt.query_map([], row_to_annotation)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn delete_annotation(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM annotations WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn upsert_symbol_catalog_from_unit(&self, unit: &CodeUnit) -> Result<()> {
        self.conn.execute(
            "INSERT INTO symbol_catalog
                (symbol_name, kind, module_path, signature, return_type,
                 fields_json, methods_json, variants_json, helper_tags,
                 source_tier, last_seen_at)
             VALUES
                (?1, ?2, ?3, ?4, NULL, NULL, NULL, NULL, ?5, 'index', unixepoch())
             ON CONFLICT(symbol_name) DO UPDATE SET
                kind = excluded.kind,
                module_path = excluded.module_path,
                signature = excluded.signature,
                helper_tags = excluded.helper_tags,
                source_tier = excluded.source_tier,
                last_seen_at = unixepoch()",
            params![
                unit.id,
                unit.kind,
                unit.module_path,
                unit.summary,
                unit.name,
            ],
        )?;
        Ok(())
    }

    pub fn add_symbol_example_if_missing(
        &self,
        symbol_name: &str,
        file_path: &str,
        line_number: Option<i64>,
        example_snippet: &str,
        source_tier: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO symbol_examples
                (symbol_name, file_path, line_number, example_snippet, source_tier, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5, unixepoch()
             WHERE NOT EXISTS (
                SELECT 1 FROM symbol_examples
                WHERE symbol_name = ?1 AND file_path = ?2 AND source_tier = ?5
             )",
            params![symbol_name, file_path, line_number, example_snippet, source_tier],
        )?;
        Ok(())
    }

    pub fn get_symbol_examples(
        &self,
        symbol_name: &str,
        source_tier: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Option<i64>, String, String)>> {
        let like_suffix = format!("%::{}", symbol_name);

        let out = if let Some(tier) = source_tier {
            let mut stmt = self.conn.prepare(
                "SELECT file_path, line_number, example_snippet, source_tier
                 FROM symbol_examples
                 WHERE (symbol_name = ?1 OR lower(symbol_name) = lower(?1) OR symbol_name LIKE ?2)
                   AND source_tier = ?3
                 ORDER BY created_at DESC
                 LIMIT ?4"
            )?;
            let rows = stmt.query_map(params![symbol_name, like_suffix, tier, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT file_path, line_number, example_snippet, source_tier
                 FROM symbol_examples
                 WHERE symbol_name = ?1 OR lower(symbol_name) = lower(?1) OR symbol_name LIKE ?2
                 ORDER BY created_at DESC
                 LIMIT ?3"
            )?;
            let rows = stmt.query_map(params![symbol_name, like_suffix, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        Ok(out)
    }

    pub fn get_symbol_catalog_entry(
        &self,
        symbol_name: &str,
    ) -> Result<Option<(String, String, String, Option<String>, Option<String>, String)>> {
        let like_suffix = format!("%::{}", symbol_name);
        self.conn.query_row(
            "SELECT symbol_name, kind, module_path, signature, return_type, helper_tags
             FROM symbol_catalog
             WHERE symbol_name = ?1 OR lower(symbol_name) = lower(?1) OR symbol_name LIKE ?2
             ORDER BY
                CASE
                    WHEN symbol_name = ?1 THEN 0
                    WHEN lower(symbol_name) = lower(?1) THEN 1
                    WHEN symbol_name LIKE ?2 THEN 2
                    ELSE 3
                END,
                last_seen_at DESC
             LIMIT 1",
            params![symbol_name, like_suffix],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        ).optional().map_err(Into::into)
    }

    pub fn find_symbol_catalog_similar(
        &self,
        symbol_name: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let like = format!("%{}%", symbol_name);
        let mut stmt = self.conn.prepare(
            "SELECT symbol_name, kind, module_path
             FROM symbol_catalog
             WHERE symbol_name LIKE ?1 OR lower(symbol_name) LIKE lower(?1)
             ORDER BY last_seen_at DESC
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![like, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── MCP call log ──────────────────────────────────────────────────────────

    pub fn log_mcp_call(&self, tool: &str, args: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO mcp_calls (tool, args, called_at) VALUES (?1, ?2, ?3)",
            params![tool, args, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a lossless-compaction saving for telemetry. Non-fatal by
    /// contract — callers ignore the Result so a logging failure never breaks
    /// the compaction itself.
    pub fn log_compression_saving(
        &self,
        session_key: &str,
        command: &str,
        original_chars: usize,
        filtered_chars: usize,
    ) -> Result<()> {
        let ratio = if original_chars > 0 {
            filtered_chars as f64 / original_chars as f64
        } else {
            1.0
        };
        self.conn.execute(
            "INSERT INTO compression_savings
                (session_key, command, original_chars, filtered_chars, ratio)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_key, command, original_chars as i64, filtered_chars as i64, ratio],
        )?;
        Ok(())
    }

    pub fn log_session_retrieval(
        &self,
        session_id: &str,
        entry_table: &str,
        entry_id: i64,
        tool_name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_retrieval_log
                (session_id, entry_table, entry_id, tool_name, retrieved_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![session_id, entry_table, entry_id, tool_name],
        )?;
        Ok(())
    }

    pub fn log_outcome(
        &self,
        session_id: &str,
        outcome_type: &str,
        error_text: Option<&str>,
        diff_symbols: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO outcome_log
                (session_id, outcome_type, error_text, diff_symbols, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![session_id, outcome_type, error_text, diff_symbols],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn outcome_session_applied(&self, session_id: &str) -> Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM outcome_applied_session WHERE session_id = ?1 LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    pub fn mark_outcome_session_applied(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO outcome_applied_session (session_id, applied_at)
             VALUES (?1, unixepoch())
             ON CONFLICT(session_id) DO UPDATE SET applied_at = unixepoch()",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn pending_outcomes_for_session(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.id, o.outcome_type
             FROM outcome_log o
             LEFT JOIN outcome_applied_log a ON a.outcome_id = o.id
             WHERE o.session_id = ?1
               AND a.outcome_id IS NULL
             ORDER BY o.id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn mark_outcomes_applied(&self, session_id: &str, outcome_ids: &[i64]) -> Result<usize> {
        if outcome_ids.is_empty() {
            return Ok(0);
        }

        let mut applied = 0usize;
        for outcome_id in outcome_ids {
            let changed = self.conn.execute(
                "INSERT OR IGNORE INTO outcome_applied_log (outcome_id, session_id, applied_at)
                 VALUES (?1, ?2, unixepoch())",
                params![outcome_id, session_id],
            )?;
            applied += changed as usize;
        }
        Ok(applied)
    }

    pub fn log_query_gap(
        &self,
        tool_name: &str,
        query_text: &str,
        session_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        let normalized = query_text.trim();
        if normalized.is_empty() {
            return Ok(());
        }

        self.conn.execute(
            "INSERT INTO query_gap_log
                (tool_name, query_text, session_id, seen_count, last_reason, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, 1, ?4, unixepoch(), unixepoch())
             ON CONFLICT(tool_name, query_text) DO UPDATE SET
                seen_count = query_gap_log.seen_count + 1,
                session_id = excluded.session_id,
                last_reason = excluded.last_reason,
                last_seen_at = unixepoch()",
            params![tool_name, normalized, session_id, reason],
        )?;
        Ok(())
    }

    pub fn query_gap_summary(&self) -> Result<(i64, i64, i64)> {
        let unique_gap_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM query_gap_log",
            [],
            |row| row.get(0),
        )?;
        let total_seen_count: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(seen_count), 0) FROM query_gap_log",
            [],
            |row| row.get(0),
        )?;
        let recurrent_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM query_gap_log WHERE seen_count >= 2",
            [],
            |row| row.get(0),
        )?;

        Ok((unique_gap_count, total_seen_count, recurrent_count))
    }

    pub fn top_query_gaps(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, i64, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool_name, query_text, seen_count, last_seen_at, last_reason
             FROM query_gap_log
             ORDER BY seen_count DESC, last_seen_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;

        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Most frequently called tools — useful for tuning what to pre-inject.
    pub fn hot_tools(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool, COUNT(*) as n FROM mcp_calls GROUP BY tool ORDER BY n DESC LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    // ── Pending observations ──────────────────────────────────────────────────

    pub fn add_observation(&self, o: &PendingObservation) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_observations (path, summary, diff_hint, observed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![o.path, o.summary, o.diff_hint, o.observed_at.to_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_observations(&self) -> Result<Vec<PendingObservation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, summary, diff_hint, observed_at
             FROM pending_observations ORDER BY observed_at ASC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingObservation {
                id: Some(row.get(0)?),
                path: row.get(1)?,
                summary: row.get(2)?,
                diff_hint: row.get(3)?,
                observed_at: parse_rfc3339_or_flag(
                    &row.get::<_, String>(4)?, "pending_observations", None, "observed_at"),
            })
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn dismiss_observation(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_observations WHERE id = ?1", params![id]
        )?;
        Ok(())
    }

    // ── ADRs ──────────────────────────────────────────────────────────────────

    pub fn next_adr_number(&self) -> Result<i64> {
        let max: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(adr_number), 0) FROM adrs",
            [], |r| r.get(0))?
        ;
        Ok(max + 1)
    }

    pub fn insert_adr(&self, adr: &Adr) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO adrs (adr_number, title, status, context, decision, reasoning,
                              alternatives, consequences, concept_tags, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                adr.adr_number, adr.title, adr.status, adr.context, adr.decision,
                adr.reasoning, adr.alternatives, adr.consequences,
                serde_json::to_string(&adr.concept_tags)?,
                now,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        // Index concept tags.
        for tag in &adr.concept_tags {
            let _ = self.conn.execute(
                "INSERT OR IGNORE INTO adr_tag_index (tag, adr_id) VALUES (?1, ?2)",
                params![tag.to_lowercase(), id],
            );
        }
        Ok(id)
    }

    pub fn all_adrs(&self) -> Result<Vec<Adr>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, adr_number, title, status, context, decision, reasoning,
                    alternatives, consequences, concept_tags, superseded_by, created_at, updated_at
             FROM adrs ORDER BY adr_number"
        )?;
        let rows = stmt.query_map([], row_to_adr)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_adr(&self, number: i64) -> Result<Option<Adr>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, adr_number, title, status, context, decision, reasoning,
                    alternatives, consequences, concept_tags, superseded_by, created_at, updated_at
             FROM adrs WHERE adr_number = ?1"
        )?;
        let mut rows = stmt.query_map(params![number], row_to_adr)?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_adrs_by_tags(&self, tags: &[String]) -> Result<Vec<Adr>> {
        if tags.is_empty() { return Ok(vec![]); }
        let mut result: Vec<Adr> = Vec::new();
        for tag in tags {
            let mut stmt = self.conn.prepare(
                "SELECT a.id, a.adr_number, a.title, a.status, a.context, a.decision,
                        a.reasoning, a.alternatives, a.consequences, a.concept_tags,
                        a.superseded_by, a.created_at, a.updated_at
                 FROM adrs a
                 JOIN adr_tag_index t ON t.adr_id = a.id
                 WHERE t.tag = ?1 AND a.status = 'accepted'
                 ORDER BY a.adr_number"
            )?;
            let rows = stmt.query_map(params![tag.to_lowercase()], row_to_adr)?;
            for r in rows {
                let adr = r?;
                if !result.iter().any(|existing| existing.adr_number == adr.adr_number) {
                    result.push(adr);
                }
            }
        }
        result.sort_by_key(|a| a.adr_number);
        Ok(result)
    }

    pub fn update_adr_status(&self, id: i64, status: &str, superseded_by: Option<i64>) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE adrs SET status = ?1, superseded_by = ?2, updated_at = ?3 WHERE id = ?4",
            params![status, superseded_by, now, id],
        )?;
        Ok(())
    }

    // ── Pattern unit refs (drift detection) ───────────────────────────────────

    pub fn insert_pattern_unit_ref(&self, pattern_id: i64, unit_id: &str) -> Result<()> {
        let _ = self.conn.execute(
            "INSERT OR IGNORE INTO pattern_unit_refs (pattern_id, unit_id) VALUES (?1, ?2)",
            params![pattern_id, unit_id],
        );
        Ok(())
    }

    pub fn get_unit_ids_for_pattern(&self, pattern_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT unit_id FROM pattern_unit_refs WHERE pattern_id = ?1"
        )?;
        let rows = stmt.query_map(params![pattern_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns pattern ids whose linked units have had signature changes.
    pub fn patterns_with_stale_units(&self) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, cu.name
             FROM patterns p
             JOIN pattern_unit_refs r ON r.pattern_id = p.id
             JOIN code_units cu ON cu.id = r.unit_id
             WHERE cu.signature_changed_at IS NOT NULL
             ORDER BY p.id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── Self-corrections ──────────────────────────────────────────────────────

    pub fn insert_self_correction(
        &self,
        attempted: &str,
        failure_reason: &str,
        correction: &str,
        tags: &[String],
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(tags)?;
        self.conn.execute(
            "INSERT INTO self_corrections
             (attempted, failure_reason, correction, tags, occurrence_count, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(attempted, failure_reason) DO UPDATE SET
                occurrence_count = occurrence_count + 1,
                last_seen_at = excluded.last_seen_at,
                correction = excluded.correction",
            params![attempted, failure_reason, correction, tags_json, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_self_corrections(&self) -> Result<Vec<SelfCorrection>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, attempted, failure_reason, correction, tags,
                    occurrence_count, first_seen_at, last_seen_at
             FROM self_corrections ORDER BY occurrence_count DESC, last_seen_at DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            let tags_json: String = row.get(4)?;
            let first: String = row.get(6)?;
            let last: String = row.get(7)?;
            Ok(SelfCorrection {
                id: Some(row.get(0)?),
                attempted: row.get(1)?,
                failure_reason: row.get(2)?,
                correction: row.get(3)?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                occurrence_count: row.get(5)?,
                first_seen_at: parse_rfc3339_or_flag(
                    &first, "self_corrections", None, "first_seen_at"),
                last_seen_at: parse_rfc3339_or_flag(
                    &last, "self_corrections", None, "last_seen_at"),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Promote a high-frequency self-correction (>= threshold occurrences) to an anti-pattern.
    pub fn promote_correction_to_anti_pattern(&self, id: i64) -> Result<Option<i64>> {
        let sc: Option<SelfCorrection> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, attempted, failure_reason, correction, tags,
                        occurrence_count, first_seen_at, last_seen_at
                 FROM self_corrections WHERE id = ?1"
            )?;
            let mut rows = stmt.query_map(params![id], |row| {
                let tags_json: String = row.get(4)?;
                let first: String = row.get(6)?;
                let last: String = row.get(7)?;
                Ok(SelfCorrection {
                    id: Some(row.get(0)?),
                    attempted: row.get(1)?,
                    failure_reason: row.get(2)?,
                    correction: row.get(3)?,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                    occurrence_count: row.get(5)?,
                    first_seen_at: parse_rfc3339_or_flag(
                        &first, "self_corrections", None, "first_seen_at"),
                    last_seen_at: parse_rfc3339_or_flag(
                        &last, "self_corrections", None, "last_seen_at"),
                })
            })?;
            rows.next().transpose()?
        };
        if let Some(sc) = sc {
            let ap = AntiPattern {
                id: None,
                description: format!("[auto] {} — {}", sc.failure_reason, sc.attempted),
                wrong: sc.attempted.clone(),
                correct: sc.correction.clone(),
                tags: sc.tags.clone(),
                added_at: chrono::Utc::now(),
            };
            let ap_id = self.insert_anti_pattern(&ap)?;
            self.conn.execute(
                "DELETE FROM self_corrections WHERE id = ?1", params![id]
            )?;
            return Ok(Some(ap_id));
        }
        Ok(None)
    }

    // ── Pattern merge log (consolidation) ─────────────────────────────────────

    pub fn insert_merge_log(
        &self,
        kept_id: i64,
        merged_id: i64,
        score: f32,
        reason: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pattern_merge_log
             (kept_id, merged_id, similarity_score, merge_reason, merged_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kept_id, merged_id, score, reason, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ── FTS search ────────────────────────────────────────────────────────────

    /// BM25 FTS5 search across patterns. Returns matched patterns ranked by relevance.
    pub fn fts_search_patterns(&self, query: &str, limit: usize) -> Result<Vec<Pattern>> {
        let safe_q = sanitize_fts_query(query);
        if safe_q.is_empty() { return Ok(vec![]); }
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.intent, p.body, p.uses, p.tags, p.approved_at,
                    p.use_count, p.reverted_count, p.survival_rate
             FROM pattern_fts f
             JOIN patterns p ON p.id = f.rowid
             WHERE pattern_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![safe_q, limit as i64], row_to_pattern)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// BM25 FTS5 search across anti-patterns.
    pub fn fts_search_anti_patterns(&self, query: &str, limit: usize) -> Result<Vec<AntiPattern>> {
        let safe_q = sanitize_fts_query(query);
        if safe_q.is_empty() { return Ok(vec![]); }
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.description, a.wrong, a.correct, a.tags, a.added_at
             FROM anti_pattern_fts f
             JOIN anti_patterns a ON a.id = f.rowid
             WHERE anti_pattern_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![safe_q, limit as i64], row_to_anti_pattern)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// BM25 FTS5 search across annotations.
    pub fn fts_search_annotations(&self, query: &str, limit: usize) -> Result<Vec<Annotation>> {
        let safe_q = sanitize_fts_query(query);
        if safe_q.is_empty() { return Ok(vec![]); }
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.topic, a.body, a.tags, a.added_at
             FROM annotation_fts f
             JOIN annotations a ON a.id = f.rowid
             WHERE annotation_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![safe_q, limit as i64], row_to_annotation)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

}

/// Sanitize user input for FTS5 MATCH queries. Strips special chars, joins with spaces (AND).
fn sanitize_fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| t.chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>())
        .filter(|t| t.len() >= 2)
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Row mappers ───────────────────────────────────────────────────────────────

fn row_to_adr(row: &rusqlite::Row) -> rusqlite::Result<Adr> {
    let tags_json: String = row.get(9)?;
    let created: String = row.get(11)?;
    let updated: String = row.get(12)?;
    Ok(Adr {
        id: Some(row.get(0)?),
        adr_number: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        context: row.get(4)?,
        decision: row.get(5)?,
        reasoning: row.get(6)?,
        alternatives: row.get(7)?,
        consequences: row.get(8)?,
        concept_tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        superseded_by: row.get(10)?,
        created_at: parse_rfc3339_or_flag(&created, "adrs", None, "created_at"),
        updated_at: parse_rfc3339_or_flag(&updated, "adrs", None, "updated_at"),
    })
}

fn row_to_unit(row: &rusqlite::Row) -> rusqlite::Result<CodeUnit> {
    let tv_json: String = row.get(6)?;
    let term_vector: Vec<(String, f32)> = serde_json::from_str(&tv_json)
        .unwrap_or_default();
    let indexed_at = parse_rfc3339_or_flag(
        &row.get::<_, String>(7)?, "code_units", None, "indexed_at");

    Ok(CodeUnit {
        id:          row.get(0)?,
        kind:        row.get(1)?,
        name:        row.get(2)?,
        module_path: row.get(3)?,
        summary:     row.get(4)?,
        compressed:  row.get(5)?,
        term_vector,
        indexed_at,
    })
}

/// Parse a stored timestamp, degrading instead of panicking.
///
/// Every timestamp read goes through here. The bare
/// `parse_from_rfc3339(..).unwrap()` this replaced turned one malformed row into
/// a process abort: a knowledge-capture path that wrote epoch integers instead of
/// RFC 3339 strings crashed `get_context` outright, taking the whole MCP server
/// with it and returning nothing for the rest of the session.
///
/// A bad row should cost one timestamp, not the server.
fn parse_rfc3339_or_flag(
    raw: &str,
    table: &str,
    row_id: Option<i64>,
    field: &str,
) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|err| {
            let id_text = row_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let preview: String = raw.chars().take(96).collect();
            eprintln!(
                "[cortex][repair-needed] {}.{} id={} has invalid RFC3339 timestamp ({}). raw='{}'. Using fallback 1970-01-01T00:00:00Z; timestamp accuracy is compromised until this row is repaired.",
                table,
                field,
                id_text,
                err,
                preview
            );
            chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                .expect("UNIX epoch must be representable")
        })
}

fn row_to_pattern(row: &rusqlite::Row) -> rusqlite::Result<Pattern> {
    let id: i64 = row.get(0)?;
    let uses: Vec<String> = serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default();
    let tags: Vec<String> = serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default();
    let approved_at_raw: String = row.get(6)?;
    let approved_at = parse_rfc3339_or_flag(&approved_at_raw, "patterns", Some(id), "approved_at");
    Ok(Pattern {
        id: Some(id), name: row.get(1)?, intent: row.get(2)?,
        body: row.get(3)?, uses, tags, approved_at,
        use_count: row.get(7)?,
        reverted_count: row.get(8)?,
        survival_rate: row.get(9)?,
    })
}

fn row_to_anti_pattern(row: &rusqlite::Row) -> rusqlite::Result<AntiPattern> {
    let id: i64 = row.get(0)?;
    let tags: Vec<String> = serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default();
    let added_at_raw: String = row.get(5)?;
    let added_at = parse_rfc3339_or_flag(&added_at_raw, "anti_patterns", Some(id), "added_at");
    Ok(AntiPattern {
        id: Some(id), description: row.get(1)?,
        wrong: row.get(2)?, correct: row.get(3)?, tags, added_at,
    })
}

fn row_to_annotation(row: &rusqlite::Row) -> rusqlite::Result<Annotation> {
    let id: i64 = row.get(0)?;
    let tags: Vec<String> = serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default();
    let added_at_raw: String = row.get(4)?;
    let added_at = parse_rfc3339_or_flag(&added_at_raw, "annotations", Some(id), "added_at");
    Ok(Annotation {
        id: Some(id), topic: row.get(1)?,
        body: row.get(2)?, tags, added_at,
    })
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use crate::model::CodeUnit;

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join("cortex-prune-test");
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join(format!("{name}.db"));
        let _ = std::fs::remove_file(&db);
        Store::open(&db).unwrap()
    }

    fn unit(id: &str) -> CodeUnit {
        CodeUnit {
            id: id.to_string(),
            kind: "struct".into(),
            name: id.rsplit("::").next().unwrap().to_string(),
            module_path: id.rsplit_once("::").map(|(m, _)| m).unwrap_or("").to_string(),
            summary: String::new(),
            term_vector: vec![],
            compressed: format!("[struct: {id}]\n"),
            indexed_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn prune_removes_only_units_from_unconfigured_sources() {
        let s = store("orphans");
        s.upsert_unit_from(&unit("canvas::core::Canvas"), Some("quartz/src")).unwrap();
        s.upsert_unit_from(&unit("cortex::memory::Store"), Some("cortex/src")).unwrap();

        let deleted = s.prune_orphan_units(&["quartz/src".to_string()]).unwrap();

        assert_eq!(deleted, 1, "only the unconfigured source should be pruned");
        let names: Vec<String> = s.conn
            .prepare("SELECT id FROM code_units ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(names, vec!["canvas::core::Canvas".to_string()]);
    }

    /// Rows predating provenance stamping have a NULL source_root. After a full
    /// reindex restamps every configured source, whatever is still NULL is residue.
    #[test]
    fn unstamped_units_are_treated_as_orphans() {
        let s = store("unstamped");
        s.upsert_unit(&unit("air_src::legacy::Thing")).unwrap();
        s.upsert_unit_from(&unit("canvas::core::Canvas"), Some("quartz/src")).unwrap();

        assert_eq!(s.prune_orphan_units(&["quartz/src".to_string()]).unwrap(), 1);
        let left: i64 = s.conn
            .query_row("SELECT COUNT(*) FROM code_units", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn units_by_source_groups_stamped_and_unstamped() {
        let s = store("bysource");
        s.upsert_unit_from(&unit("a::A"), Some("quartz/src")).unwrap();
        s.upsert_unit_from(&unit("b::B"), Some("quartz/src")).unwrap();
        s.upsert_unit(&unit("c::C")).unwrap();

        let groups = s.units_by_source().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], (Some("quartz/src".to_string()), 2));
        assert_eq!(groups[1], (None, 1));
    }
}
