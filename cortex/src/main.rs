mod adr;
mod audit;
mod cache;
mod closeout;
mod compressor;
mod consolidator;
mod corrections;
mod consolidator2;
mod crystallizer;
mod git;
mod graph;
mod graph_diff;
mod markers;
mod memory;
mod mcp;
mod meta;
mod miner;
mod model;
mod output_filter;
mod test_signal;
mod planner;
mod recall_match;
mod prefs;
mod protocol;
mod reasoner;
mod scoreboard;
mod search;
mod session_store;
mod skills;
mod verify;
mod watcher;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use memory::Store;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "cortex", version, about = "Persistent semantic memory layer for Copilot")]
struct Cli {
    /// Path to the cortex database. Defaults to .cortex/memory.db in the project root.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Output format for script-safe automation.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// First-time project bootstrap: create launcher and MCP config files.
    Bootstrap(BootstrapArgs),

    /// Index a source directory (and optional quartz-ctx api-graph.json).
    Index(IndexArgs),

    /// Run the MCP skill server — Copilot calls cortex as a live tool.
    Serve(ServeArgs),

    /// Watch for file changes and queue them for review. Never auto-approves.
    Watch(WatchArgs),

    /// List pending observations queued by `watch` or Copilot.
    Review,

    /// Promote a pending observation to an approved pattern.
    Crystallize(CrystallizeArgs),

    /// Discard a pending observation.
    Dismiss(DismissArgs),

    /// Get a pre-compiled context packet for a task or set of files.
    Context(ContextArgs),

    /// Graph relation management and querying.
    #[command(subcommand)]
    Graph(GraphCmd),

    /// Run graph drift analysis between current graph and latest snapshot.
    GraphDiff,

    /// Preference file management.
    #[command(subcommand)]
    Prefs(PrefsCmd),

    /// Pattern management.
    #[command(subcommand)]
    Pattern(PatternCmd),

    /// Anti-pattern management.
    #[command(subcommand)]
    AntiPattern(AntiPatternCmd),

    /// Annotation management.
    #[command(subcommand)]
    Annotate(AnnotateCmd),

    /// Print the indexed sources from .cortex/index-sources.json as TSV.
    ///
    /// Exists so the shell launchers need no JSON parser. cortex.sh previously
    /// shelled out to python3 for this, which quietly made python a dependency
    /// of a suite that advertises none -- and that only fails on a machine that
    /// lacks it, which is never the author's.
    Manifest {
        /// Repo root holding .cortex/index-sources.json.
        #[arg(long, default_value = ".")]
        repo: std::path::PathBuf,
    },

    /// Prune call log and run VACUUM to reclaim space.
    Prune {
        /// Number of MCP call log entries to keep.
        #[arg(long, default_value = "500")]
        keep_calls: usize,
    },

    /// Remove indexed units that no configured source root claims.
    ///
    /// Indexing is INSERT OR REPLACE with no delete, so units from sources that
    /// were renamed or dropped out of index-sources.json stay in the index and
    /// keep being served. Run this after a full reindex, passing every root that
    /// is still configured.
    ///
    /// Reports without deleting unless --apply is given.
    ///
    /// Example:
    ///   cortex prune-index --keep quartz/src --keep path_forge/src --apply
    PruneIndex {
        /// A source root that is still configured. Repeatable.
        #[arg(long = "keep")]
        keep: Vec<String>,

        /// Actually delete. Without this the command only reports.
        #[arg(long)]
        apply: bool,
    },

    /// Show memory store statistics.
    Status {
        #[arg(long)]
        full: bool,
    },

    /// Run meta-analysis: analyze proposal effectiveness and stage threshold suggestions.
    #[command(subcommand)]
    Meta(MetaCmd),

    /// Run production-style workflow health checks.
    #[command(subcommand)]
    Doctor(DoctorCmd),

    /// Show which mechanisms have actually fired, and which are silently idle.
    Fired,

    /// Search patterns, anti-patterns, annotations, and indexed units for a topic.
    Recall {
        /// Topic keyword to search for across all memory.
        topic: String,
    },

    /// Scan recent git diff for pattern relevance — shows which patterns apply to changed files.
    GitReview {
        /// Compare against this base ref (default: HEAD~1).
        #[arg(long, default_value = "HEAD~1")]
        base: String,

        /// Path to the repo root (default: current dir).
        #[arg(long)]
        repo: Option<PathBuf>,
    },

    /// Architecture Decision Records (ADRs).
    #[command(subcommand)]
    Adr(AdrCmd),

    /// Find and optionally merge duplicate/overlapping patterns using cosine similarity.
    Consolidate {
        /// Similarity threshold 0.0–1.0 (default 0.72). Pairs above this score are flagged.
        #[arg(long, default_value_t = 0.72)]
        threshold: f32,

        /// Just report candidates; do not merge anything.
        #[arg(long)]
        report: bool,
    },

    /// Log a self-correction: what was attempted, why it failed, and what the fix was.
    Correction {
        /// The thing that was attempted (wrong approach or snippet).
        #[arg(long)]
        attempted: String,

        /// Reason it failed.
        #[arg(long)]
        reason: String,

        /// The correct approach or fix.
        #[arg(long)]
        fix: String,

        /// Comma-separated tags.
        #[arg(long, default_value = "")]
        tags: String,
    },

    /// Log an execution outcome for evidence tracking.
    Outcome(OutcomeArgs),

    /// Apply weighted pattern confidence updates from retrieval + outcome evidence.
    OutcomeApply(OutcomeApplyArgs),

    /// Run lightweight benchmark harnesses for syntax lookup and dependency persistence.
    Benchmark(BenchmarkArgs),

    // ── Phase 1: Session mining + consolidation pipeline ──────────────────────

    /// Cluster session snapshots from .cortex/mined-tasks/ by TF-IDF tool-sequence similarity.
    ClusterSessions {
        /// Cosine similarity threshold for clustering (default: 0.55).
        #[arg(long, default_value_t = 0.55)]
        threshold: f32,
        /// Output file for cluster JSON (default: .cortex/clusters.json).
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Detect skill candidates from session clusters and stage drafts in .cortex/proposals/.
    DetectSkills {
        /// Minimum sessions in a cluster before a skill candidate is drafted.
        #[arg(long, default_value_t = 3)]
        min_occurrences: u32,
    },

    /// Surface hot query gaps (seen >= N times) as prefs.toml note proposals.
    ProposeGaps {
        /// Minimum seen_count before a gap becomes a proposal (default: 3).
        #[arg(long, default_value_t = 3)]
        min_count: i64,
    },

    /// Flag patterns with survival_rate < 0.4 and use_count >= 3 for review.
    ProposeSurvival,

    /// Run the full 6-stage consolidation pipeline.
    /// Equivalent to: cluster-sessions → detect-skills → propose-gaps → propose-survival.
    ConsolidatePipeline,

    /// Run consolidation only if last run was more than N hours ago (for runOn:folderOpen).
    ConsolidateIfStale {
        /// Staleness threshold in hours (default: 8).
        #[arg(long, default_value_t = 8)]
        staleness_hours: u32,
    },

    /// Interactively review pending cross-session proposals (approve / reject / defer).
    ReviewProposals {
        /// Filter by proposal type (e.g. skill, pref_note, dying_pattern).
        #[arg(long)]
        kind: Option<String>,
    },

    /// List skill candidates detected from session patterns.
    SkillStatus,

    /// Approve a skill candidate: publish its draft to the skills directory.
    SkillApprove {
        /// Skill name (as shown by skill-status).
        name: String,
        /// Publish even if the draft still contains [Edit: ...] placeholders.
        #[arg(long)]
        force: bool,
    },

    /// Reject a skill candidate draft.
    SkillReject {
        /// Skill name.
        name: String,
    },

    /// Approve a pending proposal by id (as shown by review-proposals).
    ///
    /// `review-proposals` is interactive, which is no use from a script or an
    /// agent, and it printed instructions for these two subcommands before they
    /// existed. Now they do.
    ProposalApprove {
        /// Proposal id.
        id: i64,
    },

    /// Reject a pending proposal by id.
    ProposalReject {
        /// Proposal id.
        id: i64,
        /// Optional one-line reason, recorded so a later reader knows why.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Find sessions without a closeout record.
    SessionOrphans,

    /// Print a one-line system health report.
    HealthReport,

    /// Self-learning KPI scoreboard: pass rate, gap rate, marker capture,
    /// pattern reuse, telemetry coverage — each with a trend vs the previous window.
    Scoreboard {
        /// Rolling window length in days (compared against the previous window of the same length).
        #[arg(long, default_value_t = 14)]
        window_days: u32,
    },

    /// Install the lossless compact_output hook into Claude Code settings.
    HooksInit {
        /// Repo root to write settings to (default: current dir).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Write to the shared, committed .claude/settings.json instead of the
        /// personal, git-ignored .claude/settings.local.json (default).
        #[arg(long)]
        shared: bool,
        /// Refresh the hook even if an identical one is already present.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

// ── ADR subcommand ────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
enum AdrCmd {
    /// Record a new Architecture Decision.
    New {
        #[arg(long)] title: String,
        #[arg(long)] context: String,
        #[arg(long)] decision: String,
        #[arg(long, default_value = "")] reasoning: String,
        #[arg(long, default_value = "")] alternatives: String,
        #[arg(long, default_value = "")] consequences: String,
        /// Comma-separated concept tags for context matching.
        #[arg(long, default_value = "")] tags: String,
    },
    /// List all ADRs.
    List,
    /// Show a single ADR by number.
    Show {
        #[arg()] number: i64,
    },
    /// Deprecate or supersede an ADR.
    Deprecate {
        #[arg()] number: i64,
        #[arg(long)] superseded_by: Option<i64>,
    },
}

// ── Subcommand args ───────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
struct IndexArgs {
    /// Source directory to parse and compress.
    #[arg(short, long, default_value = "src")]
    source: PathBuf,

    /// Optional path to a quartz-ctx api-graph.json to ingest alongside source.
    #[arg(long)]
    api_graph: Option<PathBuf>,

    /// Engine/project name label.
    #[arg(short, long, default_value = "Quartz")]
    name: String,

    /// Optional scope prefix prepended to all unit IDs (e.g. "synful").
    /// Use when indexing multiple source roots into the same DB to avoid ID collisions.
    #[arg(long)]
    scope: Option<String>,
}

#[derive(Parser, Debug)]
struct BootstrapArgs {
    /// Workspace root where .cortex/ and .vscode/ should be created.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Primary source path used by the MCP serve entry.
    #[arg(long, default_value = "src")]
    source: String,

    /// Project display name used by MCP serve.
    #[arg(long)]
    name: Option<String>,

    /// Overwrite existing files when present.
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Parser, Debug)]
struct ServeArgs {
    #[arg(short, long, default_value = "src")]
    source: PathBuf,

    #[arg(long, default_value = ".")]
    repo: PathBuf,

    #[arg(long)]
    api_graph: Option<PathBuf>,

    #[arg(long)]
    prefs: Option<PathBuf>,

    #[arg(short, long, default_value = "Quartz")]
    name: String,
}

#[derive(Parser, Debug)]
struct WatchArgs {
    #[arg(short, long, default_value = "src")]
    source: PathBuf,
}

#[derive(Parser, Debug)]
struct CrystallizeArgs {
    /// ID of the pending observation to promote.
    pub id: i64,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub intent: String,
    /// Override the observation body with custom code. Defaults to the observation's diff_hint.
    #[arg(long)]
    pub body: Option<String>,
    /// API item names this pattern uses.
    #[arg(long, value_delimiter = ',')]
    pub uses: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,
}

#[derive(Parser, Debug)]
struct DismissArgs {
    pub id: i64,
}

#[derive(Parser, Debug)]
struct OutcomeArgs {
    /// Logical session identifier (for example: protocol_run_2026_06_07).
    #[arg(long, default_value = "cli_manual")]
    session_id: String,

    /// Outcome classification (for example: build_pass, build_fail, test_fail, review_findings).
    #[arg(long)]
    outcome_type: String,

    /// Optional error payload or failure message.
    #[arg(long)]
    error_text: Option<String>,

    /// Optional comma-separated or free-form impacted symbols summary.
    #[arg(long)]
    diff_symbols: Option<String>,

    /// Automatically apply weighted evidence after logging the outcome.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    auto_apply: bool,
}

#[derive(Parser, Debug)]
struct OutcomeApplyArgs {
    /// Session identifier to evaluate from retrieval and outcome logs.
    #[arg(long)]
    session_id: String,

    /// Preview computed weights without mutating pattern counters.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum BenchmarkTarget {
    Syntax,
    Dependency,
}

#[derive(Parser, Debug)]
struct BenchmarkArgs {
    /// Benchmark category.
    #[arg(long, value_enum)]
    target: BenchmarkTarget,

    /// Number of samples to evaluate.
    #[arg(long, default_value_t = 64)]
    samples: usize,

    /// Graph traversal depth for dependency benchmark.
    #[arg(long, default_value_t = 2)]
    depth: u8,

    /// Optional JSON corpus path for dependency precision checks.
    #[arg(long)]
    corpus: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct ContextArgs {
    /// Task description or space-separated file paths.
    pub hint: String,
    #[arg(long, default_value = "2000")]
    pub token_budget: usize,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    #[arg(long)]
    pub prefs: Option<PathBuf>,

    /// Optional include filter for delta paths in the context packet.
    #[arg(long)]
    pub delta_include: Option<String>,

    /// Optional exclude filter for delta paths in the context packet.
    #[arg(long)]
    pub delta_exclude: Option<String>,

    /// Max changed files to include in context deltas.
    #[arg(long, default_value = "8")]
    pub delta_max_files: usize,
}

#[derive(Subcommand, Debug)]
enum DoctorCmd {
    /// Full workflow smoke test for automation and CI checks.
    Workflow(DoctorWorkflowArgs),
}

#[derive(Parser, Debug)]
struct DoctorWorkflowArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    #[arg(long, default_value = "src")]
    source: PathBuf,

    #[arg(long, default_value = "Quartz")]
    name: String,

    /// Mutate and rollback a sentinel pattern as part of validation.
    #[arg(long, default_value_t = false)]
    mutate_pattern: bool,

    /// Optional include filter for delta checks.
    #[arg(long)]
    delta_include: Option<String>,

    /// Optional exclude filter for delta checks.
    #[arg(long)]
    delta_exclude: Option<String>,

    /// Max changed files to inspect in delta checks.
    #[arg(long, default_value = "25")]
    delta_max_files: usize,
}

#[derive(Subcommand, Debug)]
enum MetaCmd {
    /// Show full meta-analysis report (rejection rates, fidelity trends, gaps, thresholds).
    Report,
    /// Run all analyzers and stage meta-proposals for review.
    Propose,
    /// Apply an approved meta-proposal to its target file.
    Apply {
        /// Proposal ID from `cortex meta report` or `cortex review-proposals`.
        id: i64,
    },
    /// Show what `apply` would change without writing any files.
    DryRun {
        id: i64,
    },
}

#[derive(Subcommand, Debug)]
enum GraphCmd {
    Sync,
    AddPair {
        from: String,
        to: String,
        /// Relation type: pairs (default), owns, uses, calls, implements, conflicts, derived_from
        #[arg(long, default_value = "pairs")]
        relation: String,
    },
    AddConflict {
        from: String,
        to: String,
    },
    Query {
        name: String,
        #[arg(long, default_value = "1")]
        depth: u8,
    },
}

#[derive(Subcommand, Debug)]
enum PrefsCmd {
    Show {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Edit {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum PatternCmd {
    /// List all approved patterns.
    List,
    /// Add a pattern directly.
    Add {
        #[arg(long)] name: String,
        #[arg(long)] intent: String,
        #[arg(long)] body: String,
        #[arg(long, value_delimiter = ',')] uses: Vec<String>,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
    },
    /// Remove a pattern by id.
    Remove { id: i64 },
    /// Mark a pattern as reverted once and update survival rate.
    Revert { id: i64 },
    /// Retire a pattern in favour of a newer one. It stays in the DB as history
    /// but is never served again.
    Supersede {
        /// The pattern being retired.
        id: i64,
        /// The pattern that replaces it.
        #[arg(long)] by: i64,
    },
    /// List patterns retired by `supersede`.
    Retired,
    /// Show pattern survival health.
    Health,
}

#[derive(Subcommand, Debug)]
enum AntiPatternCmd {
    List,
    Add {
        #[arg(long)] description: String,
        #[arg(long)] wrong: String,
        #[arg(long)] correct: String,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
    },
    Remove { id: i64 },
    /// Retire an anti-pattern in favour of a newer one. Use this when a later
    /// entry CORRECTS an earlier one — otherwise both are served, and a reader
    /// gets told to do the thing the correction exists to prevent.
    Supersede {
        /// The anti-pattern being retired.
        id: i64,
        /// The anti-pattern that replaces it.
        #[arg(long)] by: i64,
    },
    /// List anti-patterns retired by `supersede`.
    Retired,
}

#[derive(Subcommand, Debug)]
enum AnnotateCmd {
    List,
    Add {
        #[arg(long)] topic: String,
        #[arg(long)] body: String,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
    },
    Remove { id: i64 },
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db_path = match &cli.db {
        Some(path) => path.clone(),
        None => resolve_default_db_path(),
    };
    let format = cli.format;

    match cli.command {
        Command::Bootstrap(args)   => run_bootstrap(args, &db_path),
        Command::Index(args)       => run_index(args, &db_path),
        Command::Serve(args)       => run_serve(args, &db_path),
        Command::Watch(args)       => run_watch(args, &db_path),
        Command::Review            => run_review(&db_path),
        Command::Crystallize(args) => run_crystallize(args, &db_path),
        Command::Dismiss(args)     => run_dismiss(args, &db_path),
        Command::Context(args)     => run_context(args, &db_path),
        Command::Graph(cmd)        => run_graph(cmd, &db_path),
        Command::GraphDiff         => run_graph_diff_cmd(&db_path, format),
        Command::Prefs(cmd)        => run_prefs(cmd),
        Command::Pattern(cmd)      => run_pattern(cmd, &db_path, format),
        Command::AntiPattern(cmd)  => run_anti_pattern(cmd, &db_path, format),
        Command::Annotate(cmd)     => run_annotate(cmd, &db_path, format),
        Command::Manifest { repo } => run_manifest(&repo),
        Command::Prune { keep_calls } => run_prune(keep_calls, &db_path),
        Command::PruneIndex { keep, apply } => run_prune_index(keep, apply, &db_path),
        Command::Status { full }   => run_status(&db_path, full, format),
        Command::Meta(cmd)        => run_meta(cmd, &db_path, format),
        Command::Doctor(cmd)       => run_doctor(cmd, &db_path, format),
        Command::Fired             => audit::run_cli(&Store::open(&db_path)?),
        Command::Recall { topic }  => run_recall(&topic, &db_path, format),
        Command::GitReview { base, repo } => run_git_review(&base, repo.as_deref(), &db_path),
        Command::Adr(cmd)          => run_adr(cmd, &db_path),
        Command::Consolidate { threshold, report } => run_consolidate(threshold, report, &db_path),
        Command::Correction { attempted, reason, fix, tags } => {
            let tag_vec: Vec<String> = tags.split(',').map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()).collect();
            run_correction(&attempted, &reason, &fix, &tag_vec, &db_path)
        }
        Command::Outcome(args) => run_outcome(args, &db_path, format),
        Command::OutcomeApply(args) => run_outcome_apply(args, &db_path, format),
        Command::Benchmark(args) => run_benchmark(args, &db_path, format),

        // ── Phase 1 commands ──────────────────────────────────────────────────
        Command::ClusterSessions { threshold, output } => {
            run_cluster_sessions(threshold, output.as_deref(), &db_path)
        }
        Command::DetectSkills { min_occurrences } => {
            run_detect_skills(min_occurrences, &db_path)
        }
        Command::ProposeGaps { min_count } => run_propose_gaps(min_count, &db_path),
        Command::ProposeSurvival                => run_propose_survival(&db_path),
        Command::ConsolidatePipeline            => run_consolidate_pipeline(&db_path),
        Command::ConsolidateIfStale { staleness_hours } => {
            run_consolidate_if_stale(staleness_hours, &db_path)
        }
        Command::ReviewProposals { kind }       => run_review_proposals(kind.as_deref(), &db_path),
        Command::SkillStatus                    => run_skill_status(&db_path),
        Command::SkillApprove { name, force }   => run_skill_approve(&name, &db_path, force),
        Command::SkillReject { name }           => run_skill_reject(&name, &db_path),
        Command::ProposalApprove { id }         => run_proposal_decision(id, "approved", None, &db_path),
        Command::ProposalReject { id, reason }  => {
            run_proposal_decision(id, "rejected", reason.as_deref(), &db_path)
        }
        Command::SessionOrphans                 => run_session_orphans(&db_path),
        Command::HealthReport                   => run_health_report(&db_path),
        Command::Scoreboard { window_days }     => run_scoreboard(&db_path, window_days, format),
        Command::HooksInit { root, shared, force } => run_hooks_init(root, shared, force),
    }
}

fn resolve_default_db_path() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(found) = discover_cortex_dir(&cwd) {
        return found.join("memory.db");
    }
    PathBuf::from(".cortex/memory.db")
}

fn discover_cortex_dir(start: &Path) -> Option<PathBuf> {
    let mut fallback: Option<PathBuf> = None;

    // First pass: prefer `.cortex` in current or parent directories.
    for ancestor in start.ancestors() {
        let candidate = ancestor.join(".cortex");
        if candidate.is_dir() {
            if is_workspace_cortex_dir(&candidate) {
                return Some(candidate);
            }
            if fallback.is_none() {
                fallback = Some(candidate);
            }
        }
    }

    // Second pass: also check `cortex/.cortex` under current and parent directories.
    for ancestor in start.ancestors() {
        let candidate = ancestor.join("cortex").join(".cortex");
        if candidate.is_dir() {
            if is_workspace_cortex_dir(&candidate) {
                return Some(candidate);
            }
            if fallback.is_none() {
                fallback = Some(candidate);
            }
        }
    }

    fallback
}

fn is_workspace_cortex_dir(dir: &Path) -> bool {
    dir.join("cortex.ps1").is_file()
    || dir.join("index-sources.json").is_file()
}

fn run_scoreboard(db_path: &Path, window_days: u32, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let sb = scoreboard::compute(&store, window_days)?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&sb)?),
        OutputFormat::Text => print!("{}", scoreboard::format_text(&sb)),
    }
    Ok(())
}

/// Outcome of an `ensure_compact_hook` call, for honest reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookOutcome {
    /// The hook was written (file created or the block added/refreshed).
    Written,
    /// The hook was already present and identical — nothing changed.
    AlreadyPresent,
}

/// Ensure the lossless `compact_output` hook exists in the given Claude Code
/// settings file (`settings.json` shared, or `settings.local.json` personal).
///
/// Passes BOTH `${tool_response.stdout}` and `${tool_response.stderr}` —
/// cargo/rustc write diagnostics to stderr, so an stdout-only hook would
/// silently drop every compiler error (the mistake that shipped in
/// agentmemory#539). Merge-safe (preserves every other key/hook) and idempotent
/// (replaces an existing cortex compact_output hook rather than duplicating).
fn ensure_compact_hook(root: &Path, local: bool, force: bool) -> Result<HookOutcome> {
    let claude_dir = root.join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("failed to create {}", claude_dir.display()))?;
    let filename = if local { "settings.local.json" } else { "settings.json" };
    let settings_path = claude_dir.join(filename);

    let compact_hook = json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "mcp_tool",
            "server": "cortex",
            "tool": "compact_output",
            "input": {
                "command": "${tool_input.command}",
                "stdout": "${tool_response.stdout}",
                "stderr": "${tool_response.stderr}"
            }
        }]
    });

    let mut root_obj: serde_json::Map<String, Value> = if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("failed to read {}", settings_path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", settings_path.display()))?
    } else {
        serde_json::Map::new()
    };

    let hooks = root_obj
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {filename} is not an object"))?;
    hooks_obj
        .entry("PostToolUse".to_string())
        .or_insert_with(|| Value::Array(vec![]));
    hooks_obj
        .entry("UserPromptSubmit".to_string())
        .or_insert_with(|| Value::Array(vec![]));

    // The edit guard rides the same mechanism: an mcp_tool hook needs no shell,
    // no binary path and no per-machine wiring, so it installs itself with the
    // compaction hook and there is nothing for anyone to remember. It returns an
    // empty string unless an edit touches a recorded trap, which is why it can
    // be attached to every edit without becoming noise.
    let guard_hook = json!({
        "matcher": "Edit|Write",
        "hooks": [{
            "type": "mcp_tool",
            "server": "cortex",
            "tool": "edit_guard",
            "input": {
                "file_path": "${tool_input.file_path}",
                "added": "${tool_input.new_string}",
                "content": "${tool_input.content}"
            }
        }]
    });

    // Corrections ride the same mechanism, on a different event. This is the
    // only hook that watches the CHAT rather than the tools, and it exists
    // because an agent that has just been corrected is the worst possible
    // witness to the fact — the record has to be made by something with no
    // stake in it. It returns an empty string for almost every message.
    let challenge_hook = json!({
        "hooks": [{
            "type": "mcp_tool",
            "server": "cortex",
            "tool": "note_challenge",
            "input": { "prompt": "${prompt}" }
        }]
    });

    let names_tool = |entry: &Value, tool: &str| -> bool {
        entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("tool").and_then(|t| t.as_str()) == Some(tool)
                    && h.get("server").and_then(|s| s.as_str()) == Some("cortex")
            })
        })
    };
    let is_compact = |e: &Value| names_tool(e, "compact_output");
    let is_guard = |e: &Value| names_tool(e, "edit_guard");

    let is_challenge = |e: &Value| names_tool(e, "note_challenge");

    // A hook is up to date only if it is present AND byte-identical to what we
    // would write. Anything else is refreshed — including a hook from an older
    // cortex that passed the wrong template variable, which would otherwise
    // survive forever looking installed.
    let array_of = |obj: &serde_json::Map<String, Value>, key: &str| -> Vec<Value> {
        obj.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default()
    };
    let post_now = array_of(hooks_obj, "PostToolUse");
    let prompt_now = array_of(hooks_obj, "UserPromptSubmit");
    let up_to_date = post_now.iter().find(|e| is_compact(e)).is_some_and(|e| *e == compact_hook)
        && post_now.iter().find(|e| is_guard(e)).is_some_and(|e| *e == guard_hook)
        && prompt_now.iter().find(|e| is_challenge(e)).is_some_and(|e| *e == challenge_hook);
    if !force && up_to_date {
        return Ok(HookOutcome::AlreadyPresent);
    }

    // PostToolUse: output compaction and the edit guard.
    let arr = hooks_obj
        .get_mut("PostToolUse")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("`hooks.PostToolUse` in {filename} is not an array"))?;
    arr.retain(|e| !is_compact(e) && !is_guard(e));
    arr.push(compact_hook);
    arr.push(guard_hook);

    // UserPromptSubmit is a SEPARATE event array. Pushing this onto PostToolUse
    // would be accepted by the JSON and then never fire — exactly the shipped-
    // and-doing-nothing failure this whole subsystem exists to catch.
    let prompt_arr = hooks_obj
        .get_mut("UserPromptSubmit")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("`hooks.UserPromptSubmit` in {filename} is not an array"))?;
    prompt_arr.retain(|e| !is_challenge(e));
    prompt_arr.push(challenge_hook);

    let rendered = serde_json::to_string_pretty(&Value::Object(root_obj))?;
    std::fs::write(&settings_path, rendered)
        .with_context(|| format!("failed to write {}", settings_path.display()))?;
    Ok(HookOutcome::Written)
}

fn run_hooks_init(root: Option<PathBuf>, shared: bool, force: bool) -> Result<()> {
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let outcome = ensure_compact_hook(&root, !shared, force)?;
    let filename = if shared { "settings.json" } else { "settings.local.json" };
    match outcome {
        HookOutcome::Written => println!(
            "Wrote .claude/{filename} — three cortex hooks installed:\n\
             \x20 compact_output on PostToolUse(Bash) — losslessly strips build/test progress \
             noise (stdout + stderr) and tees the full log to .cortex/tee/.\n\
             \x20 edit_guard on PostToolUse(Edit|Write) — names a recorded trap when an edit \
             touches one. Silent otherwise, and capped at one warning per file and four per \
             session, so it cannot become wallpaper.\n\
             \x20 note_challenge on UserPromptSubmit — records that a claim was disputed, so a \
             correction cannot be quietly dropped by the party that received it. Records a \
             QUESTION, never a finding: nothing reaches memory until someone checks who was \
             right, and even then it arrives as a proposal.\n\
             Restart Claude Code (or reload the session) for them to take effect.\n\
             Note: these are Claude Code hooks. VS Code Copilot cannot auto-rewrite tool output \
             or observe edits — it can still call the MCP tools directly (via .vscode/mcp.json)."
        ),
        HookOutcome::AlreadyPresent => {
            println!("cortex compact_output hook already present in .claude/{filename} — no change.")
        }
    }
    Ok(())
}

/// True when this process looks like it is running inside Claude Code — either
/// Claude Code set its entrypoint env var, or the repo already has a `.claude/`
/// directory (the project has used Claude Code before). Used to decide whether
/// the serve-time auto-install of the compact_output hook is "applicable".
fn is_claude_code_context(repo: &Path) -> bool {
    std::env::var_os("CLAUDECODE").is_some()
        || std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_some()
        || repo.join(".claude").is_dir()
}

/// Auto-install the compact_output hook the FIRST time cortex serves a Claude
/// Code project, then never again (a sentinel file records the install). This
/// makes the token saving automatic for existing projects without a re-bootstrap,
/// while never re-adding the hook if the user deliberately removes it.
///
/// Non-fatal by contract: any failure is logged and serving continues.
fn auto_install_hook_on_serve(repo: &Path) {
    // Explicit opt-out for users who don't want cortex touching their config.
    if std::env::var_os("CORTEX_NO_AUTO_HOOKS").is_some() {
        return;
    }
    if !is_claude_code_context(repo) {
        return; // Copilot / unknown host — the MCP tool is still available.
    }
    // The sentinel is versioned. Without that, adding a hook ships it to nobody:
    // every machine that ever ran cortex already has the unversioned sentinel,
    // so the install is skipped forever and the new mechanism reports NEVER in
    // the audit with no explanation. Bump this whenever the hook set changes.
    const HOOK_SET_VERSION: u32 = 2; // 2: added note_challenge on UserPromptSubmit
    let cortex_dir = repo.join(".cortex");
    let sentinel = cortex_dir.join(format!(".claude-hooks-installed.v{HOOK_SET_VERSION}"));
    if sentinel.exists() {
        return; // This hook set already handled — respect the user's later edits.
    }
    // Clear the previous generation's marker so it cannot accumulate.
    let _ = std::fs::remove_file(cortex_dir.join(".claude-hooks-installed"));
    for old in 1..HOOK_SET_VERSION {
        let _ = std::fs::remove_file(cortex_dir.join(format!(".claude-hooks-installed.v{old}")));
    }
    // Personal, git-ignored settings: the hook targets THIS machine's cortex
    // server, so it must not be committed into a teammate's checkout.
    match ensure_compact_hook(repo, /* local */ true, /* force */ false) {
        Ok(outcome) => {
            let _ = std::fs::create_dir_all(repo.join(".cortex"));
            let _ = std::fs::write(&sentinel, chrono::Utc::now().to_rfc3339());
            match outcome {
                HookOutcome::Written => eprintln!(
                    "  hooks: installed compact_output into .claude/settings.local.json \
                     (lossless output compaction; active next Claude Code session)"
                ),
                HookOutcome::AlreadyPresent => eprintln!(
                    "  hooks: compact_output already configured; recorded install sentinel"
                ),
            }
        }
        Err(e) => eprintln!("  hooks: auto-install skipped ({e})"),
    }
}

fn run_bootstrap(args: BootstrapArgs, db_path: &Path) -> Result<()> {
    let repo = args.repo;
    let cortex_dir = repo.join(".cortex");
    let vscode_dir = repo.join(".vscode");
    let primary_source = args.source.clone();
    std::fs::create_dir_all(&cortex_dir)
        .with_context(|| format!("failed to create {}", cortex_dir.display()))?;
    std::fs::create_dir_all(&vscode_dir)
        .with_context(|| format!("failed to create {}", vscode_dir.display()))?;

    let project_name = args.name.unwrap_or_else(|| {
        repo.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Project".to_string())
    });
    let has_nested_cortex = repo.join("cortex").join("Cargo.toml").exists();
    let mcp_command = if has_nested_cortex {
        "cortex/target/debug/cortex.exe"
    } else {
        "target/debug/cortex.exe"
    };
    let build_manifest = if has_nested_cortex {
        "cortex/Cargo.toml"
    } else {
        "Cargo.toml"
    };

    let script_path = cortex_dir.join("cortex.ps1");
    let sync_continue_path = cortex_dir.join("sync-continue-mcp.ps1");
    let reset_path = cortex_dir.join("cortex-reset.ps1");
    let notes_path = cortex_dir.join("FIRST_RUN_SETUP_NOTES.md");
    let index_path = cortex_dir.join("index-sources.json");
    let mcp_path = vscode_dir.join("mcp.json");

    if args.force || !script_path.exists() {
        std::fs::write(&script_path, bootstrap_cortex_ps1_template())
            .with_context(|| format!("failed to write {}", script_path.display()))?;
        println!("wrote {}", script_path.display());
    } else {
        println!("kept existing {}", script_path.display());
    }

    if args.force || !sync_continue_path.exists() {
        std::fs::write(&sync_continue_path, bootstrap_sync_continue_mcp_template())
            .with_context(|| format!("failed to write {}", sync_continue_path.display()))?;
        println!("wrote {}", sync_continue_path.display());
    } else {
        println!("kept existing {}", sync_continue_path.display());
    }

    if args.force || !reset_path.exists() {
        std::fs::write(&reset_path, bootstrap_cortex_reset_ps1_template())
            .with_context(|| format!("failed to write {}", reset_path.display()))?;
        println!("wrote {}", reset_path.display());
    } else {
        println!("kept existing {}", reset_path.display());
    }

    if args.force || !notes_path.exists() {
        std::fs::write(&notes_path, bootstrap_first_run_notes_template())
            .with_context(|| format!("failed to write {}", notes_path.display()))?;
        println!("wrote {}", notes_path.display());
    } else {
        println!("kept existing {}", notes_path.display());
    }

    if args.force || !index_path.exists() {
        let index_json = json!({
            "targets": [
                {
                    "source": primary_source.clone(),
                    "name": project_name.clone(),
                    "scope": Value::Null,
                }
            ]
        });
        std::fs::write(&index_path, serde_json::to_string_pretty(&index_json)?)
            .with_context(|| format!("failed to write {}", index_path.display()))?;
        println!("wrote {}", index_path.display());
    } else {
        println!("kept existing {}", index_path.display());
    }

    let mut mcp: Value = if mcp_path.exists() {
        let raw = std::fs::read_to_string(&mcp_path)
            .with_context(|| format!("failed to read {}", mcp_path.display()))?;
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "servers": {}, "inputs": [] }))
    } else {
        json!({ "servers": {}, "inputs": [] })
    };

    if !mcp.is_object() {
        mcp = json!({ "servers": {}, "inputs": [] });
    }
    if !mcp.get("servers").map(|v| v.is_object()).unwrap_or(false) {
        mcp["servers"] = json!({});
    }
    if !mcp.get("inputs").map(|v| v.is_array()).unwrap_or(false) {
        mcp["inputs"] = json!([]);
    }

    mcp["servers"]["cortex"] = json!({
        "type": "stdio",
        "command": mcp_command,
        "args": [
            "--db",
            db_path.to_string_lossy().replace('\\', "/"),
            "serve",
            "--source",
            primary_source,
            "--repo",
            ".",
            "--name",
            project_name
        ],
        "description": "Cortex MCP direct binary server. Reindex via .cortex/cortex.ps1 reindex (uses .cortex/index-sources.json)."
    });

    std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp)?)
        .with_context(|| format!("failed to write {}", mcp_path.display()))?;
    println!("updated {}", mcp_path.display());

    println!("\nbootstrap complete:");
    println!("  1. Build cortex binary: cargo build --manifest-path {build_manifest}");
    println!("  2. Repair MCP config: .\\.cortex\\cortex.ps1 setup-mcp");
    println!("  3. Index sources: .\\.cortex\\cortex.ps1 reindex");
    println!("  4. Validate MCP readiness: .\\.cortex\\cortex.ps1 mcp-ready -SelfCheckFormat json");
    println!("  5. Run tooling smoke check: .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json");
    println!("  6. Start MCP server: .\\.cortex\\cortex.ps1 serve");
    Ok(())
}

fn bootstrap_cortex_ps1_template() -> &'static str {
    r#"# cortex.ps1 (bootstrap template)
# Generated by: cortex bootstrap

param(
    [Parameter(Position=0)]
    [string]$Command = "serve",

    [Parameter(Position=1, ValueFromRemainingArguments=$true)]
    [string[]]$Rest,

    [ValidateSet("text", "line", "json")]
    [string]$SelfCheckFormat = "text"
)

$DB = ".cortex\memory.db"
$INDEX_CONFIG = ".cortex\index-sources.json"
$BIN_CANDIDATES = @("cortex\target\debug\cortex.exe", "target\debug\cortex.exe")
$MANIFEST_CANDIDATES = @("cortex\Cargo.toml", "Cargo.toml")
$REPO = "."

function Get-FirstExistingPath {
    param(
        [string[]]$Candidates,
        [string]$Fallback
    )

    foreach ($candidate in $Candidates) {
        if (Test-Path $candidate) {
            return $candidate
        }
    }

    return $Fallback
}

$BIN = Get-FirstExistingPath -Candidates $BIN_CANDIDATES -Fallback "target\debug\cortex.exe"
$MANIFEST = Get-FirstExistingPath -Candidates $MANIFEST_CANDIDATES -Fallback "Cargo.toml"

function Get-McpCommandPath {
    if (Test-Path "cortex\Cargo.toml") {
        return "cortex/target/debug/cortex.exe"
    }
    return "target/debug/cortex.exe"
}

function Write-Prefix {
    param([string]$Message)
    Write-Host "[cortex] $Message"
}

function Ensure-Binary {
    if (-not (Test-Path $BIN)) {
        Write-Error "Cortex binary not found at $BIN. Run: cargo build --manifest-path $MANIFEST"
        exit 1
    }
}

function Get-PrimaryTarget {
    $target = [pscustomobject]@{
        source = "src"
        name = "Project"
        scope = $null
    }

    if (Test-Path $INDEX_CONFIG) {
        try {
            $cfg = Get-Content -Raw -Path $INDEX_CONFIG | ConvertFrom-Json
            if ($cfg.targets -and $cfg.targets.Count -gt 0 -and $cfg.targets[0].source) {
                $target.source = [string]$cfg.targets[0].source
                if ($cfg.targets[0].name) {
                    $target.name = [string]$cfg.targets[0].name
                }
                if ($cfg.targets[0].scope) {
                    $target.scope = [string]$cfg.targets[0].scope
                }
            }
        }
        catch {
            Write-Prefix "WARN: failed to parse $INDEX_CONFIG; using defaults"
        }
    }

    return $target
}

function Get-PrimarySource {
    $target = Get-PrimaryTarget
    return [string]$target.source
}

function Get-PrimaryName {
    $target = Get-PrimaryTarget
    return [string]$target.name
}

function Setup-Mcp {
    $path = ".vscode\mcp.json"
    $cfg = $null

    if (Test-Path $path) {
        try {
            $cfg = Get-Content -Raw -Path $path | ConvertFrom-Json
        }
        catch {
            Write-Prefix "WARN: existing $path is invalid JSON; recreating a minimal config"
            $cfg = $null
        }
    }

    if (-not $cfg) {
        $cfg = [pscustomobject]@{}
    }
    if (-not $cfg.servers) {
        $cfg | Add-Member -NotePropertyName servers -NotePropertyValue ([pscustomobject]@{}) -Force
    }
    if (-not $cfg.inputs) {
        $cfg | Add-Member -NotePropertyName inputs -NotePropertyValue @() -Force
    }

    $source = Get-PrimarySource
    $name = Get-PrimaryName
    $command = Get-McpCommandPath

    $cfg.servers | Add-Member -NotePropertyName cortex -NotePropertyValue ([pscustomobject]@{
        type = "stdio"
        command = $command
        args = @("--db", ".cortex/memory.db", "serve", "--source", $source, "--repo", ".", "--name", $name)
        description = "Cortex MCP direct binary server. Reindex via .cortex/cortex.ps1 reindex (uses .cortex/index-sources.json)."
    }) -Force

    Set-Content -Path $path -Value ($cfg | ConvertTo-Json -Depth 20) -Encoding UTF8
    Write-Prefix "updated $path"
}

function Convert-CortexOutputToJson {
    param([string]$Text)

    if (-not $Text) {
        return $null
    }

    try {
        return ($Text | ConvertFrom-Json)
    }
    catch {}

    $lines = $Text -split "`r?`n"
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        if (-not $trimmed) { continue }
        if (-not (($trimmed.StartsWith("{")) -or ($trimmed.StartsWith("[")))) { continue }
        try {
            return ($trimmed | ConvertFrom-Json)
        }
        catch {}
    }

    return $null
}

function Invoke-LegacyMigrationPathway {
    param(
        [string]$TriggerCommand = ""
    )

    if (-not (Test-Path $DB)) {
        return $true
    }

    $output = (& $BIN --db $DB --format json status --full 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        Write-Prefix "WARN: legacy migration preflight failed for command '$TriggerCommand'."
        Write-Prefix "Run: .\\.cortex\\cortex.ps1 migrate-legacy"
        Write-Prefix "AI workflow prompt: PROTOCOL - CORTEX - migrate .cortex legacy DB and run smoke"
        return $false
    }

    $migrationLine = @($output -split "`r?`n" | Where-Object { $_ -match "legacy outcome application markers detected" } | Select-Object -First 1)
    if ($migrationLine -and $migrationLine.Count -gt 0) {
        Write-Prefix $migrationLine[0].Trim()
        Write-Prefix "Legacy migration pathway applied automatically during startup."
        Write-Prefix "Recommended verification: .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json"
    }

    return $true
}

function Get-StatusCheck {
    $output = (& $BIN --db $DB --format json status --full 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    $json = Convert-CortexOutputToJson -Text $output

    $indexedUnits = 0
    if ($json -and $null -ne $json.indexed_units) {
        $indexedUnits = [int]$json.indexed_units
    }
    elseif ($json -and $json.index -and $json.index.metrics -and $null -ne $json.index.metrics.indexed_units) {
        $indexedUnits = [int]$json.index.metrics.indexed_units
    }

    return [pscustomobject]@{
        ok = ($exitCode -eq 0)
        indexed_units = $indexedUnits
        parsed = [bool]($null -ne $json)
    }
}

function Get-DoctorCheck {
    $source = Get-PrimarySource
    $name = Get-PrimaryName

    $output = (& $BIN --db $DB --format json doctor workflow --repo $REPO --source $source --name $name 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    $json = Convert-CortexOutputToJson -Text $output

    $checksTotal = 0
    $checksPass = 0
    if ($json -and $json.checks) {
        $checksTotal = @($json.checks).Count
        $checksPass = @($json.checks | Where-Object { $_.pass -eq $true }).Count
    }

    return [pscustomobject]@{
        ok = ($exitCode -eq 0)
        workflow_ok = [bool]($json -and $json.ok -eq $true)
        checks_pass = $checksPass
        checks_total = $checksTotal
        parsed = [bool]($null -ne $json)
    }
}

function Write-SelfCheckResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor
    )

    if ($SelfCheckFormat -eq "json") {
        [pscustomobject]@{
            pass = $Pass
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            workflow_ok = $Doctor.workflow_ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            timestamp = (Get-Date).ToString("s")
        } | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        Write-Host ("CORTEX_SELFCHECK {0} status_ok={1} doctor_ok={2} workflow_ok={3} indexed_units={4} checks={5}/{6}" -f $resultText, $Status.ok, $Doctor.ok, $Doctor.workflow_ok, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total)
        return
    }

    if ($Pass) {
        Write-Prefix "selfcheck: PASS"
    }
    else {
        Write-Prefix "selfcheck: FAIL"
    }
}

function Invoke-SelfCheck {
    $status = Get-StatusCheck
    $doctor = Get-DoctorCheck
    $pass = $status.ok -and $doctor.ok -and $doctor.workflow_ok -and ($status.indexed_units -gt 0)
    Write-SelfCheckResult -Pass $pass -Status $status -Doctor $doctor
    return $pass
}

function Read-JsonRpcResponse {
    param(
        [Parameter(Mandatory=$true)]
        [System.Diagnostics.Process]$Process,

        [Parameter(Mandatory=$true)]
        [int]$ExpectedId,

        [int]$TimeoutMs = 10000
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        $task = $Process.StandardOutput.ReadLineAsync()
        if (-not $task.Wait(500)) {
            continue
        }

        $line = $task.Result
        if ($null -eq $line) {
            break
        }

        $trim = $line.Trim()
        if (-not $trim) {
            continue
        }

        try {
            $json = $trim | ConvertFrom-Json
        }
        catch {
            continue
        }

        if ($null -ne $json.id -and [int]$json.id -eq $ExpectedId) {
            return $json
        }
    }

    return $null
}

function Test-McpToolSurface {
    param(
        [string[]]$RequiredTools = @("get_delta", "get_preferences", "get_anti_patterns", "list_patterns", "get_context"),
        [int]$TimeoutMs = 12000
    )

    if (-not (Test-Path $BIN)) {
        return [pscustomobject]@{
            ok = $false
            reason = "missing_binary"
            tools = @()
            tool_defs = @()
            missing_tools = $RequiredTools
        }
    }

    $source = Get-PrimarySource
    $name = Get-PrimaryName

    $proc = $null
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $BIN
        $psi.Arguments = "--db `"$DB`" serve --source `"$source`" --repo `"$REPO`" --name `"$name`""
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true

        $proc = New-Object System.Diagnostics.Process
        $proc.StartInfo = $psi
        $started = $proc.Start()
        if (-not $started) {
            return [pscustomobject]@{
                ok = $false
                reason = "process_start_failed"
                tools = @()
                tool_defs = @()
                missing_tools = $RequiredTools
            }
        }

        $initReq = @{
            jsonrpc = "2.0"
            id = 1
            method = "initialize"
            params = @{
                protocolVersion = "2024-11-05"
                capabilities = @{}
                clientInfo = @{ name = "cortex.ps1"; version = "1.0" }
            }
        } | ConvertTo-Json -Depth 8 -Compress

        $toolsReq = @{
            jsonrpc = "2.0"
            id = 2
            method = "tools/list"
            params = @{}
        } | ConvertTo-Json -Depth 8 -Compress

        $proc.StandardInput.WriteLine($initReq)
        $proc.StandardInput.Flush()
        $null = Read-JsonRpcResponse -Process $proc -ExpectedId 1 -TimeoutMs $TimeoutMs

        $proc.StandardInput.WriteLine($toolsReq)
        $proc.StandardInput.Flush()
        $toolsResp = Read-JsonRpcResponse -Process $proc -ExpectedId 2 -TimeoutMs $TimeoutMs

        if ($null -eq $toolsResp -or $null -eq $toolsResp.result -or $null -eq $toolsResp.result.tools) {
            return [pscustomobject]@{
                ok = $false
                reason = "tools_list_unavailable"
                tools = @()
                tool_defs = @()
                missing_tools = $RequiredTools
            }
        }

        $toolNames = @()
        foreach ($tool in $toolsResp.result.tools) {
            if ($tool.name) {
                $toolNames += [string]$tool.name
            }
        }

        $missing = @($RequiredTools | Where-Object { $toolNames -notcontains $_ })
        return [pscustomobject]@{
            ok = ($missing.Count -eq 0)
            reason = if ($missing.Count -eq 0) { "ok" } else { "missing_required_tools" }
            tools = $toolNames
            tool_defs = @($toolsResp.result.tools)
            missing_tools = $missing
        }
    }
    catch {
        return [pscustomobject]@{
            ok = $false
            reason = "exception"
            detail = [string]$_
            tools = @()
            tool_defs = @()
            missing_tools = $RequiredTools
        }
    }
    finally {
        if ($proc) {
            try {
                if (-not $proc.HasExited) {
                    $proc.Kill()
                }
            }
            catch {}
            $proc.Dispose()
        }
    }
}

function Write-McpReadyResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor,
        [object]$Probe
    )

    if ($SelfCheckFormat -eq "json") {
        [pscustomobject]@{
            pass = $Pass
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            workflow_ok = $Doctor.workflow_ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            mcp_tools_ok = $Probe.ok
            mcp_reason = $Probe.reason
            missing_tools = $Probe.missing_tools
            tool_count = @($Probe.tools).Count
            timestamp = (Get-Date).ToString("s")
        } | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        $missing = if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) { $Probe.missing_tools -join "," } else { "none" }
        Write-Host ("CORTEX_MCP_READY {0} status_ok={1} doctor_ok={2} workflow_ok={3} indexed_units={4} checks={5}/{6} mcp_tools_ok={7} missing_tools={8}" -f $resultText, $Status.ok, $Doctor.ok, $Doctor.workflow_ok, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total, $Probe.ok, $missing)
        return
    }

    if ($Pass) {
        Write-Prefix "mcp-ready: PASS"
        Write-Prefix "Baseline MCP tools are available."
    }
    else {
        Write-Prefix "mcp-ready: FAIL"
        if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) {
            Write-Prefix ("Missing required MCP tools: {0}" -f ($Probe.missing_tools -join ", "))
        }
        Write-Prefix "If mcp-ready passes but chat still lacks wrappers, the limitation is in chat tool exposure, not cortex server registration."
    }
}

function Invoke-McpReady {
    $status = Get-StatusCheck
    $doctor = Get-DoctorCheck
    $probe = Test-McpToolSurface
    $healthOk = $status.ok -and $doctor.ok -and $doctor.workflow_ok -and ($status.indexed_units -gt 0)
    $pass = $healthOk -and $probe.ok
    Write-McpReadyResult -Pass $pass -Status $status -Doctor $doctor -Probe $probe
    return $pass
}

function Test-ToolSchemaProperty {
    param(
        [object]$Probe,
        [string]$ToolName,
        [string]$PropertyName
    )

    if (-not $Probe -or -not $Probe.tool_defs) {
        return $false
    }

    $tool = @($Probe.tool_defs | Where-Object { $_.name -eq $ToolName } | Select-Object -First 1)
    if (-not $tool -or $tool.Count -eq 0) {
        return $false
    }

    $schema = $tool[0].inputSchema
    if (-not $schema -or -not $schema.properties) {
        return $false
    }

    $propertyNames = @($schema.properties.PSObject.Properties.Name)
    return ($propertyNames -contains $PropertyName)
}

function Write-SmokeResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor,
        [object]$Probe,
        [bool]$RelationFilterOk
    )

    if ($SelfCheckFormat -eq "json") {
        [pscustomobject]@{
            pass = $Pass
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            workflow_ok = $Doctor.workflow_ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            mcp_tools_ok = $Probe.ok
            relation_filter_ok = $RelationFilterOk
            missing_tools = $Probe.missing_tools
            tool_count = @($Probe.tools).Count
            timestamp = (Get-Date).ToString("s")
        } | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        $missing = if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) { $Probe.missing_tools -join "," } else { "none" }
        Write-Host ("CORTEX_SMOKE {0} status_ok={1} doctor_ok={2} workflow_ok={3} indexed_units={4} checks={5}/{6} mcp_tools_ok={7} relation_filter_ok={8} missing_tools={9}" -f $resultText, $Status.ok, $Doctor.ok, $Doctor.workflow_ok, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total, $Probe.ok, $RelationFilterOk, $missing)
        return
    }

    if ($Pass) {
        Write-Prefix "smoke: PASS"
        Write-Prefix "Baseline and extended MCP tooling checks passed."
    }
    else {
        Write-Prefix "smoke: FAIL"
        if (-not $Probe.ok -and $Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) {
            Write-Prefix ("Missing required tools: {0}" -f ($Probe.missing_tools -join ", "))
        }
        if (-not $RelationFilterOk) {
            Write-Prefix "simulate_change schema missing relation_filter property"
        }
    }
}

function Invoke-Smoke {
    $requiredTools = @(
        "get_delta",
        "get_preferences",
        "get_anti_patterns",
        "list_patterns",
        "get_context",
        "get_usage_examples",
        "get_helper",
        "explain_dependency_path",
        "simulate_change"
    )

    $status = Get-StatusCheck
    $doctor = Get-DoctorCheck
    $probe = Test-McpToolSurface -RequiredTools $requiredTools
    $relationFilterOk = Test-ToolSchemaProperty -Probe $probe -ToolName "simulate_change" -PropertyName "relation_filter"

    $healthOk = $status.ok -and $doctor.ok -and $doctor.workflow_ok -and ($status.indexed_units -gt 0)
    $pass = $healthOk -and $probe.ok -and $relationFilterOk

    Write-SmokeResult -Pass $pass -Status $status -Doctor $doctor -Probe $probe -RelationFilterOk $relationFilterOk
    return $pass
}

$skipLegacyMigrationPreflight = @("setup-mcp", "migrate-legacy")
if ($skipLegacyMigrationPreflight -notcontains $Command) {
    $null = Invoke-LegacyMigrationPathway -TriggerCommand $Command
}

Ensure-Binary

switch ($Command) {
    "serve" {
        $target = Get-PrimaryTarget
        & $BIN --db $DB serve --source $target.source --repo $REPO --name $target.name
        exit $LASTEXITCODE
    }
    "reindex" {
        $targets = @()
        if (Test-Path $INDEX_CONFIG) {
            try {
                $cfg = Get-Content -Raw -Path $INDEX_CONFIG | ConvertFrom-Json
                if ($cfg.targets) {
                    $targets = @($cfg.targets)
                }
            }
            catch {
                Write-Prefix "WARN: failed to parse $INDEX_CONFIG; using fallback target"
            }
        }

        if (-not $targets -or $targets.Count -eq 0) {
            $primary = Get-PrimaryTarget
            $targets = @($primary)
        }

        $indexed = 0
        foreach ($t in $targets) {
            if (-not $t.source) { continue }

            $source = [string]$t.source
            if (-not (Test-Path $source)) {
                Write-Prefix "WARN: skipping missing source path $source"
                continue
            }

            $name = if ($t.name) { [string]$t.name } else { "Project" }
            $args = @("--db", $DB, "index", "--source", $source, "--name", $name)
            if ($t.scope) {
                $args += @("--scope", [string]$t.scope)
            }

            & $BIN @args
            if ($LASTEXITCODE -ne 0) {
                exit $LASTEXITCODE
            }
            $indexed++
        }

        if ($indexed -eq 0) {
            Write-Error "No valid source paths were indexed. Update .cortex/index-sources.json or pass an existing path."
            exit 1
        }

        Write-Prefix "reindex complete"
    }
    "setup-mcp" {
        Setup-Mcp
    }
    "sync-continue-mcp" {
        $syncScript = Join-Path $PSScriptRoot "sync-continue-mcp.ps1"
        if (-not (Test-Path $syncScript)) {
            Write-Error "Missing sync helper script: $syncScript"
            exit 1
        }

        & $syncScript @Rest
        $syncExit = $LASTEXITCODE
        if ($syncExit -ne 0) { exit $syncExit }
    }
    "status" {
        & $BIN --db $DB --format json status --full
        exit $LASTEXITCODE
    }
    "doctor" {
        $source = Get-PrimarySource
        $name = Get-PrimaryName
        & $BIN --db $DB --format json doctor workflow --repo $REPO --source $source --name $name
        exit $LASTEXITCODE
    }
    "selfcheck" {
        $ok = Invoke-SelfCheck
        if (-not $ok) { exit 1 }
    }
    "mcp-ready" {
        $ok = Invoke-McpReady
        if (-not $ok) { exit 1 }
    }
    "smoke" {
        $ok = Invoke-Smoke
        if (-not $ok) { exit 1 }
    }
    "migrate-legacy" {
        $ok = Invoke-LegacyMigrationPathway -TriggerCommand $Command
        if (-not $ok) { exit 1 }
        Write-Prefix "legacy migration pathway complete"
        Write-Prefix "next: .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json"
    }
    "--" {
        & $BIN --db $DB @Rest
        exit $LASTEXITCODE
    }
    default {
        & $BIN --db $DB $Command @Rest
        exit $LASTEXITCODE
    }
}
"#
}

fn bootstrap_cortex_reset_ps1_template() -> &'static str {
    r#"# cortex-reset.ps1 (bootstrap template)
# Soft-default helper for stale cortex.exe lock cleanup.

param(
    [switch]$rebuild,
    [switch]$full,
    [switch]$Aggressive,
    [switch]$ForceKill,
    [switch]$PurgeBinaries
)

if ($Aggressive) {
    $ForceKill = $true
    $PurgeBinaries = $true
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
if (Test-Path (Join-Path $RepoRoot "cortex\Cargo.toml")) {
    $CortexRoot = Join-Path $RepoRoot "cortex"
}
else {
    $CortexRoot = $RepoRoot
}
$DebugBinary = Join-Path $CortexRoot "target\debug\cortex.exe"
$ReleaseBinary = Join-Path $CortexRoot "target\release\cortex.exe"

function Get-CortexProcesses {
    Get-Process -Name "cortex" -ErrorAction SilentlyContinue
}

function Stop-CortexProcesses {
    param([switch]$UseForce)
    $procs = Get-CortexProcesses
    if (-not $procs) { return }

    foreach ($proc in $procs) {
        try {
            if ($UseForce) {
                Stop-Process -Id $proc.Id -Force -ErrorAction Stop
            }
            else {
                Stop-Process -Id $proc.Id -ErrorAction Stop
            }
        }
        catch {}
    }
}

function Remove-Binaries {
    param([string[]]$Paths)
    foreach ($path in $Paths) {
        if (-not (Test-Path $path)) { continue }
        try { Remove-Item $path -Force -ErrorAction Stop } catch {}
    }
}

Write-Host "[cortex-reset] Step 1/3: stopping cortex processes"
Stop-CortexProcesses

$remaining = Get-CortexProcesses
if ($remaining -and $remaining.Count -gt 0) {
    if ($ForceKill) {
        Write-Host "[cortex-reset] Step 1b/3: force-killing remaining cortex processes"
        Stop-CortexProcesses -UseForce
    }
    else {
        Write-Host "[cortex-reset] Some processes are still running. Re-run with -Aggressive if lock persists."
    }
}

if ($PurgeBinaries) {
    Write-Host "[cortex-reset] Step 2/3: removing cortex binaries"
    Remove-Binaries -Paths @($DebugBinary, $ReleaseBinary)
}
else {
    Write-Host "[cortex-reset] Step 2/3: skipping binary removal (soft mode)"
}

$buildExit = 0
if ($full -or $rebuild) {
    Push-Location $CortexRoot
    try {
        if ($full) {
            Write-Host "[cortex-reset] Step 3/3: cargo clean"
            & cargo clean
            if ($LASTEXITCODE -ne 0) { $buildExit = $LASTEXITCODE }
        }

        if ($rebuild) {
            Write-Host "[cortex-reset] Step 3/3: cargo build --quiet"
            & cargo build --quiet
            if ($LASTEXITCODE -ne 0) { $buildExit = $LASTEXITCODE }
        }
    }
    finally {
        Pop-Location
    }
}

Write-Host "[cortex-reset] Reset complete"
if ($buildExit -ne 0) {
    exit $buildExit
}
"#
}

fn bootstrap_sync_continue_mcp_template() -> &'static str {
    r##"# sync-continue-mcp.ps1 (bootstrap template)
# Sync workspace MCP servers (.vscode/mcp.json) into Continue config (~/.continue/config.yaml).

param(
    [string]$WorkspaceRoot = "",
    [string]$McpConfigPath = "",
    [string]$ContinueConfigPath = "",
    [switch]$DryRun
)

function Write-Prefix {
    param([string]$Message)
    Write-Host "[continue-sync] $Message"
}

function Convert-ToYamlQuoted {
    param([string]$Value)

    if ($null -eq $Value) {
        return '""'
    }

    $escaped = $Value -replace '\\', '\\\\' -replace '"', '\\"'
    return '"' + $escaped + '"'
}

function Convert-ToYamlPath {
    param([string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $Value
    }

    return ($Value -replace '\\', '/')
}

function Resolve-WorkspacePath {
    param(
        [string]$Workspace,
        [string]$Value,
        [switch]$AllowNonExisting
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $Value
    }

    if ($Value -match '^[a-zA-Z]+://') {
        return $Value
    }

    $looksLikePath = ($Value -match '[\\/]') -or $Value.StartsWith('.')
    if (-not $looksLikePath) {
        return $Value
    }

    if ([System.IO.Path]::IsPathRooted($Value)) {
        return $Value
    }

    $candidate = Join-Path $Workspace $Value
    if ((Test-Path $candidate) -or $AllowNonExisting) {
        return $candidate
    }

    return $Value
}

if ([string]::IsNullOrWhiteSpace($WorkspaceRoot)) {
    $WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
}

if ([string]::IsNullOrWhiteSpace($McpConfigPath)) {
    $McpConfigPath = Join-Path $WorkspaceRoot ".vscode\mcp.json"
}

if ([string]::IsNullOrWhiteSpace($ContinueConfigPath)) {
    $ContinueConfigPath = Join-Path $env:USERPROFILE ".continue\config.yaml"
}

$WorkspaceRoot = (Resolve-Path $WorkspaceRoot).Path
$workspaceRootYaml = Convert-ToYamlPath -Value $WorkspaceRoot

if (-not (Test-Path $McpConfigPath)) {
    Write-Error "Workspace MCP config not found: $McpConfigPath"
    exit 1
}

if (-not (Test-Path $ContinueConfigPath)) {
    Write-Error "Continue config not found: $ContinueConfigPath"
    exit 1
}

try {
    $mcp = Get-Content -Raw -Path $McpConfigPath | ConvertFrom-Json
}
catch {
    Write-Error ("Failed to parse JSON in " + $McpConfigPath + "; details: " + $_)
    exit 1
}

if (-not $mcp.servers) {
    Write-Error "No servers object found in $McpConfigPath"
    exit 1
}

$serverProps = @($mcp.servers.PSObject.Properties)
if ($serverProps.Count -eq 0) {
    Write-Error "No MCP servers found in $McpConfigPath"
    exit 1
}

$managedStart = "# BEGIN FLOWMAKE MCP SYNC - DO NOT EDIT"
$managedEnd = "# END FLOWMAKE MCP SYNC - DO NOT EDIT"

$generated = New-Object System.Collections.Generic.List[string]
$generated.Add($managedStart)
$generated.Add("mcpServers:")

foreach ($prop in $serverProps) {
    $name = [string]$prop.Name
    $srv = $prop.Value

    $typeRaw = ""
    if ($srv.PSObject.Properties.Name -contains "type") {
        $typeRaw = [string]$srv.type
    }
    $type = $typeRaw.ToLowerInvariant()

    $isHttp = @("http", "sse", "streamable-http") -contains $type
    if (-not $isHttp -and ($srv.PSObject.Properties.Name -contains "url")) {
        $isHttp = $true
    }

    $generated.Add(("  - name: {0}" -f (Convert-ToYamlQuoted -Value $name)))

    if ($isHttp) {
        $continueType = if ($type -eq "sse") { "sse" } else { "streamable-http" }

        $url = ""
        if ($srv.PSObject.Properties.Name -contains "url") {
            $url = [string]$srv.url
        }

        if ([string]::IsNullOrWhiteSpace($url)) {
            Write-Prefix "WARN: skipping server '$name' because url is missing"
            $generated.RemoveAt($generated.Count - 1)
            continue
        }

        $timeout = if ($name -match "(?i)shadervine") { 2500 } else { 5000 }

        $generated.Add(("    type: {0}" -f $continueType))
        $generated.Add(("    url: {0}" -f (Convert-ToYamlQuoted -Value $url)))
        $generated.Add(("    connectionTimeout: {0}" -f $timeout))
        continue
    }

    $command = ""
    if ($srv.PSObject.Properties.Name -contains "command") {
        $command = [string]$srv.command
    }

    if ([string]::IsNullOrWhiteSpace($command)) {
        Write-Prefix "WARN: skipping server '$name' because command is missing"
        $generated.RemoveAt($generated.Count - 1)
        continue
    }

    $resolvedCommand = Resolve-WorkspacePath -Workspace $WorkspaceRoot -Value $command
    $commandYaml = Convert-ToYamlPath -Value $resolvedCommand

    $generated.Add("    type: stdio")
    $generated.Add(("    command: {0}" -f (Convert-ToYamlQuoted -Value $commandYaml)))

    $serverArgs = @()
    if ($srv.PSObject.Properties.Name -contains "args") {
        $serverArgs = @($srv.args)
    }

    if ($serverArgs.Count -gt 0) {
        $pathFlags = @("--source", "--db", "--repo", "--api-graph", "--manifest-path")
        $normalizedArgs = New-Object System.Collections.Generic.List[string]

        for ($idx = 0; $idx -lt $serverArgs.Count; $idx++) {
            $argText = [string]$serverArgs[$idx]
            $prevArg = if ($idx -gt 0) { [string]$serverArgs[$idx - 1] } else { "" }

            $resolvedArg = $argText
            if ($pathFlags -contains $prevArg) {
                if ($prevArg -eq "--repo" -and $argText -eq ".") {
                    $resolvedArg = $WorkspaceRoot
                }
                else {
                    $resolvedArg = Resolve-WorkspacePath -Workspace $WorkspaceRoot -Value $argText -AllowNonExisting
                }
            }

            $normalizedArgs.Add($resolvedArg)
        }

        $generated.Add("    args:")
        foreach ($arg in $normalizedArgs) {
            $argYaml = Convert-ToYamlPath -Value ([string]$arg)
            $generated.Add(("      - {0}" -f (Convert-ToYamlQuoted -Value $argYaml)))
        }
    }

    $generated.Add(("    cwd: {0}" -f (Convert-ToYamlQuoted -Value $workspaceRootYaml)))
    $generated.Add("    connectionTimeout: 20000")
}

$generated.Add($managedEnd)

$lines = @()
$lines = Get-Content -Path $ContinueConfigPath

$startIdx = -1
$endIdx = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i].Trim() -eq $managedStart) {
        $startIdx = $i
        break
    }
}

if ($startIdx -ge 0) {
    for ($i = $startIdx + 1; $i -lt $lines.Count; $i++) {
        if ($lines[$i].Trim() -eq $managedEnd) {
            $endIdx = $i
            break
        }
    }
    if ($endIdx -lt 0) {
        $endIdx = $lines.Count - 1
    }

    $before = @()
    if ($startIdx -gt 0) {
        $before = $lines[0..($startIdx - 1)]
    }

    $after = @()
    if (($endIdx + 1) -le ($lines.Count - 1)) {
        $after = $lines[($endIdx + 1)..($lines.Count - 1)]
    }

    $lines = @($before + $after)
}

$mcpKeyIdx = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^mcpServers\s*:\s*$') {
        $mcpKeyIdx = $i
        break
    }
}

if ($mcpKeyIdx -ge 0) {
    $blockEnd = $lines.Count
    for ($j = $mcpKeyIdx + 1; $j -lt $lines.Count; $j++) {
        if ($lines[$j] -match '^[A-Za-z_][A-Za-z0-9_-]*\s*:') {
            $blockEnd = $j
            break
        }
    }

    $before = @()
    if ($mcpKeyIdx -gt 0) {
        $before = $lines[0..($mcpKeyIdx - 1)]
    }

    $after = @()
    if ($blockEnd -le ($lines.Count - 1)) {
        $after = $lines[$blockEnd..($lines.Count - 1)]
    }

    $lines = @($before + $after)
}

$final = New-Object System.Collections.Generic.List[string]
foreach ($line in $lines) {
    $final.Add($line)
}

while ($final.Count -gt 0 -and [string]::IsNullOrWhiteSpace($final[$final.Count - 1])) {
    $final.RemoveAt($final.Count - 1)
}

if ($final.Count -gt 0) {
    $final.Add("")
}

foreach ($line in $generated) {
    $final.Add($line)
}
$final.Add("")

if ($DryRun) {
    Write-Prefix "Dry-run mode: preview only"
    $final -join "`n" | Write-Output
    exit 0
}

Set-Content -Path $ContinueConfigPath -Value $final -Encoding UTF8
Write-Prefix ("Synced {0} server(s) from {1} into {2}" -f $serverProps.Count, $McpConfigPath, $ContinueConfigPath)
exit 0
"##
}

fn bootstrap_first_run_notes_template() -> &'static str {
    r#"# First-run setup notes

## Why this exists

Cortex can bootstrap launcher scripts and MCP wiring directly from the cortex repo.

## Launcher support

- `.cortex/cortex.ps1 setup-mcp` writes or repairs the Cortex MCP entry.
- `.cortex/cortex.ps1 selfcheck -SelfCheckFormat json` validates status and doctor health.
- `.cortex/cortex.ps1 mcp-ready -SelfCheckFormat json` validates required MCP baseline tools from the server tool registry.
- `.cortex/cortex.ps1 smoke -SelfCheckFormat json` validates baseline + extended MCP tool surface and schema shape.
- `.cortex/cortex.ps1 reindex` indexes configured targets from `.cortex/index-sources.json`.

## New-user sequence

1. Build cortex binary.
2. Run `./.cortex/cortex.ps1 setup-mcp`.
3. Run `./.cortex/cortex.ps1 reindex`.
4. Run `./.cortex/cortex.ps1 mcp-ready -SelfCheckFormat json`.
5. Run `./.cortex/cortex.ps1 smoke -SelfCheckFormat json`.
6. Start `./.cortex/cortex.ps1 serve` (or let VS Code start MCP).
"#
}

// ── Command handlers ──────────────────────────────────────────────────────────

/// Emit the manifest's targets as `source<TAB>name<TAB>scope`.
fn run_manifest(repo: &Path) -> Result<()> {
    let path = repo.join(".cortex").join("index-sources.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(targets) = parsed.get("targets").and_then(|t| t.as_array()) {
        for t in targets {
            let src = t.get("source").and_then(|v| v.as_str()).unwrap_or("");
            if src.is_empty() {
                continue;
            }
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let scope = t.get("scope").and_then(|v| v.as_str()).unwrap_or("");
            println!("{src}	{name}	{scope}");
        }
    }
    Ok(())
}

fn run_index(args: IndexArgs, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;

    eprintln!("cortex index: compressing {}", args.source.display());
    let (mut units, members) = compressor::compress_dir(&args.source, args.scope.as_deref())?;
    eprintln!("  source: {} items compressed, {} members", units.len(), members.len());

    let mut graph_calls: Vec<(String, Vec<model::ApiGraphCall>)> = Vec::new();
    if let Some(graph_path) = &args.api_graph {
        let json = std::fs::read_to_string(graph_path)
            .with_context(|| format!("could not read api-graph: {}", graph_path.display()))?;
        let graph_items: Vec<model::ApiGraphItem> = serde_json::from_str(&json)?;
        // Same scope as the source itself: quartz-ctx ids are unprefixed, so
        // ingesting a scoped source without it would collide with the primary
        // engine's ids and overwrite them.
        // Keep each item's calls alongside the unit id they belong to, so they
        // can be ingested once the graph nodes exist.
        for gi in &graph_items {
            if gi.calls.is_empty() { continue; }
            let raw_module = gi.module_path.join("::");
            let module_path = match args.scope.as_deref() {
                Some(sc) if raw_module.is_empty() => sc.to_string(),
                Some(sc) => format!("{}::{}", sc, raw_module),
                None => raw_module,
            };
            let id = if module_path.is_empty() { gi.name.clone() } else { format!("{}::{}", module_path, gi.name) };
            graph_calls.push((id, gi.calls.clone()));
        }
        let graph_units = compressor::compress_api_graph(&graph_items, args.scope.as_deref());
        // Merge: api-graph items take precedence (they carry full method
        // signatures with types, per-method docs and field docs).
        let source_ids: std::collections::HashSet<String> =
            graph_units.iter().map(|u| u.id.clone()).collect();
        let ingested = source_ids.len();
        let replaced = units.iter().filter(|u| source_ids.contains(&u.id)).count();
        units.retain(|u| !source_ids.contains(&u.id));
        units.extend(graph_units);
        eprintln!(
            "  api-graph: {} items from {} ({} replaced own extraction, {} added)",
            ingested,
            graph_path.display(),
            replaced,
            ingested - replaced,
        );
    }

    // Stamp provenance so orphaned sources can be pruned later. Normalised to
    // forward slashes so the same root indexed from PowerShell and bash agrees.
    let source_root = args.source.to_string_lossy().replace('\\', "/");

    for unit in &units {
        store.upsert_unit_from(unit, Some(&source_root))?;
        store.upsert_symbol_catalog_from_unit(unit)?;
        store.add_symbol_example_if_missing(
            &unit.id,
            &unit.module_path,
            None,
            &unit.compressed,
            "index_unit",
        )?;
    }
    for member in &members {
        store.upsert_member(member)?;
    }

    let synced = graph::sync_nodes(store.conn())?;
    let all_units = store.all_units()?;
    let inferred = graph::infer_edges(store.conn(), &all_units)?;

    // Call edges, if the api-graph carried any. Recorded after node sync so
    // callees can be resolved against the units that were just indexed.
    if !graph_calls.is_empty() {
        // Clear this source's previous call rows so a reindex replaces rather
        // than accumulates. Keyed on the unit ids being re-ingested, because an
        // empty scope prefix in a LIKE would match — and delete — everything.
        for (unit_id, _) in &graph_calls {
            let module = unit_id.rsplit_once("::").map(|(m, _)| m).unwrap_or(unit_id);
            store.conn().execute(
                "DELETE FROM call_graph WHERE source = 'extracted' AND caller LIKE ?1",
                rusqlite::params![format!("{module}::%")],
            ).ok();
            // Same for the derived graph edges, which carry source='calls' so
            // infer_edges cannot delete them on the next source's pass.
            store.conn().execute(
                "DELETE FROM graph_edges WHERE source = 'calls' AND from_id LIKE ?1",
                rusqlite::params![format!("{module}::%")],
            ).ok();
        }
        let mut recorded = 0usize;
        let mut edged = 0usize;
        for (unit_id, calls) in &graph_calls {
            let (r, e) = graph::ingest_calls(store.conn(), unit_id, calls, args.scope.as_deref(), &source_root)?;
            recorded += r;
            edged += e;
        }
        eprintln!(
            "  calls: {} recorded, {} resolved to graph edges ({} left unresolved)",
            recorded, edged, recorded.saturating_sub(edged)
        );
    }

    eprintln!("  total: {} units in index", units.len());
    eprintln!("  graph: {} nodes synced, {} edges inferred", synced, inferred);
    eprintln!("  db: {}", db_path.display());

    // Record what this source looked like at ingest, so the server can tell
    // later whether the store still reflects the code. Keyed by the manifest's
    // own relative path so it lines up with what stale_roots reads.
    let rel = std::env::current_dir()
        .ok()
        .and_then(|cwd| args.source.strip_prefix(&cwd).ok().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| args.source.clone());
    // MAIN_SEPARATOR rather than an escaped backslash literal: the manifest
    // stores forward slashes, and this key has to match what stale_roots builds.
    let rel_key = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let fp = cache::root_fingerprint(&args.source);
    match store.set_meta(&format!("source_fp:{rel_key}"), &fp) {
        Ok(()) => eprintln!("  stamped: source_fp:{rel_key}"),
        Err(e) => eprintln!("  warn: could not stamp source fingerprint for {rel_key}: {e}"),
    }
    eprintln!("\ndone.");
    Ok(())
}

fn run_serve(args: ServeArgs, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;

    // Load units: prefer DB (already indexed), fall back to live parse
    let units = if store.unit_count()? > 0 {
        eprintln!("cortex serve: loading {} units from index", store.unit_count()?);
        store.all_units()?
    } else {
        eprintln!("cortex serve: index empty, compressing {} live", args.source.display());
        let (mut units, _members) = compressor::compress_dir(&args.source, None)?;
        if let Some(graph_path) = &args.api_graph {
            let json = std::fs::read_to_string(graph_path)?;
            let graph_items: Vec<model::ApiGraphItem> = serde_json::from_str(&json)?;
            units.extend(compressor::compress_api_graph(&graph_items, None));
        }
        units
    };

    let prefs_path = args.prefs.unwrap_or_else(default_prefs_path);
    let prefs = prefs::load(&prefs_path).unwrap_or_default();
    let prefs_summary = prefs::render_for_copilot(&prefs);

    eprintln!("  {} units loaded — listening on stdio", units.len());
    // Automatic, install-once setup of the lossless compaction hook for Claude
    // Code projects (no-op for Copilot/other hosts and after the first install).
    auto_install_hook_on_serve(&args.repo);
    mcp::serve(store, units, &args.name, args.repo, prefs_summary)
}

fn run_watch(args: WatchArgs, db_path: &Path) -> Result<()> {
    watcher::watch(&args.source, db_path)
}

fn run_review(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    crystallizer::list_pending(&store)
}

fn run_crystallize(args: CrystallizeArgs, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    crystallizer::crystallize_observation(
        &store,
        args.id,
        &args.name,
        &args.intent,
        args.body.as_deref(),
        args.uses,
        args.tags,
    )
}

fn run_dismiss(args: DismissArgs, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    crystallizer::dismiss_observation(&store, args.id)
}

fn run_context(args: ContextArgs, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let delta_opts = git::DeltaOptions {
        include: args.delta_include,
        exclude: args.delta_exclude,
        max_files: args.delta_max_files,
        max_patch_lines: 40,
    };
    let packet = planner::build_context_packet(
        &store,
        &args.hint,
        args.token_budget,
        Some(&args.repo),
        Some(&delta_opts),
    )?;

    let mut output = String::new();
    let prefs_path = args.prefs.unwrap_or_else(default_prefs_path);
    let prefs = prefs::load(&prefs_path).unwrap_or_default();
    let prefs_summary = prefs::render_for_copilot(&prefs);
    if !prefs_summary.trim().is_empty() {
        // Same tiering the MCP get_context path uses — notes are the bulk of the
        // blob, so expand only the ones this hint touches. Every note stays listed.
        output.push_str(&prefs::tier_notes(&prefs_summary, Some(&args.hint), false));
        output.push('\n');
    }

    output.push_str(&planner::render_packet(&packet));
    print!("{}", output);
    eprintln!("\n[~{} tokens estimated]", packet.estimated_tokens);
    Ok(())
}

fn run_graph(cmd: GraphCmd, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    match cmd {
        GraphCmd::Sync => {
            let units = store.all_units()?;
            let synced = graph::sync_nodes(store.conn())?;
            let inferred = graph::infer_edges(store.conn(), &units)?;
            println!("graph synced: {} nodes, {} inferred edges", synced, inferred);
        }
        GraphCmd::AddPair { from, to, relation } => {
            let rel = model::RelationType::from_str(&relation)
                .unwrap_or(model::RelationType::Pairs);
            graph::add_edge(store.conn(), &from, &to, rel)?;
            println!("added {} edge: {} -> {}", rel.as_str(), from, to);
        }
        GraphCmd::AddConflict { from, to } => {
            graph::add_edge(store.conn(), &from, &to, model::RelationType::Conflicts)?;
            println!("added conflict edge: {} -> {}", from, to);
        }
        GraphCmd::Query { name, depth } => {
            let unit = store.get_unit(&name)?;
            if let Some(u) = unit {
                let (edges, nodes) = graph::subgraph(store.conn(), &u.id, depth)?;
                println!("subgraph root: {}", u.name);
                println!("nodes: {}", nodes.len());
                println!("edges: {}", edges.len());
                for e in edges {
                    println!("{} -[{}]-> {}", e.from_id, e.relation.as_str(), e.to_id);
                }
            } else {
                println!("no unit found for {}", name);
            }
        }
    }
    Ok(())
}

fn run_graph_diff_cmd(db_path: &Path, format: OutputFormat) -> Result<()> {
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));
    let snapshots_dir = repo_root.join(".graphify-output").join("snapshots");
    let current_graph = repo_root.join(".graphify-output").join("graph.json");

    let report = graph_diff::run_graph_diff(&snapshots_dir, &current_graph)?;

    match report {
        None => {
            if format == OutputFormat::Json {
                println!("{}", serde_json::json!({"error": "no previous snapshot available"}));
            } else {
                println!("No previous graph snapshot found. Run closeout first to create one.");
            }
        }
        Some(r) => {
            if format == OutputFormat::Json {
                println!("{}", graph_diff::drift_report_to_json(&r));
            } else {
                println!("=== GRAPH DRIFT REPORT ===");
                println!("Current:  {} ({} nodes, {} links)", r.current_graph_path, r.total_nodes_current, r.total_links_current);
                println!("Previous: {} ({} nodes, {} links)", r.previous_graph_path, r.total_nodes_previous, r.total_links_previous);
                println!("Node delta: {:+}", r.node_count_delta);
                println!("Link delta: {:+}", r.link_count_delta);
                println!("Communities affected: {}/{}", r.communities_affected, r.community_drifts.len());
                println!();
                if r.high_drift_communities.is_empty() {
                    println!("No high-drift communities detected.");
                } else {
                    println!("HIGH-DRIFT COMMUNITIES (score >= 0.3):");
                    for c in &r.high_drift_communities {
                        println!("  Community {}: drift={:.2}, +{}/-{} nodes (now {}, was {})",
                            c.community_id, c.drift_score, c.nodes_added, c.nodes_removed,
                            c.node_count_current, c.node_count_previous);
                    }
                }
                println!("===========================");
            }
        }
    }
    Ok(())
}

fn run_prefs(cmd: PrefsCmd) -> Result<()> {
    match cmd {
        PrefsCmd::Show { path } => {
            let p = path.unwrap_or_else(default_prefs_path);
            let prefs = prefs::load(&p).unwrap_or_default();
            println!("{}", prefs::render_for_copilot(&prefs));
        }
        PrefsCmd::Edit { path } => {
            let p = path.unwrap_or_else(default_prefs_path);
            if !p.exists() {
                prefs::save(&prefs::Preferences::default(), &p)?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
                if cfg!(windows) { "notepad".to_string() } else { "vi".to_string() }
            });
            std::process::Command::new(editor).arg(&p).status()?;
        }
    }
    Ok(())
}

fn default_prefs_path() -> PathBuf {
    PathBuf::from(".cortex/prefs.toml")
}

fn run_pattern(cmd: PatternCmd, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    if format == OutputFormat::Text {
        return match cmd {
            PatternCmd::List => crystallizer::list_patterns(&store),
            PatternCmd::Add { name, intent, body, uses, tags } =>
                crystallizer::add_pattern(&store, &name, &intent, &body, uses, tags),
            PatternCmd::Remove { id } => crystallizer::remove_pattern(&store, id),
            PatternCmd::Revert { id } => crystallizer::report_revert(&store, id),
            PatternCmd::Supersede { id, by } => run_supersede(&store, "patterns", id, by),
            PatternCmd::Retired => run_retired(&store, "patterns"),
            PatternCmd::Health => crystallizer::list_pattern_health(&store),
        };
    }

    match cmd {
        PatternCmd::List => {
            let patterns = store.all_patterns()?;
            print_json(&patterns)
        }
        PatternCmd::Add { name, intent, body, uses, tags } => {
            let id = store.insert_pattern(&model::Pattern {
                id: None,
                name: name.clone(),
                intent: intent.clone(),
                body,
                uses,
                tags,
                approved_at: chrono::Utc::now(),
                use_count: 0,
                reverted_count: 0,
                survival_rate: 1.0,
            })?;
            print_json(&json!({"ok": true, "action": "add", "id": id, "name": name, "intent": intent}))
        }
        PatternCmd::Remove { id } => {
            store.delete_pattern(id)?;
            print_json(&json!({"ok": true, "action": "remove", "id": id}))
        }
        PatternCmd::Revert { id } => {
            store.pattern_reverted(id)?;
            print_json(&json!({"ok": true, "action": "revert", "id": id}))
        }
        PatternCmd::Supersede { id, by } => {
            let n = store.supersede("patterns", id, by)?;
            print_json(&json!({"ok": n > 0, "action": "supersede", "id": id, "by": by}))
        }
        PatternCmd::Retired => print_json(&store.superseded_rows("patterns")?),
        PatternCmd::Health => {
            let rows = store.pattern_health_rows()?;
            let health: Vec<_> = rows
                .into_iter()
                .map(|(id, name, use_count, reverted_count, survival_rate)| {
                    json!({
                        "id": id,
                        "name": name,
                        "use_count": use_count,
                        "reverted_count": reverted_count,
                        "survival_rate": survival_rate
                    })
                })
                .collect();
            print_json(&json!({"ok": true, "action": "health", "patterns": health}))
        }
    }
}

fn run_anti_pattern(cmd: AntiPatternCmd, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    if format == OutputFormat::Text {
        return match cmd {
            AntiPatternCmd::List => crystallizer::list_anti_patterns(&store),
            AntiPatternCmd::Add { description, wrong, correct, tags } =>
                crystallizer::add_anti_pattern(&store, &description, &wrong, &correct, tags),
            AntiPatternCmd::Remove { id } => crystallizer::remove_anti_pattern(&store, id),
            AntiPatternCmd::Supersede { id, by } => run_supersede(&store, "anti_patterns", id, by),
            AntiPatternCmd::Retired => run_retired(&store, "anti_patterns"),
        };
    }

    match cmd {
        AntiPatternCmd::List => {
            let anti_patterns = store.all_anti_patterns()?;
            print_json(&anti_patterns)
        }
        AntiPatternCmd::Add { description, wrong, correct, tags } => {
            let id = store.insert_anti_pattern(&model::AntiPattern {
                id: None,
                description: description.clone(),
                wrong,
                correct,
                tags,
                added_at: chrono::Utc::now(),
            })?;
            print_json(&json!({"ok": true, "action": "add", "id": id, "description": description}))
        }
        AntiPatternCmd::Remove { id } => {
            store.delete_anti_pattern(id)?;
            print_json(&json!({"ok": true, "action": "remove", "id": id}))
        }
        AntiPatternCmd::Supersede { id, by } => {
            let n = store.supersede("anti_patterns", id, by)?;
            print_json(&json!({"ok": n > 0, "action": "supersede", "id": id, "by": by}))
        }
        AntiPatternCmd::Retired => print_json(&store.superseded_rows("anti_patterns")?),
    }
}

/// Retire one entry in favour of another, and say what happened.
///
/// The message names both sides because the destructive-looking half is the one
/// that disappears from every future call, and a bare "ok" would not let anyone
/// check that the right row went.
fn run_supersede(store: &Store, table: &str, id: i64, by: i64) -> Result<()> {
    let n = store.supersede(table, id, by)?;
    if n == 0 {
        println!("[cortex] {table} #{id} was already retired, or does not exist.");
    } else {
        println!("[cortex] {table} #{id} retired — superseded by #{by}.");
        println!("         It stays in the database as history and will not be served again.");
    }
    Ok(())
}

fn run_retired(store: &Store, table: &str) -> Result<()> {
    let rows = store.superseded_rows(table)?;
    if rows.is_empty() {
        println!("[cortex] no retired {table}.");
        return Ok(());
    }
    println!("[cortex] {} retired {table}:", rows.len());
    for (id, by, label) in rows {
        let one_line: String = label.chars().take(88).collect();
        println!("  #{id:<5} superseded by #{by:<5} {one_line}");
    }
    Ok(())
}

fn run_annotate(cmd: AnnotateCmd, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    if format == OutputFormat::Text {
        return match cmd {
            AnnotateCmd::List => crystallizer::list_annotations(&store),
            AnnotateCmd::Add { topic, body, tags } =>
                crystallizer::add_annotation(&store, &topic, &body, tags),
            AnnotateCmd::Remove { id } => crystallizer::remove_annotation(&store, id),
        };
    }

    match cmd {
        AnnotateCmd::List => {
            let annotations = store.all_annotations()?;
            print_json(&annotations)
        }
        AnnotateCmd::Add { topic, body, tags } => {
            let id = store.insert_annotation(&model::Annotation {
                id: None,
                topic: topic.clone(),
                body,
                tags,
                added_at: chrono::Utc::now(),
            })?;
            print_json(&json!({"ok": true, "action": "add", "id": id, "topic": topic}))
        }
        AnnotateCmd::Remove { id } => {
            store.delete_annotation(id)?;
            print_json(&json!({"ok": true, "action": "remove", "id": id}))
        }
    }
}

fn run_outcome(args: OutcomeArgs, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let id = store.log_outcome(
        &args.session_id,
        &args.outcome_type,
        args.error_text.as_deref(),
        args.diff_symbols.as_deref(),
    )?;

    let evidence_report = if args.auto_apply {
        Some(apply_weighted_pattern_evidence(
            &store,
            &args.session_id,
            false,
        )?)
    } else {
        None
    };

    if format == OutputFormat::Json {
        let mut payload = json!({
            "ok": true,
            "action": "outcome_logged",
            "id": id,
            "session_id": args.session_id,
            "outcome_type": args.outcome_type,
            "auto_apply": args.auto_apply,
        });
        if let Some(report) = &evidence_report {
            payload["evidence"] = serde_json::to_value(report)?;
        }
        print_json(&payload)?;
    } else {
        println!(
            "outcome logged: id={} session={} type={}",
            id, args.session_id, args.outcome_type
        );
        if let Some(report) = &evidence_report {
            println!(
                "auto evidence: pending={} applied={} updated_patterns={} use_delta={} reverted_delta={}",
                report.pending_outcomes,
                report.applied_outcomes,
                report.updated_patterns,
                report.use_delta_total,
                report.reverted_delta_total
            );
        } else {
            println!("auto evidence: disabled");
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct OutcomeEvidenceReport {
    session_id: String,
    dry_run: bool,
    already_applied: bool,
    pending_outcomes: usize,
    applied_outcomes: usize,
    retrieved_patterns: usize,
    positive_outcomes: i64,
    negative_outcomes: i64,
    updated_patterns: usize,
    use_delta_total: i64,
    reverted_delta_total: i64,
    applied: bool,
}

fn classify_outcome_signal(outcome_type: &str) -> (i64, i64) {
    let lower = outcome_type.to_lowercase();
    let positive = ["pass", "success", "clean", "ok"]
        .iter()
        .any(|k| lower.contains(k));
    let negative = ["fail", "error", "panic", "regression", "timeout"]
        .iter()
        .any(|k| lower.contains(k));

    (if positive { 1 } else { 0 }, if negative { 1 } else { 0 })
}

fn apply_weighted_pattern_evidence(
    store: &Store,
    session_id: &str,
    dry_run: bool,
) -> Result<OutcomeEvidenceReport> {
    let mut report = OutcomeEvidenceReport {
        session_id: session_id.to_string(),
        dry_run,
        already_applied: false,
        pending_outcomes: 0,
        applied_outcomes: 0,
        retrieved_patterns: 0,
        positive_outcomes: 0,
        negative_outcomes: 0,
        updated_patterns: 0,
        use_delta_total: 0,
        reverted_delta_total: 0,
        applied: false,
    };

    let pending_outcomes = store.pending_outcomes_for_session(session_id)?;
    report.pending_outcomes = pending_outcomes.len();

    if pending_outcomes.is_empty() {
        report.already_applied = true;
        return Ok(report);
    }

    for (_, kind) in &pending_outcomes {
        let (pos, neg) = classify_outcome_signal(kind);
        report.positive_outcomes += pos;
        report.negative_outcomes += neg;
    }

    let pattern_hits: Vec<(i64, i64)> = {
        let mut stmt = store.conn().prepare(
            "SELECT entry_id, COUNT(*)
             FROM session_retrieval_log
             WHERE session_id = ?1
               AND entry_table = 'patterns'
             GROUP BY entry_id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut parsed: Vec<(i64, i64)> = Vec::new();
        for row in rows {
            let (entry_id, hits) = row?;
            if let Ok(pattern_id) = entry_id.parse::<i64>() {
                parsed.push((pattern_id, hits));
            }
        }
        parsed
    };

    report.retrieved_patterns = pattern_hits.len();
    let has_signal = report.positive_outcomes > 0 || report.negative_outcomes > 0;

    if has_signal && !pattern_hits.is_empty() {
        for (pattern_id, hit_count) in pattern_hits {
            let use_delta = if report.positive_outcomes > 0 {
                ((hit_count as f64) * (0.5 + 0.25 * report.positive_outcomes as f64)).ceil() as i64
            } else {
                0
            };
            let reverted_delta = if report.negative_outcomes > 0 {
                ((hit_count as f64) * (0.5 + 0.35 * report.negative_outcomes as f64)).ceil() as i64
            } else {
                0
            };

            if use_delta == 0 && reverted_delta == 0 {
                continue;
            }

            report.use_delta_total += use_delta;
            report.reverted_delta_total += reverted_delta;

            if !dry_run {
                let touched = store.conn().execute(
                    "UPDATE patterns
                     SET use_count = use_count + ?1,
                         reverted_count = reverted_count + ?2
                     WHERE id = ?3",
                    params![use_delta, reverted_delta, pattern_id],
                )?;
                report.updated_patterns += touched as usize;
            }
        }
    }

    if !dry_run {
        let pending_ids: Vec<i64> = pending_outcomes.iter().map(|(id, _)| *id).collect();
        report.applied_outcomes = store.mark_outcomes_applied(session_id, &pending_ids)?;

        if report.updated_patterns > 0 {
            store.conn().execute(
                "UPDATE patterns
                 SET survival_rate = CASE
                     WHEN (use_count + reverted_count) = 0 THEN 1.0
                     ELSE CAST(use_count AS REAL) / CAST((use_count + reverted_count) AS REAL)
                 END",
                [],
            )?;
        }

        report.applied = report.updated_patterns > 0 || report.applied_outcomes > 0;
    }

    Ok(report)
}

fn run_outcome_apply(args: OutcomeApplyArgs, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let report = apply_weighted_pattern_evidence(&store, &args.session_id, args.dry_run)?;

    if format == OutputFormat::Json {
        print_json(&report)?;
        return Ok(());
    }

    println!("outcome evidence");
    println!("  session:           {}", report.session_id);
    println!("  dry run:           {}", report.dry_run);
    println!("  already applied:   {}", report.already_applied);
    println!("  pending outcomes:  {}", report.pending_outcomes);
    println!("  applied outcomes:  {}", report.applied_outcomes);
    println!("  retrieved patterns:{}", report.retrieved_patterns);
    println!("  positive outcomes: {}", report.positive_outcomes);
    println!("  negative outcomes: {}", report.negative_outcomes);
    println!("  updated patterns:  {}", report.updated_patterns);
    println!("  use delta total:   {}", report.use_delta_total);
    println!("  revert delta total:{}", report.reverted_delta_total);
    println!("  applied:           {}", report.applied);
    Ok(())
}

#[derive(Serialize)]
struct BenchmarkReport {
    target: String,
    samples_requested: usize,
    samples_used: usize,
    p50_ms: f64,
    p95_ms: f64,
    coverage: f64,
    detail: Value,
}

#[derive(Deserialize)]
struct DependencyBenchmarkCase {
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct DependencyBenchmarkCaseWrapper {
    cases: Vec<DependencyBenchmarkCase>,
}

fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((values.len() - 1) as f64 * fraction).round() as usize;
    values[index.min(values.len() - 1)]
}

fn run_benchmark(args: BenchmarkArgs, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let report = match args.target {
        BenchmarkTarget::Syntax => run_syntax_benchmark(&store, args.samples)?,
        BenchmarkTarget::Dependency => {
            run_dependency_benchmark(&store, args.samples, args.depth, args.corpus.as_deref())?
        }
    };

    if format == OutputFormat::Json {
        print_json(&report)?;
        return Ok(());
    }

    println!("benchmark {}", report.target);
    println!("  samples:      {}/{}", report.samples_used, report.samples_requested);
    println!("  p50 latency:  {:.3} ms", report.p50_ms);
    println!("  p95 latency:  {:.3} ms", report.p95_ms);
    println!("  coverage:     {:.1}%", report.coverage * 100.0);
    if report.target == "dependency" {
        if let Some(precision) = report.detail.get("precision").and_then(Value::as_f64) {
            println!("  precision:    {:.1}%", precision * 100.0);
        }
        if let Some(cases) = report.detail.get("corpus_cases").and_then(Value::as_u64) {
            println!("  corpus cases: {}", cases);
        }
    }
    Ok(())
}

fn run_syntax_benchmark(store: &Store, requested_samples: usize) -> Result<BenchmarkReport> {
    let mut names: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT DISTINCT symbol_name FROM symbol_catalog ORDER BY last_seen_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![requested_samples as i64], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if names.is_empty() {
        let mut seen = HashSet::new();
        for unit in store.all_units()?.into_iter() {
            if seen.insert(unit.name.clone()) {
                names.push(unit.name);
            }
            if names.len() >= requested_samples {
                break;
            }
        }
    }

    let mut latencies: Vec<f64> = Vec::new();
    let mut rich_hits = 0usize;
    let mut lookup_stmt = store.conn().prepare(
           "SELECT signature, methods_json, return_type
         FROM symbol_catalog
            WHERE symbol_name = ?1
         LIMIT 1",
    )?;

    for name in &names {
        let start = Instant::now();
        let mut rows = lookup_stmt.query(params![name])?;
        let mut has_shape = false;
        if let Some(row) = rows.next()? {
            let signature: Option<String> = row.get(0)?;
            let methods: Option<String> = row.get(1)?;
            let return_type: Option<String> = row.get(2)?;
            has_shape = signature.as_deref().unwrap_or("").trim().len() > 3
                || methods.as_deref().unwrap_or("").trim().len() > 3
                || return_type.as_deref().unwrap_or("").trim().len() > 1;
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        latencies.push(elapsed_ms);
        if has_shape {
            rich_hits += 1;
        }
    }

    let samples_used = latencies.len();
    let coverage = if samples_used == 0 {
        0.0
    } else {
        rich_hits as f64 / samples_used as f64
    };

    let mut sorted = latencies.clone();
    let p50 = percentile(&mut sorted, 0.50);
    let mut sorted = latencies;
    let p95 = percentile(&mut sorted, 0.95);

    Ok(BenchmarkReport {
        target: "syntax".to_string(),
        samples_requested: requested_samples,
        samples_used,
        p50_ms: p50,
        p95_ms: p95,
        coverage,
        detail: json!({
            "rich_shape_hits": rich_hits,
        }),
    })
}

fn load_dependency_corpus(path: &Path) -> Result<Vec<DependencyBenchmarkCase>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read dependency benchmark corpus at {}", path.display()))?;

    if let Ok(cases) = serde_json::from_str::<Vec<DependencyBenchmarkCase>>(&content) {
        return Ok(cases);
    }

    let wrapper: DependencyBenchmarkCaseWrapper = serde_json::from_str(&content)
        .with_context(|| format!("invalid dependency benchmark corpus JSON at {}", path.display()))?;
    Ok(wrapper.cases)
}

fn dependency_path_exists(store: &Store, from: &str, to: &str, depth: u8) -> Result<bool> {
    if from == to {
        return Ok(true);
    }

    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, u8)> = VecDeque::new();
    seen.insert(from.to_string());
    queue.push_back((from.to_string(), 0));

    while let Some((current, level)) = queue.pop_front() {
        if level >= depth {
            continue;
        }

        for (edge, _) in graph::neighbors(store.conn(), &current)? {
            if edge.to_id == to {
                return Ok(true);
            }
            if seen.insert(edge.to_id.clone()) {
                queue.push_back((edge.to_id, level + 1));
            }
        }
    }

    Ok(false)
}

fn run_dependency_benchmark(
    store: &Store,
    requested_samples: usize,
    depth: u8,
    corpus: Option<&Path>,
) -> Result<BenchmarkReport> {
    let node_ids: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT id FROM graph_nodes ORDER BY id LIMIT ?1")?;
        let rows = stmt.query_map(params![requested_samples as i64], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut latencies: Vec<f64> = Vec::new();
    let mut with_neighbors = 0usize;
    for node_id in &node_ids {
        let start = Instant::now();
        let (edges, _) = graph::subgraph(store.conn(), node_id, depth)?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        latencies.push(elapsed_ms);
        if !edges.is_empty() {
            with_neighbors += 1;
        }
    }

    let samples_used = latencies.len();
    let coverage = if samples_used == 0 {
        0.0
    } else {
        with_neighbors as f64 / samples_used as f64
    };

    let mut precision = None;
    let mut corpus_cases = 0usize;
    if let Some(path) = corpus {
        let cases = load_dependency_corpus(path)?;
        corpus_cases = cases.len();
        if !cases.is_empty() {
            let mut hits = 0usize;
            for case in &cases {
                if dependency_path_exists(store, &case.from, &case.to, depth)? {
                    hits += 1;
                }
            }
            precision = Some(hits as f64 / cases.len() as f64);
        }
    }

    let mut sorted = latencies.clone();
    let p50 = percentile(&mut sorted, 0.50);
    let mut sorted = latencies;
    let p95 = percentile(&mut sorted, 0.95);

    Ok(BenchmarkReport {
        target: "dependency".to_string(),
        samples_requested: requested_samples,
        samples_used,
        p50_ms: p50,
        p95_ms: p95,
        coverage,
        detail: json!({
            "depth": depth,
            "graph_neighbor_hits": with_neighbors,
            "corpus_cases": corpus_cases,
            "precision": precision,
        }),
    })
}

fn run_status(db_path: &Path, full: bool, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;

    if format == OutputFormat::Json {
        let report = build_status_json(&store, db_path, full)?;
        print_json(&report)?;
        return Ok(());
    }

    let report = build_status_report(&store, db_path, full)?;
    print!("{}", report);
    Ok(())
}

fn build_status_json(store: &Store, db_path: &Path, full: bool) -> Result<serde_json::Value> {
    let unit_count = store.unit_count()?;
    let patterns = store.all_patterns()?;
    let anti_patterns = store.all_anti_patterns()?;
    let annotations = store.all_annotations()?;
    let observations = store.all_observations()?;
    let hot = store.hot_tools(5)?;
    let (gap_unique, gap_seen, gap_recurrent) = store.query_gap_summary()?;
    let cache = cache::cache_stats(store.conn()).ok();
    let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let mut root = json!({
        "db": db_path.display().to_string(),
        "db_bytes": db_size,
        "indexed_units": unit_count,
        "patterns": patterns.len(),
        "anti_patterns": anti_patterns.len(),
        "annotations": annotations.len(),
        "pending_review": observations.len(),
        "hot_tools": hot,
        "query_gaps": {
            "unique": gap_unique,
            "seen": gap_seen,
            "recurrent": gap_recurrent,
        }
    });

    if let Some(c) = cache {
        root["cache"] = json!({
            "entries": c.entries,
            "total_hits": c.total_hits,
            "content_blobs": c.content_blobs,
            "approx_bytes": c.approx_bytes,
        });
    }

    if full {
        let (nodes, edges, inferred, manual) = store.graph_counts()?;
        let scratchpads = store.scratchpad_count()?;
        let recent_hot = store.hot_tools_recent(500, 5)?;
        let health = store.pattern_health_rows()?;
        let top_query_gaps = store.top_query_gaps(8)?;
        root["full"] = json!({
            "graph": {
                "nodes": nodes,
                "edges": edges,
                "inferred": inferred,
                "manual": manual,
            },
            "scratchpads": scratchpads,
            "recent_hot_tools": recent_hot,
            "pattern_health": health,
            "query_gaps": top_query_gaps
        });
    }

    Ok(root)
}

fn run_doctor(cmd: DoctorCmd, db_path: &Path, format: OutputFormat) -> Result<()> {
    match cmd {
        DoctorCmd::Workflow(args) => run_doctor_workflow(args, db_path, format),
    }
}

fn run_meta(cmd: MetaCmd, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));
    let rejected_log = repo_root.join(".cortex").join("rejected-proposals.jsonl");

    match cmd {
        MetaCmd::Report => {
            let report = meta::build_meta_report(&store, &rejected_log)?;
            if format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("=== META ANALYSIS REPORT ===");
                println!("Total proposals: {}", report.total_proposals);
                println!("Approved: {} | Rejected: {} | Pending: {} | Trial: {}",
                    report.approved, report.rejected, report.pending, report.trial);
                println!("Approval rate: {:.1}%", report.approval_rate * 100.0);
                println!("Gate rejection rate: {:.1}%", report.gate_rejection_rate * 100.0);
                if !report.top_rejected_gates.is_empty() {
                    println!("\nTop gate rejections:");
                    for (g, c) in &report.top_rejected_gates { println!("  {g}: {c}"); }
                }
                println!("\nFidelity: avg={:.0}%, low-fidelity sessions={}",
                    report.avg_fidelity_score * 100.0, report.low_fidelity_sessions);
                if let Some(ref step) = report.most_missed_step {
                    println!("  Most missed step: '{step}'");
                }
                println!("Persistent unresolved gaps: {}", report.persistent_gaps);
                if !report.threshold_alerts.is_empty() {
                    println!("\nThreshold alerts:");
                    for a in &report.threshold_alerts { println!("  ! {a}"); }
                }
                println!("=============================");
            }
        }
        MetaCmd::Propose => {
            let report = meta::build_meta_report(&store, &rejected_log)?;
            let staged = meta::stage_meta_proposals(&store, &report)?;
            println!("Staged {} meta-proposal(s) for review.", staged);
        }
        MetaCmd::Apply { id } => {
            let (applied, diff) = meta::apply_meta_proposal(&store, id, repo_root, false)?;
            if applied {
                println!("Applied proposal {}:\n{}", id, diff);
            } else {
                println!("Could not apply: {}", diff);
            }
        }
        MetaCmd::DryRun { id } => {
            let (_applied, diff) = meta::apply_meta_proposal(&store, id, repo_root, true)?;
            println!("[dry-run] Proposal {} would change:\n{}", id, diff);
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct DoctorCheck {
    step: String,
    pass: bool,
    detail: String,
}

fn run_doctor_workflow(args: DoctorWorkflowArgs, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let mut checks: Vec<DoctorCheck> = Vec::new();

    let unit_count = store.unit_count()?;
    checks.push(DoctorCheck {
        step: "index_present".to_string(),
        pass: unit_count > 0,
        detail: format!("indexed_units={}", unit_count),
    });

    let delta_opts = git::DeltaOptions {
        include: args.delta_include,
        exclude: args.delta_exclude,
        max_files: args.delta_max_files,
        max_patch_lines: 12,
    };
    let deltas = git::head_deltas_with_options(&args.repo, &delta_opts)?;
    checks.push(DoctorCheck {
        step: "delta_query".to_string(),
        pass: true,
        detail: format!("delta_files_returned={}", deltas.len()),
    });

    let packet = planner::build_context_packet(
        &store,
        "workflow doctor check",
        800,
        Some(&args.repo),
        Some(&delta_opts),
    )?;
    checks.push(DoctorCheck {
        step: "context_packet".to_string(),
        pass: true,
        detail: format!("estimated_tokens={} relevant_units={} deltas={}", packet.estimated_tokens, packet.relevant_units.len(), packet.deltas.len()),
    });

    let (gap_unique, gap_seen, gap_recurrent) = store.query_gap_summary()?;
    checks.push(DoctorCheck {
        step: "query_gap_telemetry".to_string(),
        pass: true,
        detail: format!(
            "query_gaps_unique={} seen={} recurrent={}",
            gap_unique, gap_seen, gap_recurrent
        ),
    });

    let full_status = build_status_report(&store, db_path, true)?;
    checks.push(DoctorCheck {
        step: "status_full_render".to_string(),
        pass: full_status.contains("full details"),
        detail: "status --full report generated".to_string(),
    });

    if args.mutate_pattern {
        let pattern = model::Pattern {
            id: None,
            name: format!("doctor sentinel {}", chrono::Utc::now().timestamp()),
            intent: "Doctor mutation check".to_string(),
            body: "if grounded_transition { Action::PlaySound(..) }".to_string(),
            uses: vec!["Action".to_string(), "Condition".to_string()],
            tags: vec!["doctor".to_string(), "workflow".to_string()],
            approved_at: chrono::Utc::now(),
            use_count: 0,
            reverted_count: 0,
            survival_rate: 1.0,
        };

        let id = store.insert_pattern(&pattern)?;
        store.pattern_reverted(id)?;
        store.delete_pattern(id)?;
        checks.push(DoctorCheck {
            step: "pattern_roundtrip".to_string(),
            pass: true,
            detail: format!("added_reverted_removed_pattern_id={}", id),
        });
    }

    let pass = checks.iter().all(|c| c.pass);
    if format == OutputFormat::Json {
        print_json(&json!({
            "ok": pass,
            "checks": checks
        }))?;
    } else {
        println!("workflow doctor:\n");
        for c in &checks {
            let marker = if c.pass { "✓" } else { "✗" };
            println!("  {} {:22} {}", marker, c.step, c.detail);
        }
    }

    if !pass {
        anyhow::bail!("workflow doctor failed one or more checks");
    }
    Ok(())
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn build_status_report(store: &Store, db_path: &Path, full: bool) -> Result<String> {

    let unit_count = store.unit_count()?;
    let patterns = store.all_patterns()?;
    let anti_patterns = store.all_anti_patterns()?;
    let annotations = store.all_annotations()?;
    let observations = store.all_observations()?;
    let hot = store.hot_tools(5)?;
    let (gap_unique, gap_seen, gap_recurrent) = store.query_gap_summary()?;
    let cache = cache::cache_stats(store.conn()).ok();

    // Rough DB file size
    let db_size = std::fs::metadata(db_path)
        .map(|m| format_bytes(m.len()))
        .unwrap_or_else(|_| "unknown".to_string());

    let mut out = String::new();
    out.push_str("cortex status\n\n");
    out.push_str(&format!("  db:               {}\n", db_path.display()));
    out.push_str(&format!("  db size:          {}\n", db_size));
    out.push_str(&format!("  indexed units:    {}\n", unit_count));
    out.push_str(&format!("  patterns:         {}\n", patterns.len()));
    out.push_str(&format!("  anti-patterns:    {}\n", anti_patterns.len()));
    out.push_str(&format!("  annotations:      {}\n", annotations.len()));
    out.push_str(&format!("  pending review:   {}\n", observations.len()));
    out.push_str(&format!("  query gaps:       {} unique / {} seen / {} recurrent\n", gap_unique, gap_seen, gap_recurrent));

    if let Some(c) = cache {
        out.push('\n');
        out.push_str(&format!(
            "  response cache:   {} entries ({} cache hits total)\n",
            c.entries, c.total_hits
        ));
        out.push_str(&format!(
            "  content store:    {} blobs (~{} compressed)\n",
            c.content_blobs,
            format_bytes(c.approx_bytes as u64)
        ));
    }

    if !hot.is_empty() {
        out.push_str("\n  most-called tools:\n");
        for (tool, count) in &hot {
            out.push_str(&format!("    {:25} {}x\n", tool, count));
        }
    }

    if !observations.is_empty() {
        out.push_str(&format!(
            "\n  {} observation(s) waiting — run `cortex review`\n",
            observations.len()
        ));
    }

    if full {
        let (nodes, edges, inferred, manual) = store.graph_counts()?;
        let scratchpads = store.scratchpad_count()?;
        let recent_hot = store.hot_tools_recent(500, 5)?;
        let health = store.pattern_health_rows()?;
        let top_query_gaps = store.top_query_gaps(8)?;

        out.push_str("\nfull details\n\n");
        out.push_str("  graph:\n");
        out.push_str(&format!("    nodes:           {}\n", nodes));
        out.push_str(&format!(
            "    edges:           {}  ({} inferred, {} manual)\n",
            edges, inferred, manual
        ));
        out.push_str(&format!("\n  scratchpads:       {} active\n", scratchpads));

        if !recent_hot.is_empty() {
            out.push_str("\n  top tools (last 500 calls):\n");
            for (tool, count) in &recent_hot {
                out.push_str(&format!("    {:20} {}x\n", tool, count));
            }
        }

        if !health.is_empty() {
            let low_count = health.iter().filter(|(_, _, _, _, s)| *s < 0.4).count();

            out.push_str("\n  pattern health:\n");
            for (_id, name, _uses, _reverted, survival) in &health {
                let (marker, tier) = if *survival < 0.4 {
                    ("⚠", "critical")
                } else if *survival < 0.8 {
                    ("!", "watch")
                } else {
                    ("✓", "healthy")
                };
                out.push_str(&format!(
                    "    {} {} ({:.0}%) [{}]\n",
                    marker,
                    name,
                    survival * 100.0,
                    tier
                ));
            }

            if low_count > 0 {
                out.push_str(&format!(
                    "\n  {} pattern(s) below 40% survival — run `cortex pattern health` and revise risky patterns.\n",
                    low_count
                ));
            }
        }

        if !top_query_gaps.is_empty() {
            out.push_str("\n  query gap hotspots:\n");
            for (tool, query, count, _last_seen, reason) in &top_query_gaps {
                out.push_str(&format!("    {:18} {:4}x  {}\n", tool, count, query));
                if let Some(r) = reason {
                    if !r.trim().is_empty() {
                        out.push_str(&format!("    {:18}      reason: {}\n", "", r));
                    }
                }
            }
        }
    }

    Ok(out)
}

fn run_prune_index(keep: Vec<String>, apply: bool, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let keep: Vec<String> = keep.iter().map(|k| k.replace('\\', "/")).collect();

    if keep.is_empty() {
        anyhow::bail!(
            "refusing to run with no --keep roots: that would delete the entire index.\n\
             help: pass every configured source, e.g. --keep quartz/src --keep path_forge/src"
        );
    }

    println!("cortex prune-index");
    println!("  keeping {} source root(s): {}", keep.len(), keep.join(", "));
    println!("\n  units by source:");

    let mut orphans = 0i64;
    for (source, count) in store.units_by_source()? {
        let label = source.clone().unwrap_or_else(|| "<unstamped>".to_string());
        let kept = source.as_ref().map(|s| keep.contains(s)).unwrap_or(false);
        if !kept {
            orphans += count;
        }
        println!("    {:<48} {:>5}  {}", label, count, if kept { "keep" } else { "PRUNE" });
    }

    if orphans == 0 {
        println!("\n  nothing to prune.");
        return Ok(());
    }

    if !apply {
        println!("\n  {orphans} unit(s) would be deleted. Re-run with --apply to delete.");
        println!("  note: run a full reindex first, or units from configured sources that");
        println!("        predate provenance stamping will be counted as orphans.");
        return Ok(());
    }

    let deleted = store.prune_orphan_units(&keep)?;
    println!("\n  deleted {deleted} orphaned unit(s) and their members, nodes and edges.");
    Ok(())
}

fn run_prune(keep_calls: usize, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;

    let pruned_calls = cache::prune_call_log(store.conn(), keep_calls)?;
    println!("  pruned {} call log entries (keeping {})", pruned_calls, keep_calls);

    cache::vacuum(store.conn())?;
    println!("  vacuumed db");

    let db_size = std::fs::metadata(db_path)
        .map(|m| format_bytes(m.len()))
        .unwrap_or_else(|_| "unknown".to_string());
    println!("  db size now: {}", db_size);

    Ok(())
}

fn format_bytes(b: u64) -> String {
    if b < 1024 { format!("{}B", b) }
    else if b < 1024 * 1024 { format!("{:.1}KB", b as f64 / 1024.0) }
    else { format!("{:.2}MB", b as f64 / (1024.0 * 1024.0)) }
}

fn run_recall(topic: &str, db_path: &Path, format: OutputFormat) -> Result<()> {
    let store = Store::open(db_path)?;
    let topic_lower = topic.to_lowercase();
    let terms = crate::recall_match::recall_terms(topic);

    let units = store.all_units()?;
    let matched_units: Vec<_> = units
        .iter()
        .filter(|u| {
            crate::recall_match::recall_score(&[&u.name, &u.compressed], &topic_lower, &terms) > 0
        })
        .take(6)
        .collect();

    let patterns = store.all_patterns()?;
    let matched_patterns: Vec<_> = patterns
        .iter()
        .filter(|p| {
            let uses = p.uses.join(" ");
            let tags = p.tags.join(" ");
            crate::recall_match::recall_score(
                &[&p.name, &p.intent, &p.body, &uses, &tags],
                &topic_lower,
                &terms,
            ) > 0
        })
        .collect();

    let aps = store.all_anti_patterns()?;
    let matched_aps: Vec<_> = aps
        .iter()
        .filter(|ap| {
            let tags = ap.tags.join(" ");
            crate::recall_match::recall_score(
                &[&ap.description, &ap.wrong, &ap.correct, &tags],
                &topic_lower,
                &terms,
            ) > 0
        })
        .collect();

    let annotations = store.all_annotations()?;
    let matched_annotations: Vec<_> = annotations
        .iter()
        .filter(|a| {
            a.topic.to_lowercase().contains(&topic_lower)
                || a.body.to_lowercase().contains(&topic_lower)
                || a.tags.iter().any(|t| t.to_lowercase().contains(&topic_lower))
        })
        .collect();

    if format == OutputFormat::Json {
        print_json(&json!({
            "topic": topic,
            "units": matched_units.iter().map(|u| json!({
                "id": u.id,
                "name": u.name,
                "kind": u.kind,
                "summary": u.compressed.chars().take(200).collect::<String>(),
            })).collect::<Vec<_>>(),
            "patterns": matched_patterns.iter().map(|p| json!({
                "id": p.id,
                "name": p.name,
                "intent": p.intent,
                "body": p.body,
                "survival_rate": p.survival_rate,
            })).collect::<Vec<_>>(),
            "anti_patterns": matched_aps.iter().map(|ap| json!({
                "id": ap.id,
                "description": ap.description,
                "wrong": ap.wrong,
                "correct": ap.correct,
            })).collect::<Vec<_>>(),
            "annotations": matched_annotations.iter().map(|a| json!({
                "id": a.id,
                "topic": a.topic,
                "body": a.body,
                "tags": a.tags,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(());
    }

    println!("# Recall: `{topic}`\n");

    if !matched_units.is_empty() {
        println!("## Indexed Units");
        for u in &matched_units {
            println!("### {} ({})", u.name, u.kind);
            let preview: String = u.compressed.chars().take(300).collect();
            println!("{}", preview);
            println!();
        }
    }

    if !matched_patterns.is_empty() {
        println!("## Patterns");
        for p in &matched_patterns {
            println!("### {} — {}", p.name, p.intent);
            println!("{}", p.body);
            println!("  survival: {:.0}%", p.survival_rate * 100.0);
            println!();
        }
    }

    if !matched_aps.is_empty() {
        println!("## Anti-Patterns");
        for ap in &matched_aps {
            println!("### {}", ap.description);
            println!("  WRONG:   {}", ap.wrong);
            println!("  CORRECT: {}", ap.correct);
            println!();
        }
    }

    if !matched_annotations.is_empty() {
        println!("## Annotations");
        for a in &matched_annotations {
            println!("### {}", a.topic);
            println!("{}", a.body);
            println!();
        }
    }

    let total = matched_units.len() + matched_patterns.len() + matched_aps.len() + matched_annotations.len();
    if total == 0 {
        println!("No results found for `{topic}`.");
    }

    Ok(())
}

fn run_git_review(base: &str, repo: Option<&Path>, db_path: &Path) -> Result<()> {
    let repo_root = repo.unwrap_or_else(|| Path::new("."));
    let store = Store::open(db_path)?;

    // Get changed files from git diff
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", base, "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("git diff failed — is this a git repo?")?;

    let changed_files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    if changed_files.is_empty() {
        println!("git-review: no changed files between {} and HEAD", base);
        return Ok(());
    }

    // Read content of changed .rs files to extract API names mentioned
    let mut mentioned_apis: Vec<String> = Vec::new();
    for f in &changed_files {
        if !f.ends_with(".rs") { continue; }
        let path = repo_root.join(f);
        if let Ok(src) = std::fs::read_to_string(&path) {
            // Collect capitalised identifiers (likely API types/enums)
            for word in src.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if word.len() >= 4 && word.chars().next().map_or(false, |c| c.is_uppercase()) {
                    mentioned_apis.push(word.to_string());
                }
            }
        }
    }
    mentioned_apis.sort();
    mentioned_apis.dedup();

    // Match patterns whose `uses` overlap with mentioned APIs
    let patterns = store.all_patterns()?;
    let anti_patterns = store.all_anti_patterns()?;

    let relevant_patterns: Vec<_> = patterns.iter().filter(|p| {
        p.uses.iter().any(|u| mentioned_apis.iter().any(|m| m == u))
    }).collect();

    let relevant_aps: Vec<_> = anti_patterns.iter().filter(|ap| {
        ap.tags.iter().any(|t| mentioned_apis.iter().any(|m| m.to_lowercase().contains(&t.to_lowercase())))
        || mentioned_apis.iter().any(|m| ap.wrong.contains(m.as_str()) || ap.description.contains(m.as_str()))
    }).collect();

    println!("git-review: {} changed files ({}..HEAD)\n", changed_files.len(), base);
    println!("Changed files:");
    for f in &changed_files { println!("  {}", f); }
    println!();

    if relevant_patterns.is_empty() && relevant_aps.is_empty() {
        println!("No patterns or anti-patterns matched the changed files.");
        return Ok(());
    }

    if !relevant_patterns.is_empty() {
        println!("## Relevant Patterns ({} matched)\n", relevant_patterns.len());
        for p in &relevant_patterns {
            println!("  [{}] {} (survival {:.0}%)", p.id.unwrap_or(0), p.name, p.survival_rate * 100.0);
            println!("      {}", p.intent);
            println!("      uses: {}", p.uses.join(", "));
            println!();
        }
    }

    if !relevant_aps.is_empty() {
        println!("## Relevant Anti-Patterns ({} matched)\n", relevant_aps.len());
        for ap in &relevant_aps {
            println!("  [{}] {}", ap.id.unwrap_or(0), ap.description);
            println!("      WRONG:   {}", ap.wrong);
            println!("      CORRECT: {}", ap.correct);
            println!();
        }
    }

    println!("Run `cortex pattern revert <id>` to mark a pattern as not used in this diff.");
    Ok(())
}

// ── ADR handler ────────────────────────────────────────────────────────────────

fn run_adr(cmd: AdrCmd, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    match cmd {
        AdrCmd::New { title, context, decision, reasoning, alternatives, consequences, tags } => {
            let concept_tags: Vec<String> = tags.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let number = store.next_adr_number()?;
            let a = model::Adr {
                id: None,
                adr_number: number,
                title: title.clone(),
                status: "accepted".into(),
                context,
                decision,
                reasoning,
                alternatives,
                consequences,
                concept_tags,
                superseded_by: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            let id = store.insert_adr(&a)?;
            println!("ADR-{:03}: {} (id={})", number, title, id);
        }
        AdrCmd::List => {
            let adrs = store.all_adrs()?;
            if adrs.is_empty() {
                println!("No ADRs recorded yet.");
            }
            for a in adrs {
                println!("ADR-{:03} [{}] {}", a.adr_number, a.status, a.title);
            }
        }
        AdrCmd::Show { number } => {
            match store.get_adr(number)? {
                None => println!("ADR-{:03} not found.", number),
                Some(a) => {
                    println!("{}", adr::format_for_context(&a));
                    println!("Reasoning: {}", a.reasoning);
                    if !a.alternatives.is_empty() {
                        println!("Alternatives considered: {}", a.alternatives);
                    }
                    if !a.consequences.is_empty() {
                        println!("Consequences: {}", a.consequences);
                    }
                    if !a.concept_tags.is_empty() {
                        println!("Tags: {}", a.concept_tags.join(", "));
                    }
                }
            }
        }
        AdrCmd::Deprecate { number, superseded_by } => {
            match store.get_adr(number)? {
                None => println!("ADR-{:03} not found.", number),
                Some(a) => {
                    let id = a.id.unwrap();
                    let status = if superseded_by.is_some() { "superseded" } else { "deprecated" };
                    store.update_adr_status(id, status, superseded_by)?;
                    println!("ADR-{:03} marked as {}.", number, status);
                }
            }
        }
    }
    Ok(())
}

// ── Consolidate handler ────────────────────────────────────────────────────────

fn run_consolidate(threshold: f32, report: bool, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let candidates = consolidator::find_candidates(&store, threshold)?;

    if candidates.is_empty() {
        println!("No duplicate pattern candidates found at threshold {:.0}%.", threshold * 100.0);
        return Ok(());
    }

    println!(
        "{} candidate pair(s) above {:.0}% similarity:\n",
        candidates.len(),
        threshold * 100.0
    );
    for (keep_id, discard_id, score, keep_name, discard_name) in &candidates {
        println!(
            "  [{keep_id}] {keep_name}  <=>  [{discard_id}] {discard_name}  ({:.1}%)",
            score * 100.0
        );
    }

    if !report {
        println!("\nMerging: keeping higher-use pattern in each pair...");
        for (keep_id, discard_id, score, keep_name, discard_name) in &candidates {
            consolidator::merge_patterns(&store, *keep_id, *discard_id, *score)?;
            println!("  Merged [{discard_id}] {discard_name} -> [{keep_id}] {keep_name}");
        }
        println!("Done. Run `cortex index` to rebuild FTS from updated patterns.");
    } else {
        println!("\nReport-only mode. No patterns were modified.");
    }
    Ok(())
}

// ── Correction handler ─────────────────────────────────────────────────────────

fn run_correction(
    attempted: &str,
    reason: &str,
    fix: &str,
    tags: &[String],
    db_path: &Path,
) -> Result<()> {
    let store = Store::open(db_path)?;
    let id = store.insert_self_correction(attempted, reason, fix, tags)?;
    println!("Correction recorded (id={id}). Use `cortex anti-pattern add` to promote if this recurs.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(name: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis();
        std::env::temp_dir().join(format!("{}_{}.db", name, ts))
    }

    #[test]
    fn phase4_pattern_revert_reflects_in_status_full() {
        let db_path = temp_db_path("cortex_phase4_status_test");
        let store = Store::open(&db_path).expect("open store");

        crate::crystallizer::add_pattern(
            &store,
            "Grounded sound",
            "Play landing sound once",
            "if grounded_transition { Action::PlaySound(..) }",
            vec!["Action".to_string(), "Condition".to_string()],
            vec!["audio".to_string()],
        )
        .expect("add pattern");

        crate::crystallizer::report_revert(&store, 1).expect("revert pattern");

        let report = build_status_report(&store, &db_path, true).expect("status report");
        assert!(report.contains("pattern health:"));
        assert!(report.contains("Grounded sound"));
        assert!(report.contains("0%"));

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn legacy_session_outcome_markers_backfill_to_per_outcome_log() {
        let db_path = temp_db_path("cortex_outcome_backfill_test");

        {
            let store = Store::open(&db_path).expect("open store");
            store
                .log_outcome("legacy_session", "build_pass", None, None)
                .expect("log outcome");
            store
                .mark_outcome_session_applied("legacy_session")
                .expect("mark legacy session applied");

            // Before reopen/migrate, the per-outcome ledger is still empty.
            let pending_before = store
                .pending_outcomes_for_session("legacy_session")
                .expect("pending before migrate");
            assert_eq!(pending_before.len(), 1);
        }

        {
            // Reopen triggers migrate(), which backfills legacy session markers.
            let store = Store::open(&db_path).expect("reopen store");
            let pending_after = store
                .pending_outcomes_for_session("legacy_session")
                .expect("pending after migrate");
            assert!(
                pending_after.is_empty(),
                "legacy session marker should be backfilled to per-outcome ledger"
            );
        }

        let _ = std::fs::remove_file(&db_path);
    }
}

// ── Phase 1: CLI handler functions ────────────────────────────────────────────

fn run_cluster_sessions(threshold: f32, output: Option<&Path>, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));
    let mined_tasks_dir = repo_root.join(".cortex").join("mined-tasks");

    let snapshots = miner::load_snapshots(&mined_tasks_dir)?;
    if snapshots.is_empty() {
        println!("[cortex] No session snapshots found in {}.", mined_tasks_dir.display());
        println!("         Run closeout_session (or cortex.ps1 post-session) to generate snapshots.");
        return Ok(());
    }

    let clusters = miner::cluster_snapshots(&snapshots, threshold);
    let report   = miner::format_cluster_report(&clusters);
    println!("{report}");

    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo_root.join(".cortex").join("clusters.json"));
    std::fs::write(&out_path, miner::clusters_to_json(&clusters))?;
    println!("[cortex] Cluster JSON written to {}", out_path.display());

    let _ = store; // keep borrow alive
    Ok(())
}

fn run_detect_skills(min_occurrences: u32, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));

    // Load clusters from existing clusters.json if present, otherwise re-cluster.
    let clusters_path = repo_root.join(".cortex").join("clusters.json");
    let clusters: Vec<miner::SessionCluster> = if clusters_path.exists() {
        let raw = std::fs::read_to_string(&clusters_path)?;
        serde_json::from_str(&raw).unwrap_or_default()
    } else {
        let mined_tasks_dir = repo_root.join(".cortex").join("mined-tasks");
        let snapshots = miner::load_snapshots(&mined_tasks_dir)?;
        miner::cluster_snapshots(&snapshots, 0.55)
    };

    let prefs = load_prefs_from_repo(repo_root);
    let skills_dir = &prefs.skills.skills_dir;
    let proposals_dir = repo_root.join(".cortex").join("proposals");

    let candidates = skills::detect_skill_candidates(&store, &clusters, min_occurrences)?;

    if candidates.is_empty() {
        println!("[cortex] No skill candidates with >= {} occurrences found.", min_occurrences);
        return Ok(());
    }

    println!("[cortex] {} skill candidate(s) detected:", candidates.len());
    for c in &candidates {
        match skills::draft_skill_file(
            &c.name, &c.tool_sequence, c.occurrence_count, c.confidence,
            &proposals_dir, skills_dir,
        ) {
            Ok(path) => {
                let _ = skills::set_skill_draft_path(&store, &c.name, &path);
                println!("  [drafted] {} → {}", c.name, path);
            }
            Err(e) => println!("  [error]   {}: {e}", c.name),
        }
    }
    Ok(())
}

fn run_propose_gaps(min_count: i64, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let proposals = skills::detect_gap_proposals(&store, min_count)?;
    if proposals.is_empty() {
        println!("[cortex] No hot query gaps with >= {min_count} occurrences.");
        return Ok(());
    }
    println!("[cortex] {} gap proposal(s):", proposals.len());
    for p in &proposals {
        println!("  [{}x] {} (via {}) → {}", p.seen_count, p.query_text, p.tool_name, &p.proposed_note[..p.proposed_note.len().min(80)]);
    }
    Ok(())
}

fn run_propose_survival(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));
    let proposals_dir = repo_root.join(".cortex").join("proposals");
    std::fs::create_dir_all(&proposals_dir)?;
    let count = consolidator2::propose_survival_pub(&store, &proposals_dir)?;
    println!("[cortex] {} dying pattern proposal(s) written to {}", count, proposals_dir.display());
    Ok(())
}

fn run_consolidate_pipeline(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));
    let prefs = load_prefs_from_repo(repo_root);
    let result = consolidator2::run(&store, repo_root, &prefs)?;
    println!("[cortex] {}", result.summary());
    Ok(())
}

fn run_consolidate_if_stale(staleness_hours: u32, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    if !consolidator2::is_stale(&store, staleness_hours) {
        println!("[cortex] consolidation is fresh (last run < {staleness_hours}h ago). Skipping.");
        return Ok(());
    }
    drop(store);
    run_consolidate_pipeline(db_path)
}

fn run_review_proposals(kind: Option<&str>, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let mut proposals = consolidator2::load_pending_proposals(&store)?;
    if let Some(k) = kind {
        proposals.retain(|p| p.proposal_type.contains(k));
    }
    if proposals.is_empty() {
        println!("[cortex] No pending proposals{}.",
            kind.map(|k| format!(" of type '{k}'")).unwrap_or_default());
        return Ok(());
    }
    println!("{}", consolidator2::format_pending_proposals(&proposals));
    println!("To approve: cortex.exe proposal-approve <id>");
    println!("To reject:  cortex.exe proposal-reject <id> [--reason \"...\"]");
    Ok(())
}

/// Approve or reject one proposal by id, without an interactive prompt.
///
/// Reports what the proposal WAS before changing it, so the record of the
/// decision is legible afterwards, and refuses an id that does not exist rather
/// than reporting success for a no-op update.
fn run_proposal_decision(
    id: i64,
    status: &str,
    reason: Option<&str>,
    db_path: &Path,
) -> Result<()> {
    let store = Store::open(db_path)?;
    let pending = consolidator2::load_pending_proposals(&store)?;
    let Some(p) = pending.iter().find(|p| p.id == id) else {
        anyhow::bail!(
            "no pending proposal with id {id} — run `review-proposals` for the current list"
        );
    };
    let summary: String = p.proposed_text.lines().next().unwrap_or("").chars().take(100).collect();
    consolidator2::set_proposal_status(&store, id, status)?;
    println!("[cortex] [{id}] {} → {status}", p.proposal_type);
    println!("         {summary}");
    if let Some(r) = reason {
        println!("         reason: {r}");
    }
    Ok(())
}

fn run_skill_status(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let candidates = skills::list_skill_candidates(&store)?;
    print!("{}", skills::format_skill_status(&candidates));
    Ok(())
}

fn run_skill_approve(name: &str, db_path: &Path, force: bool) -> Result<()> {
    let store = Store::open(db_path)?;
    let repo_root = db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new("."));

    // Locate the draft. Approval publishes a file; without one there is nothing
    // to approve, and flipping the status alone is how three skills came to be
    // recorded as live while existing nowhere on disk.
    let draft: Option<String> = store.conn().query_row(
        "SELECT draft_path FROM skill_candidates WHERE name = ?1",
        rusqlite::params![name],
        |r| r.get(0),
    ).ok().flatten();

    let Some(draft) = draft else {
        println!("[cortex] No skill candidate named '{name}' found.");
        return Ok(());
    };

    let draft_path = {
        let p = Path::new(&draft);
        if p.is_absolute() { p.to_path_buf() } else { repo_root.join(p) }
    };

    let prefs = load_prefs_from_repo(repo_root);
    let skills_dir = repo_root.join(&prefs.skills.skills_dir);

    // Only publish a Copilot prompt file into a repo that already has a
    // `.github/` — creating one for a repo that has none is presumptuous.
    let github = repo_root.join(".github");
    let prompts = github.join("prompts");
    let copilot = if github.is_dir() { Some(prompts.as_path()) } else { None };

    let written = skills::publish_skill(name, &draft_path, &skills_dir, copilot, force)?;

    // Status is recorded only after the files exist.
    skills::set_skill_status(&store, name, "approved")?;
    println!("[cortex] Skill '{name}' approved and published:");
    for path in &written {
        println!("         {}", path.display());
    }
    Ok(())
}

fn run_skill_reject(name: &str, db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let changed = skills::set_skill_status(&store, name, "rejected")?;
    if changed > 0 {
        println!("[cortex] Skill '{name}' rejected.");
    } else {
        println!("[cortex] No skill candidate named '{name}' found.");
    }
    Ok(())
}

fn run_session_orphans(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let grace = chrono::Utc::now().timestamp() - 7200; // 2h grace period

    // A row is created for the current wall-clock minute on ANY MCP server
    // start, so every editor reload and every `cortex-reset` mints one. Most
    // "orphans" are therefore empty rows that never retrieved anything and
    // never could have produced knowledge — counting them as lost work turns a
    // real signal into noise, which is how this reached 57 and got ignored.
    //
    // Only a session that actually retrieved something had knowledge to lose —
    // AND that `test_signal` has not already scored.
    //
    // Those are different questions, and conflating them made this report
    // measure the wrong one. `closeout_run` records whether someone called
    // `closeout_session`; test-outcome scoring reads the verdict off the build
    // instead, precisely so an unclosed session is no longer lost. Ignoring that
    // reported a session as lost work in the same minute its verdict was
    // written — `session_000000006a7f1600` appeared in this list and in
    // `session_verdict` at once. A report that cries about work already
    // recovered is a report people stop reading.
    let mut stmt = store.conn().prepare(
        "SELECT p.session_key, p.started_at,
                (p.delta_retrieved + p.preferences_loaded + p.anti_patterns_loaded + p.context_loaded) AS steps,
                p.bootstrap_complete
         FROM protocol_sessions p
         WHERE p.closeout_run = 0
           AND p.started_at < ?1
           AND NOT EXISTS (SELECT 1 FROM session_verdict v WHERE v.session_id = p.session_key)
         ORDER BY p.started_at DESC"
    )?;
    let rows = stmt.query_map(rusqlite::params![grace], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let all: Vec<_> = rows.filter_map(|r| r.ok()).collect();
    let (worked, empty): (Vec<_>, Vec<_>) = all.into_iter().partition(|(_, _, steps, _)| *steps > 0);

    // Anything older than a month is archaeology: it pre-dates outcome scoring,
    // nobody is going to close it, and it will never leave this list. Carrying
    // it forever turns a live signal into a constant number people learn to
    // read past — so it is counted, not enumerated.
    let cutoff = chrono::Utc::now().timestamp() - 30 * 86_400;
    let (recent, historical): (Vec<_>, Vec<_>) =
        worked.into_iter().partition(|(_, ts, _, _)| *ts >= cutoff);

    if recent.is_empty() {
        println!("[cortex] No recent session did work without being scored.");
    } else {
        println!(
            "[cortex] {} session(s) in the last 30 days retrieved knowledge, never closed out, \
             and have no build verdict:",
            recent.len()
        );
        for (key, ts, steps, boot) in &recent {
            let age = (chrono::Utc::now().timestamp() - ts) / 3600;
            let b = if *boot == 1 { ", bootstrap complete" } else { "" };
            println!("  {key} ({age}h ago, {steps}/4 baseline steps{b})");
        }
    }
    if !historical.is_empty() {
        println!(
            "[cortex] {} older session(s) pre-date outcome scoring — not actionable, not listed.",
            historical.len()
        );
    }
    if !empty.is_empty() {
        println!(
            "
[cortex] {} empty session row(s) ignored — created by an MCP server start              that never retrieved anything. Not lost work.",
            empty.len()
        );
    }
    Ok(())
}

fn run_health_report(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;

    let patterns: i64 = store.conn().query_row("SELECT COUNT(*) FROM patterns", [], |r| r.get(0)).unwrap_or(0);
    let low_survival: i64 = store.conn().query_row("SELECT COUNT(*) FROM patterns WHERE survival_rate < 0.4", [], |r| r.get(0)).unwrap_or(0);
    let anti_patterns: i64 = store.conn().query_row("SELECT COUNT(*) FROM anti_patterns", [], |r| r.get(0)).unwrap_or(0);
    let pending_obs: i64 = store.conn().query_row("SELECT COUNT(*) FROM pending_observations", [], |r| r.get(0)).unwrap_or(0);
    let pending_proposals: i64 = store.conn().query_row("SELECT COUNT(*) FROM proposals WHERE status='pending'", [], |r| r.get(0)).unwrap_or(0);
    // Only sessions that actually retrieved something. A row is minted on every
    // MCP server start, so counting all of them reported 57 "orphans" on a day
    // with one real session — a number nobody could act on, which is the same
    // as no number at all.
    let orphans: i64 = store.conn().query_row(
        // Same two corrections as `session-orphans`: a session `test_signal`
        // already scored is not lost work whatever `closeout_run` says, and a
        // session older than the scoring mechanism is not actionable.
        "SELECT COUNT(*) FROM protocol_sessions p
          WHERE p.closeout_run=0
            AND p.started_at < (unixepoch()-7200)
            AND p.started_at >= (unixepoch()-2592000)
            AND (p.delta_retrieved + p.preferences_loaded + p.anti_patterns_loaded + p.context_loaded) > 0
            AND NOT EXISTS (SELECT 1 FROM session_verdict v WHERE v.session_id = p.session_key)",
        [], |r| r.get(0)
    ).unwrap_or(0);
    let gaps: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM query_gap_log WHERE seen_count >= 3 AND last_seen_at >= (unixepoch()-604800)",
        [], |r| r.get(0)
    ).unwrap_or(0);

    println!("=== CORTEX HEALTH REPORT ===");
    println!("  patterns:          {} ({} below 40% survival)", patterns, low_survival);
    println!("  anti-patterns:     {}", anti_patterns);
    println!("  pending review:    {}", pending_obs);
    println!("  pending proposals: {}", pending_proposals);
    println!("  unclosed sessions: {} (that did work)", orphans);
    println!("  hot gaps (7d):     {}", gaps);

    if low_survival > 0 { println!("  ! {} low-survival patterns — run: cortex.ps1 quality-check", low_survival); }
    if orphans > 0       { println!("  ! {} unclosed session(s) with work in them — run: cortex session-orphans", orphans); }
    if pending_proposals > 0 { println!("  ! {} proposals pending — run: cortex.ps1 review-proposals", pending_proposals); }
    if gaps > 0          { println!("  ! {} hot query gaps — run: cortex.ps1 propose-gaps", gaps); }
    println!("===========================");
    Ok(())
}

// Helper: load prefs from repo root (best-effort — returns defaults on failure).
fn load_prefs_from_repo(repo_root: &Path) -> prefs::Preferences {
    let path = repo_root.join(".cortex").join("prefs.toml");
    prefs::load(&path).unwrap_or_default()
}
