use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

/// A free-form annotation — facts, constraints, or notes you want Copilot to know.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: Option<i64>,
    pub topic: String,
    pub body: String,
    pub tags: Vec<String>,
    pub added_at: DateTime<Utc>,
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
