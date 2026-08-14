use serde::{Deserialize, Serialize};

/// A single public item extracted from the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiItem {
    pub kind: ItemKind,
    pub name: String,
    /// Raw doc-comment text (concatenated `///` lines).
    pub doc: String,
    /// Human-readable signature string.
    pub signature: String,
    /// Module path relative to the parsed root, e.g. `["game_object", "sprite"]`.
    pub module_path: Vec<String>,
    /// Public methods attached via `impl` blocks.
    pub methods: Vec<ApiMethod>,
    /// Variants (enums only).
    pub variants: Vec<ApiVariant>,
    /// Named fields (structs only).
    pub fields: Vec<ApiField>,
    /// Raw generics string, empty if none.
    pub generics: String,
    /// Trait names this type implements (from `impl Trait for Type` blocks).
    pub traits_impl: Vec<String>,
    /// Which source root this item came from (e.g. "quartz", "synful-quartz",
    /// "path-forge"). Empty for single-source runs. Set by the loader, not the parser.
    #[serde(default)]
    pub origin: String,
    /// How visible this item is. A library exposes its API through `pub`, but an
    /// application or binary crate mostly does not — indexing only `pub` returns
    /// almost nothing useful for a typical app, so visibility is recorded rather
    /// than used to silently drop items.
    #[serde(default)]
    pub visibility: Visibility,
    /// Where this item is declared. `None` only when the span was unavailable.
    #[serde(default)]
    pub span: Option<SourceSpan>,
    /// How much this item's shape can be trusted.
    ///
    /// Rust goes through `syn`, which resolves the language: types are real,
    /// impls are attached across files, a signature means what it says.
    /// Everything else is read off a concrete syntax tree — names and shapes as
    /// WRITTEN, with no type resolution and no cross-file linking.
    ///
    /// The distinction has to travel with the item. `lang.rs` claimed in a
    /// doc-comment that non-Rust items were "tagged `confidence: ast_only`, and
    /// the tools say so" — no such field existed, so 283 items extracted from a
    /// JavaScript frontend reached cortex indistinguishable from resolved Rust.
    /// An agent cannot calibrate what it is told if everything arrives with the
    /// same authority.
    #[serde(default)]
    pub confidence: Confidence,
    /// Which language this item was written in (`rust`, `go`, `typescript`, …).
    ///
    /// Separate from `confidence` and from `origin`, because they answer
    /// different questions and a polyglot index needs all three. `origin` says
    /// which source root ("ss_engine"), `span` says which file, and this says
    /// which language — so `Canvas` the Rust struct and `Canvas` the TypeScript
    /// class can be told apart at a glance rather than by inspecting a path.
    ///
    /// Defaulted rather than optional so an older `api-graph.json` still loads;
    /// the default is `rust`, which is what every item predating this field was.
    #[serde(default = "default_language")]
    pub language: String,
    /// Calls made from this item's own bodies (its methods, or a free function's
    /// body). Deduped, so a call made in a loop counts once.
    #[serde(default)]
    pub calls: Vec<CallEdge>,
}

fn default_language() -> String {
    "rust".to_string()
}

/// One call site found inside an item's body.
///
/// Callee resolution is deliberately partial. A path call (`Canvas::new(..)`)
/// names its owner outright; a method call (`.add_plugin(..)`) does not, because
/// knowing the receiver's type needs inference this extractor does not do.
/// Recording which kind it is lets the consumer resolve what it can and be
/// honest about the rest, rather than guessing an owner and being confidently
/// wrong.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallEdge {
    /// Enclosing function, qualified where known: `Canvas::run`, or `main`.
    pub from: String,
    /// `Canvas::new` for a path call; the bare method name for a method call.
    pub to: String,
    pub kind: CallKind,
    /// Where the call appears, so the edge is citable.
    #[serde(default)]
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CallKind {
    /// `Type::assoc(..)` or `free_fn(..)` — the path names the callee.
    Path,
    /// `receiver.method(..)` — only the method name is known here.
    Method,
    /// A call that leaves the language: an HTTP request answered by a route in
    /// another service, or a wasm/FFI symbol exported by another crate.
    ///
    /// Resolved through the literal both sides share (a URL path, an exported
    /// symbol name) rather than through either language's type system, which is
    /// the only thing that CAN join them — and is why a rename on one side has
    /// always been invisible from the other.
    CrossLanguage,
}

impl CallKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Method => "method",
            Self::CrossLanguage => "cross_language",
        }
    }
}

/// How far an extractor could see when it produced an item.
///
/// Not a quality score — a statement about the method. `Resolved` means a real
/// compiler front end agreed the types are these types. `AstOnly` means the
/// shape was read off the syntax as written, which is correct about names and
/// silent about meaning: a TypeScript `foo(x: Bar)` records `Bar` without any
/// idea what `Bar` is, or whether it exists.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Parsed by a resolving front end (`syn` for Rust).
    #[default]
    Resolved,
    /// Parsed from a concrete syntax tree (tree-sitter), then linked across
    /// files by NAME — methods attached to the owner they name, bases and
    /// interfaces recorded as declared, calls resolved where the syntax says so.
    ///
    /// This is a real resolution step, and the distinction from `AstOnly`
    /// matters: those items know nothing beyond their own file. It is still not
    /// type resolution, so two same-named types in one project can be told apart
    /// wrongly, and a call through a variable names the method without knowing
    /// the receiver.
    NameResolved,
    /// Parsed from a concrete syntax tree with no cross-file linking at all.
    /// Names and shapes within one file, and nothing more.
    AstOnly,
}

impl Confidence {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::NameResolved => "name_resolved",
            Self::AstOnly => "ast_only",
        }
    }

    /// True when the caller should treat the shape as indicative, not exact.
    ///
    /// Covers name-resolved items too: a name link can be wrong in a way a type
    /// link cannot, so anything that would be unsafe to trust blindly from
    /// `ast_only` is equally unsafe here.
    pub fn is_ast_only(&self) -> bool {
        matches!(self, Self::AstOnly | Self::NameResolved)
    }

    /// Did a resolving front end produce this?
    pub fn is_fully_resolved(&self) -> bool {
        matches!(self, Self::Resolved)
    }
}

/// Declared visibility of an item, in descending order of reach.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Visibility {
    /// `pub` — reachable outside the crate.
    #[default]
    Public,
    /// `pub(crate)` — the crate-internal API surface.
    Crate,
    /// `pub(in path)` / `pub(super)` — restricted to part of the crate.
    Restricted,
    /// No modifier — private to its module.
    Private,
}

impl Visibility {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Public => "pub",
            Self::Crate => "pub(crate)",
            Self::Restricted => "pub(restricted)",
            Self::Private => "private",
        }
    }

    /// True when this item should be kept under the given inclusion policy.
    pub fn is_included(&self, include_private: bool) -> bool {
        include_private || matches!(self, Self::Public)
    }
}

/// A source location an agent can cite and open: `path:line`.
/// `file` is relative to the scanned root and always uses forward slashes, so it
/// reads identically on every platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSpan {
    pub file: String,
    pub line: usize,
}

impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// Split a doc comment into prose and fenced code blocks.
///
/// Rustdoc examples live in ```` ``` ```` fences inside the doc text. Flattening
/// the whole comment into one blob loses them as runnable syntax, so they are
/// pulled out and surfaced separately. Rustdoc's hidden-line prefix (`# `) is
/// stripped, matching how the code would actually be read.
pub fn split_doc(doc: &str) -> (String, Vec<String>) {
    let mut prose = Vec::new();
    let mut blocks = Vec::new();
    let mut current: Option<Vec<String>> = None;

    for line in doc.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match current.take() {
                Some(block) => blocks.push(block.join("\n")),
                None => current = Some(Vec::new()),
            }
            continue;
        }
        match &mut current {
            Some(block) => {
                // `# ` marks a line rustdoc compiles but hides from readers.
                let code = trimmed.strip_prefix("# ").unwrap_or(line);
                if code.trim() != "#" {
                    block.push(code.to_string());
                }
            }
            None => prose.push(line),
        }
    }
    // An unterminated fence still yields its content rather than dropping it.
    if let Some(block) = current {
        if !block.is_empty() {
            blocks.push(block.join("\n"));
        }
    }

    (prose.join("\n").trim().to_string(), blocks)
}

impl ApiItem {
    /// Returns the first line of the doc comment, suitable for inline hints.
    pub fn doc_summary(&self) -> &str {
        self.doc.lines().next().map(str::trim).unwrap_or("")
    }

    /// Doc text with fenced code blocks removed.
    pub fn doc_prose(&self) -> String {
        split_doc(&self.doc).0
    }

    /// Fenced code blocks found in this item's doc comment.
    pub fn doc_examples(&self) -> Vec<String> {
        split_doc(&self.doc).1
    }

    /// Module path joined with `::`.
    pub fn module_str(&self) -> String {
        self.module_path.join("::")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ItemKind {
    Struct,
    Enum,
    Trait,
    Function,
    TypeAlias,
    Const,
}

impl ItemKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Struct    => "struct",
            Self::Enum      => "enum",
            Self::Trait     => "trait",
            Self::Function  => "fn",
            Self::TypeAlias => "type",
            Self::Const     => "const",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMethod {
    pub name: String,
    pub doc: String,
    pub signature: String,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub span: Option<SourceSpan>,
}

impl ApiMethod {
    pub fn doc_summary(&self) -> &str {
        self.doc.lines().next().map(str::trim).unwrap_or("")
    }

    pub fn doc_prose(&self) -> String {
        split_doc(&self.doc).0
    }

    pub fn doc_examples(&self) -> Vec<String> {
        split_doc(&self.doc).1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiVariant {
    pub name: String,
    pub doc: String,
    pub fields: Vec<ApiField>,
}

impl ApiVariant {
    pub fn doc_summary(&self) -> &str {
        self.doc.lines().next().map(str::trim).unwrap_or("")
    }

    /// Render variant fields as a compact inline string, e.g. `{ path: String, volume: f32 }`.
    pub fn fields_inline(&self) -> String {
        if self.fields.is_empty() {
            return String::new();
        }
        let inner: Vec<String> = self.fields.iter()
            .map(|f| {
                if f.name.starts_with('_') {
                    f.ty.clone()
                } else {
                    format!("{}: {}", f.name, f.ty)
                }
            })
            .collect();
        format!("{{ {} }}", inner.join(", "))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiField {
    pub name: String,
    pub ty: String,
    pub doc: String,
}

// ── Extended Metadata for Advanced Tools ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeExample {
    pub title: String,
    pub description: String,
    pub code: String,
    pub context: String, // "common", "physics", "input", "advanced"
    pub source: String,  // where this came from
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiPattern {
    pub name: String,
    pub description: String,
    pub wrong_code: String,
    pub correct_code: String,
    pub consequence: String,
    pub affected_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraitInfo {
    pub name: String,
    pub types_implementing: Vec<String>,
    pub required_methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderInfo {
    pub base_type: String,
    pub builder_name: String,
    pub method_sequence: Vec<BuilderMethod>,
    pub finish_returns: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderMethod {
    pub name: String,
    pub params: Vec<(String, String)>, // (name, type)
    pub returns: String,
    pub doc: String,
    pub order_dependency: Option<String>, // method that must come before
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRequirement {
    pub field: String,
    pub prerequisites: Vec<String>,
    pub incompatibilities: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceChar {
    pub operation: String,
    pub complexity: String,
    pub cost_description: String,
    pub optimization_tips: Vec<String>,
}
