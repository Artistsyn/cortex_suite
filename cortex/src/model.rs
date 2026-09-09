use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Epistemic tier ────────────────────────────────────────────────────────────

/// How a memory item was established. Maps to rta-smriti-brain's pramana hierarchy.
/// Ordering: higher discriminant = more authoritative.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum EpistemicTier {
    /// Single-session hypothesis; not yet corroborated.
    Kalpana    = 1,
    /// Recalled from prior session memory; no fresh confirmation.
    Smriti     = 2,
    /// Agent-inferred across sessions; plausible but unverified.
    #[default]
    Anumana    = 3,
    /// Operator/developer-supplied directly (prefs.toml, manual annotation).
    Sabda      = 4,
    /// Directly observed: survived test + review or explicit developer confirmation.
    Pratyaksha = 5,
}

impl EpistemicTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kalpana    => "kalpana",
            Self::Smriti     => "smriti",
            Self::Anumana    => "anumana",
            Self::Sabda      => "sabda",
            Self::Pratyaksha => "pratyaksha",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "pratyaksha" => Self::Pratyaksha,
            "sabda"      => Self::Sabda,
            "anumana"    => Self::Anumana,
            "smriti"     => Self::Smriti,
            _            => Self::Kalpana,
        }
    }
}

// ── Memory kind ───────────────────────────────────────────────────────────────

/// The functional role of a memory item in the context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum MemoryKind {
    /// How-to: actionable pattern (default for patterns).
    #[default]
    Procedure,
    /// Don't-do: anti-pattern or constraint.
    Constraint,
    /// Architectural decision or policy.
    Policy,
    /// General annotation or fact.
    Fact,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Procedure  => "procedure",
            Self::Constraint => "constraint",
            Self::Policy     => "policy",
            Self::Fact       => "fact",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "constraint" => Self::Constraint,
            "policy"     => Self::Policy,
            "fact"       => Self::Fact,
            _            => Self::Procedure,
        }
    }
}

// ── Trust level ───────────────────────────────────────────────────────────────

/// How a pattern was established — drives authority_score weighting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum TrustLevel {
    /// Manually promoted by the developer; highest confidence.
    Authoritative,
    /// LLM-crystallized across ≥3 sessions; high confidence.
    #[default]
    Crystallized,
    /// Auto-detected candidate; lower confidence.
    Candidate,
}

impl TrustLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Crystallized  => "crystallized",
            Self::Candidate     => "candidate",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "authoritative" => Self::Authoritative,
            "candidate"     => Self::Candidate,
            _               => Self::Crystallized,
        }
    }

    /// Numeric trust weight used in authority_score.
    pub fn weight(&self) -> f32 {
        match self {
            Self::Authoritative => 1.0,
            Self::Crystallized  => 0.65,
            Self::Candidate     => 0.25,
        }
    }
}

// ── Source representation ─────────────────────────────────────────────────────

/// A compressed semantic unit derived from a source file item.
/// Dense: conveys maximum information in minimum tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeUnit {
    pub id: String,
    /// e.g. "struct", "enum", "trait", "fn"
    pub kind: String,
    pub name: String,
    pub module_path: String,
    /// Compressed one-line semantic summary
    pub summary: String,
    /// Full compressed representation (not raw source)
    pub compressed: String,
    /// TF-IDF term vector for semantic search (term -> weight)
    pub term_vector: Vec<(String, f32)>,
    pub indexed_at: DateTime<Utc>,
}

/// A field or variant within a code unit, for structured lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeMember {
    pub parent_id: String,
    pub kind: String, // "field", "variant", "method"
    pub name: String,
    pub type_sig: String,
    pub doc: String,
}

// ── Memory ────────────────────────────────────────────────────────────────────

/// An approved pattern — something that worked and Syn explicitly approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub id: Option<i64>,
    pub name: String,
    /// What situation this pattern applies to
    pub intent: String,
    /// The actual code or pseudocode
    pub body: String,
    /// Which API items this pattern uses (names, for linkage)
    pub uses: Vec<String>,
    pub tags: Vec<String>,
    pub approved_at: DateTime<Utc>,
    pub use_count: i64,
    pub reverted_count: i64,
    pub survival_rate: f32,
    /// Computed trust signal: min(use_count, 10) / 10.0
    pub credibility: f32,
    /// How this pattern was established (authoritative / crystallized / candidate)
    pub trust_level: TrustLevel,
    /// Functional role in the context pack
    pub kind: MemoryKind,
    /// Epistemic authority tier
    pub tier: EpistemicTier,
    /// MD5 hash of name + body for dedup
    pub hash: Option<String>,
    /// How many times this pattern was included in a context pack
    pub included_in_context_count: i64,
    /// How many times the agent confirmed this pattern was useful
    pub confirmed_count: i64,
    /// How many times the agent marked this pattern as wrong
    pub corrected_count: i64,
    /// Superseded by another pattern (soft delete)
    pub superseded_by: Option<i64>,
}

/// A known bad approach — injected as negative examples so Copilot avoids them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiPattern {
    pub id: Option<i64>,
    pub description: String,
    /// What Copilot tends to generate incorrectly
    pub wrong: String,
    /// What it should do instead
    pub correct: String,
    pub tags: Vec<String>,
    pub added_at: DateTime<Utc>,
    /// MD5 hash of description + wrong for dedup
    pub hash: Option<String>,
    /// Superseded by another anti-pattern (soft delete)
    pub superseded_by: Option<i64>,
}

/// A free-form annotation — facts, constraints, or notes you want Copilot to know.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: Option<i64>,
    pub topic: String,
    pub body: String,
    pub tags: Vec<String>,
    pub added_at: DateTime<Utc>,
    /// MD5 hash of topic + body for dedup
    pub hash: Option<String>,
}

/// A record of a Copilot MCP tool call, used to track what it reaches for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCall {
    pub id: Option<i64>,
    pub tool: String,
    pub args: String,
    pub called_at: DateTime<Utc>,
}

/// An observed file change waiting for Syn's review — never auto-approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingObservation {
    pub id: Option<i64>,
    pub path: String,
    pub summary: String,
    pub diff_hint: String,
    pub observed_at: DateTime<Utc>,
}

/// An Architecture Decision Record — a formal record of a significant design choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adr {
    pub id: Option<i64>,
    pub adr_number: i64,
    pub title: String,
    /// "accepted", "proposed", "deprecated", "superseded"
    pub status: String,
    pub context: String,
    pub decision: String,
    pub reasoning: String,
    pub alternatives: String,
    pub consequences: String,
    pub concept_tags: Vec<String>,
    pub superseded_by: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A logged self-correction: Copilot attempted X, it failed, and Y was the right fix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfCorrection {
    pub id: Option<i64>,
    pub attempted: String,
    pub failure_reason: String,
    pub correction: String,
    pub tags: Vec<String>,
    pub occurrence_count: i64,
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
}

// ── Session ───────────────────────────────────────────────────────────────────

/// Pre-compiled context packet for a Copilot session.
/// Designed to be injected as minimal, high-signal preamble.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPacket {
    /// Files/modules relevant to the current task (compressed)
    pub relevant_units: Vec<CodeUnit>,
    /// Patterns that apply to current context
    pub patterns: Vec<Pattern>,
    /// Anti-patterns to warn about
    pub anti_patterns: Vec<AntiPattern>,
    /// Annotations relevant to current files
    pub annotations: Vec<Annotation>,
    /// Architecture Decision Records relevant to this context
    pub adrs: Vec<Adr>,
    /// What changed since last index (compressed deltas)
    pub deltas: Vec<DeltaEntry>,
    /// Token budget used (estimated)
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEntry {
    pub path: String,
    pub change: String, // "added", "modified", "removed"
    pub summary: String,
}

// ── Knowledge graph ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub module_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from_id: String,
    pub to_id: String,
    pub relation: RelationType,
    pub weight: f32,
    pub source: String,
    /// Unix timestamp when this edge became true (None = always/unknown)
    pub valid_at: Option<f64>,
    /// Unix timestamp when this edge was superseded; None = currently true
    pub invalid_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelationType {
    Implements,
    Uses,
    Calls,
    Pairs,
    Conflicts,
    DerivedFrom,
    /// Semantic ownership: the source type owns/contains the target (e.g. Scene → GameEvent list).
    Owns,
}

impl RelationType {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationType::Implements => "implements",
            RelationType::Uses => "uses",
            RelationType::Calls => "calls",
            RelationType::Pairs => "pairs",
            RelationType::Conflicts => "conflicts",
            RelationType::DerivedFrom => "derived_from",
            RelationType::Owns => "owns",
        }
    }

    pub fn from_str(v: &str) -> Option<Self> {
        match v {
            "implements" => Some(RelationType::Implements),
            "uses" => Some(RelationType::Uses),
            "calls" => Some(RelationType::Calls),
            "pairs" => Some(RelationType::Pairs),
            "conflicts" => Some(RelationType::Conflicts),
            "derived_from" => Some(RelationType::DerivedFrom),
            "owns" => Some(RelationType::Owns),
            _ => None,
        }
    }
}

// ── Session continuation ──────────────────────────────────────────────────────

/// Structured record of what the agent is accomplishing and what to avoid this task.
/// Append-only: every update inserts a new row; latest = ORDER BY id DESC LIMIT 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: Option<i64>,
    /// What is being accomplished in this session
    pub objective: String,
    /// What has been confirmed true (verified evidence)
    pub verified_evidence: String,
    /// What is still unknown or incomplete
    pub remaining_gaps: String,
    /// Specific next step to take
    pub next_action: String,
    /// Approaches that MUST NOT be tried again (task-scoped anti-pattern)
    pub prohibited_repetition: String,
    /// The MCP session key that wrote this checkpoint
    pub session_id: Option<String>,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Context budget ────────────────────────────────────────────────────────────

/// Per-block token budget breakdown for the assembled context pack.
/// Returned alongside the rendered pack so the LLM can self-regulate.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextWindowOverview {
    pub total_budget:       usize,
    pub used_checkpoint:    usize,
    pub used_api:           usize,
    pub used_adrs:          usize,
    pub used_patterns:      usize,
    pub used_constraints:   usize,
    pub used_annotations:   usize,
    pub used_deltas:        usize,
    pub total_used:         usize,
    pub truncated:          bool,
}

// ── Pattern history ───────────────────────────────────────────────────────────

/// Audit log entry for a pattern mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternHistoryEntry {
    pub id: Option<i64>,
    pub pattern_id: i64,
    /// "ADD", "UPDATE", "SUPERSEDE", "DELETE"
    pub event: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    /// "crystallizer", "agent", "operator"
    pub actor_id: String,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ── quartz-ctx integration ────────────────────────────────────────────────────

/// A single item from quartz-ctx's api-graph.json.
/// Mirrors the ApiItem shape from quartz-ctx so we can ingest it directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphItem {
    pub kind: String,
    pub name: String,
    pub doc: String,
    pub signature: String,
    pub module_path: Vec<String>,
    pub methods: Vec<ApiGraphMethod>,
    pub variants: Vec<ApiGraphVariant>,
    pub fields: Vec<ApiGraphField>,
    pub generics: String,
    pub traits_impl: Vec<String>,
    /// Declared visibility (`pub`, `pub(crate)`, `private`, …). Defaults to
    /// public so api-graphs written before quartz-ctx recorded visibility still
    /// deserialise.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Where the item is declared, so answers can cite `file:line`.
    #[serde(default)]
    pub span: Option<ApiGraphSpan>,
    /// Calls made from this item's bodies. See `ApiGraphCall`.
    #[serde(default)]
    pub calls: Vec<ApiGraphCall>,
}

/// One call site from quartz-ctx.
///
/// `kind` is load-bearing: `path` means the callee names its owner
/// (`Canvas::new`), `method` means only the method name is known because
/// resolving the receiver's type needs inference the extractor does not do.
/// Treating the two the same would invent ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphCall {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default)]
    pub span: Option<ApiGraphSpan>,
}

/// A `file:line` source location from quartz-ctx.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphSpan {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphMethod {
    pub name: String,
    pub doc: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphVariant {
    pub name: String,
    pub doc: String,
    pub fields: Vec<ApiGraphField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiGraphField {
    pub name: String,
    pub ty: String,
    pub doc: String,
}
