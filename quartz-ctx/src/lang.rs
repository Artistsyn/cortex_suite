//! Non-Rust extraction, via tree-sitter.
//!
//! # Why this is not simply "Rust plus some other grammars"
//!
//! Rust is parsed by `syn`, which resolves the language properly. Everything
//! else is parsed from its concrete syntax tree, which is a genuinely weaker
//! signal: no type inference, no macro expansion, no idea whether a name refers
//! to what it appears to.
//!
//! But "weaker" was doing far too much work as an excuse. Of the nine tools this
//! server exposes, five returned nothing at all for a non-Rust project — not
//! because tree-sitter cannot see the information, but because this module
//! stopped at per-file extraction while the Rust front end went on to run a
//! project-wide attachment pass. Methods declared away from their type, base
//! classes, interfaces, and every call edge were dropped on the floor by the
//! *pipeline*, not by the parser.
//!
//! That gap is the exact shape of a bug this project has already paid for once:
//! attaching `impl` blocks per file served `Canvas` with zero of its 115
//! methods. In Go it is not an edge case but the norm — `func (c *Canvas) Draw()`
//! usually lives in a different file from `type Canvas struct`. A per-file
//! extractor reports a Go codebase as a pile of structs with no behaviour and
//! looks entirely healthy doing it.
//!
//! So: these front ends emit the same two things the Rust front end emits —
//! items, and [`PendingImpl`]s for anything whose owner is not in this file —
//! and [`crate::parser`] runs **one** resolution pass over all of them. There is
//! no second pipeline to drift out of sync.
//!
//! # What is still honestly weaker
//!
//! Cross-file attachment here is by NAME, disambiguated by module-path overlap.
//! That is a real resolution step and the reason these items are no longer
//! tagged `ast_only` — but it is not type resolution, and two same-named types
//! in one project can still be told apart wrongly. Items carry
//! [`Confidence::NameResolved`] to say precisely that, rather than borrowing
//! Rust's `resolved` or hiding behind `ast_only`.

use std::path::Path;

use tree_sitter::{Node, Parser as TsParser};

use crate::model::{
    ApiField, ApiItem, ApiMethod, CallEdge, CallKind, Confidence, ItemKind, SourceSpan, Visibility,
};
use crate::parser::PendingImpl;

/// Languages handled here. Rust is deliberately absent — `syn` does it better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    JavaScript,
    Go,
    Java,
    CSharp,
    Cpp,
    Ruby,
    Php,
}

impl Language {
    /// Which language a file is, by extension. `None` means "not ours".
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" | "tsx" => Some(Self::TypeScript),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            // C is parsed by the C++ grammar, which is a superset. A `.h` may be
            // either; the C++ grammar reads both.
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "h" | "c" => Some(Self::Cpp),
            "rb" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Cpp => "cpp",
            Self::Ruby => "ruby",
            Self::Php => "php",
        }
    }

    fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::JavaScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        }
    }

    /// Does this language conventionally capitalise type names and lowercase
    /// locals? Used — and only used — to decide whether `Foo.bar()` is a static
    /// call on a type or a method call on a variable.
    fn types_are_capitalised(&self) -> bool {
        matches!(self, Self::Go | Self::Java | Self::CSharp | Self::TypeScript)
    }

    /// Node kinds declaring a class-like type.
    fn class_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Python => &["class_definition"],
            Self::TypeScript | Self::JavaScript => &["class_declaration"],
            Self::Go => &["type_declaration"],
            Self::Java => &["class_declaration", "record_declaration"],
            Self::CSharp => &["class_declaration", "struct_declaration", "record_declaration"],
            Self::Cpp => &["class_specifier", "struct_specifier"],
            Self::Ruby => &["class"],
            Self::Php => &["class_declaration", "trait_declaration"],
        }
    }

    /// Node kinds declaring an interface / protocol / trait.
    fn interface_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::TypeScript | Self::JavaScript => &["interface_declaration"],
            Self::Java | Self::CSharp | Self::Php => &["interface_declaration"],
            Self::Ruby => &["module"],
            _ => &[],
        }
    }

    fn enum_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::TypeScript | Self::JavaScript | Self::Java | Self::CSharp | Self::Php => {
                &["enum_declaration"]
            }
            Self::Cpp => &["enum_specifier"],
            _ => &[],
        }
    }

    /// Free (non-member) function declarations.
    fn function_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Python => &["function_definition"],
            Self::TypeScript | Self::JavaScript => &["function_declaration"],
            Self::Go => &["function_declaration"],
            Self::Cpp => &["function_definition", "declaration"],
            Self::Ruby => &["method"],
            Self::Php => &["function_definition"],
            // Java and C# have no top-level functions.
            _ => &[],
        }
    }

    /// Member function declarations, found inside a type body.
    fn method_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Python => &["function_definition"],
            Self::TypeScript | Self::JavaScript => &["method_definition", "method_signature"],
            Self::Java => &["method_declaration", "constructor_declaration"],
            Self::CSharp => &["method_declaration", "constructor_declaration"],
            Self::Cpp => &["function_definition", "field_declaration", "declaration"],
            Self::Ruby => &["method", "singleton_method"],
            Self::Php => &["method_declaration"],
            // Go members are never inside the type body — see `go_method`.
            Self::Go => &[],
        }
    }

    fn field_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::TypeScript | Self::JavaScript => &["public_field_definition", "property_signature"],
            Self::Java => &["field_declaration"],
            Self::CSharp => &["field_declaration", "property_declaration"],
            Self::Cpp => &["field_declaration"],
            Self::Php => &["property_declaration"],
            Self::Go => &["field_declaration"],
            _ => &[],
        }
    }

    /// Nodes that only WRAP declarations. Failing to descend into one of these
    /// silently loses every declaration underneath it — a C# file whose classes
    /// all sit inside `namespace Foo { ... }` would extract as empty, and look
    /// exactly like a file with no classes.
    fn transparent_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Python => &["decorated_definition"],
            Self::TypeScript | Self::JavaScript => &["export_statement", "decorated_definition"],
            Self::CSharp => &[
                "namespace_declaration",
                "file_scoped_namespace_declaration",
                "declaration_list",
                "compilation_unit",
            ],
            Self::Cpp => &["namespace_definition", "declaration_list", "linkage_specification"],
            Self::Java => &["program"],
            Self::Php => &["php_tag", "namespace_definition", "declaration_list"],
            Self::Go => &["type_declaration"],
            Self::Ruby => &[],
        }
    }

    /// Call-expression node kinds, and the field naming the callee.
    fn call_kinds(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Python => &[("call", "function")],
            Self::TypeScript | Self::JavaScript => &[("call_expression", "function")],
            Self::Go => &[("call_expression", "function")],
            Self::Java => &[("method_invocation", "name"), ("object_creation_expression", "type")],
            Self::CSharp => &[("invocation_expression", "function")],
            Self::Cpp => &[("call_expression", "function")],
            Self::Ruby => &[("call", "method")],
            Self::Php => &[
                ("function_call_expression", "function"),
                ("member_call_expression", "name"),
                ("scoped_call_expression", "name"),
            ],
        }
    }

    /// Fields on a type declaration that name its bases / interfaces.
    fn supertype_fields(&self) -> &'static [&'static str] {
        match self {
            Self::Python => &["superclasses"],
            Self::TypeScript | Self::JavaScript => &["heritage"],
            Self::Java => &["superclass", "interfaces", "super_interfaces"],
            Self::CSharp | Self::Php | Self::Cpp => &["bases"],
            _ => &[],
        }
    }
}

/// What one file yielded: items, plus anything whose owner lives elsewhere.
#[derive(Default)]
pub struct Extracted {
    pub items: Vec<ApiItem>,
    pub orphans: Vec<PendingImpl>,
    /// Types the SOURCE declares as split across files (C# `partial`).
    ///
    /// Kept separate from `orphans` because the orphan pass resolves by module
    /// proximity, and each half of a partial is closest to itself — so routing
    /// them that way reunites every half with the one place it already was.
    /// Merging same-named types on sight is not an option either: that is how
    /// `editor::State` absorbs `engine::State`. Only names the language itself
    /// declares as one type belong here.
    pub partial_types: Vec<String>,
}

/// Extract the API surface of one non-Rust file.
pub fn parse_file(
    source: &str,
    lang: Language,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
) -> Extracted {
    let mut parser = TsParser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Extracted::default();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Extracted::default();
    };

    let mut cx = Cx {
        src: source,
        lang,
        module_path: module_path.to_vec(),
        rel_path: rel_path.to_string(),
        include_private,
        out: Extracted::default(),
    };
    cx.walk(tree.root_node());
    cx.out
}

struct Cx<'a> {
    src: &'a str,
    lang: Language,
    module_path: Vec<String>,
    rel_path: String,
    include_private: bool,
    out: Extracted,
}

impl<'a> Cx<'a> {
    fn walk(&mut self, node: Node) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node) {
        let kind = node.kind();
        let l = self.lang;

        // Wrappers first: an unrecognised wrapper loses everything beneath it.
        if l.transparent_kinds().contains(&kind) {
            // Go's `type_declaration` wraps one or more `type_spec`s.
            self.walk(node);
            return;
        }

        if l == Language::Go && kind == "method_declaration" {
            if let Some(p) = self.go_method(node) {
                self.out.orphans.push(p);
            }
            return;
        }

        if l == Language::Go && kind == "type_spec" {
            if let Some(item) = self.go_type(node) {
                self.out.items.push(item);
            }
            return;
        }

        if l == Language::Cpp && kind == "function_definition" {
            // An out-of-line member definition (`void Canvas::draw() {}`) is
            // C++'s version of the cross-file impl block, and is the normal way
            // to write a .cpp — dropping it leaves every class with only the
            // signatures its header happened to inline.
            if let Some(p) = self.cpp_out_of_line(node) {
                self.out.orphans.push(p);
                return;
            }
        }

        if l.class_kinds().contains(&kind) {
            if let Some((item, partial)) = self.type_item(node, ItemKind::Struct) {
                if partial && !self.out.partial_types.contains(&item.name) {
                    self.out.partial_types.push(item.name.clone());
                }
                self.out.items.push(item);
            }
            return;
        }
        if l.interface_kinds().contains(&kind) {
            if let Some((item, _)) = self.type_item(node, ItemKind::Trait) {
                self.out.items.push(item);
            }
            return;
        }
        if l.enum_kinds().contains(&kind) {
            if let Some(item) = self.enum_item(node) {
                self.out.items.push(item);
            }
            return;
        }
        if kind == "type_alias_declaration" {
            if let Some(name) = child_text(node, "name", self.src) {
                let item = self.base_item(ItemKind::TypeAlias, name, node);
                self.out.items.push(item);
            }
            return;
        }
        if l.function_kinds().contains(&kind) {
            if let Some(item) = self.function_item(node) {
                self.out.items.push(item);
            }
            return;
        }

        // Anything unrecognised at this level may still contain declarations
        // (a Ruby `module` body, a PHP namespace block). Descending is cheap;
        // not descending is silent loss.
        if node.child_count() > 0 && node.named_child_count() > 0 {
            self.walk(node);
        }
    }

    // ── types ──────────────────────────────────────────────────────────────

    /// Returns the item and whether it was declared `partial`.
    fn type_item(&mut self, node: Node, kind: ItemKind) -> Option<(ApiItem, bool)> {
        let name = child_text(node, "name", self.src)
            .or_else(|| self.ruby_class_name(node))?;
        let mut item = self.base_item(kind, name, node);
        item.traits_impl = self.supertypes(node);

        let partial = self.lang == Language::CSharp && self.head_text(node).contains("partial ");

        let Some(body) = type_body(node, self.lang) else { return Some((item, partial)) };
        self.collect_members(body, &mut item);
        Some((item, partial))
    }

    fn collect_members(&mut self, body: Node, item: &mut ApiItem) {
        let l = self.lang;
        let mut c = body.walk();
        let members: Vec<Node> = body.named_children(&mut c).collect();

        // C++ and Ruby change visibility with a marker that applies to
        // everything after it, so member visibility is stateful, not local.
        let mut section = match l {
            // A `class` defaults to private, a `struct` to public.
            Language::Cpp => {
                if body.parent().map(|p| p.kind()) == Some("struct_specifier") {
                    Visibility::Public
                } else {
                    Visibility::Private
                }
            }
            _ => Visibility::Public,
        };

        // Interface members carry no access modifier because they cannot: they
        // are public by definition. Reading their absent `public` as "not
        // declared public" hides every interface method in Java, C# and PHP —
        // which is to say it empties the one thing an interface exists to
        // declare, while the type itself still shows up and looks fine.
        let implicitly_public = matches!(
            body.kind(),
            "interface_body" | "interface_declaration_list"
        ) || body.parent().map(|p| p.kind()) == Some("interface_declaration");

        for member in members {
            let mk = member.kind();

            if l == Language::Cpp && mk == "access_specifier" {
                section = match text(member, self.src).trim().trim_end_matches(':') {
                    "public" => Visibility::Public,
                    "protected" => Visibility::Crate,
                    _ => Visibility::Private,
                };
                continue;
            }
            if l == Language::Ruby && mk == "identifier" {
                match text(member, self.src).trim() {
                    "private" => section = Visibility::Private,
                    "protected" => section = Visibility::Crate,
                    "public" => section = Visibility::Public,
                    _ => {}
                }
                continue;
            }
            // Ruby, PHP and Java wrap members in a body/declaration list.
            if matches!(mk, "declaration_list" | "class_body" | "field_declaration_list" | "body_statement")
            {
                self.collect_members(member, item);
                continue;
            }

            let vis_here = if implicitly_public { Visibility::Public } else { section };

            // Before the method branch, which consumes a function_definition
            // and moves on: in Python a method body is also where instance
            // attributes are declared, so the same node has to be read twice.
            if l == Language::Python {
                self.python_fields(member, vis_here, item);
            }

            if l.method_kinds().contains(&mk) {
                if let Some(m) = self.method(member, vis_here, implicitly_public) {
                    // A C++ field_declaration can be a member FUNCTION
                    // declaration; if it has no parameter list it is data.
                    if !item.methods.iter().any(|e| e.name == m.name) {
                        item.calls.extend(self.calls_in(member, &format!("{}::{}", item.name, m.name)));
                        item.methods.push(m);
                    }
                    continue;
                }
            }
            if l.field_kinds().contains(&mk) {
                for f in self.fields(member, vis_here, implicitly_public) {
                    if !item.fields.iter().any(|e| e.name == f.name) {
                        item.fields.push(f);
                    }
                }
            }
        }
    }

    fn enum_item(&mut self, node: Node) -> Option<ApiItem> {
        let name = child_text(node, "name", self.src)?;
        let mut item = self.base_item(ItemKind::Enum, name, node);
        let Some(body) = type_body(node, self.lang) else { return Some(item) };

        let mut c = body.walk();
        for member in body.named_children(&mut c) {
            // C#'s members sit one level deeper; Java's enum constants are
            // direct children.
            let holder = if member.kind() == "enum_member_declaration_list" {
                let mut c2 = member.walk();
                member.named_children(&mut c2).collect::<Vec<_>>()
            } else {
                vec![member]
            };
            for m in holder {
                if m.kind() == "comment" {
                    continue;
                }
                let vname = child_text(m, "name", self.src)
                    .or_else(|| Some(text(m, self.src).trim().to_string()))
                    .filter(|t| !t.is_empty() && !t.starts_with('{'));
                if let Some(v) = vname {
                    let v = v.split(['=', '(', ',']).next().unwrap_or(&v).trim().to_string();
                    if !v.is_empty() && !item.variants.iter().any(|e| e.name == v) {
                        item.variants.push(crate::model::ApiVariant {
                            name: v,
                            doc: String::new(),
                            fields: vec![],
                        });
                    }
                }
            }
        }
        Some(item)
    }

    /// Base classes and implemented interfaces, as plain names.
    ///
    /// This is what makes `get_trait_implementations` answer anything at all
    /// outside Rust. Before it, asking "what implements `Drawable`" in a Java or
    /// C# project returned an empty list — indistinguishable from "nothing does".
    fn supertypes(&self, node: Node) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for field in self.lang.supertype_fields() {
            let Some(n) = node.child_by_field_name(field) else { continue };
            for name in type_names_in(n, self.src) {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        // TypeScript keeps `extends`/`implements` in an unnamed heritage clause,
        // and Ruby/PHP in their own node kinds, so a field lookup alone misses
        // them.
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if matches!(
                child.kind(),
                "class_heritage"
                    | "extends_clause"
                    | "implements_clause"
                    | "base_list"
                    | "superclass"
                    | "super_interfaces"
                    | "base_clause"
                    | "class_interface_clause"
                    | "argument_list"
                    | "superclass_arguments"
                    | "base_class_clause"
            ) {
                for name in type_names_in(child, self.src) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
        out
    }

    // ── Go ─────────────────────────────────────────────────────────────────

    fn go_type(&mut self, spec: Node) -> Option<ApiItem> {
        let name = child_text(spec, "name", self.src)?;
        let ty = spec.child_by_field_name("type")?;
        let kind = match ty.kind() {
            "interface_type" => ItemKind::Trait,
            _ => ItemKind::Struct,
        };
        let mut item = self.base_item(kind, name, spec);
        item.visibility = go_visibility(&item.name);
        if !item.visibility.is_included(self.include_private) {
            return None;
        }
        // An interface's method set is declared inline; a struct's fields are.
        let mut c = ty.walk();
        for member in ty.named_children(&mut c) {
            match member.kind() {
                "method_elem" | "method_spec" => {
                    if let Some(n) = child_text(member, "name", self.src) {
                        item.methods.push(ApiMethod {
                            doc: String::new(),
                            signature: signature_text(member, self.src),
                            visibility: go_visibility(&n),
                            span: span_of(member, &self.rel_path),
                            name: n,
                        });
                    }
                }
                "field_declaration_list" => {
                    let mut c2 = member.walk();
                    for f in member.named_children(&mut c2) {
                        // Go has no access modifiers: `member_visibility` reads
                        // the field's own capitalisation, so the section passed
                        // here is only a starting point it does not consult.
                        for field in self.fields(f, Visibility::Public, false) {
                            item.fields.push(field);
                        }
                    }
                }
                _ => {}
            }
        }
        Some(item)
    }

    /// `func (c *Canvas) Draw(x int) error { ... }`
    ///
    /// The receiver names the owning type, which is usually declared in another
    /// file. This is the single most important reason a per-file extractor
    /// reports a Go project as structs with no behaviour.
    fn go_method(&mut self, node: Node) -> Option<PendingImpl> {
        let name = child_text(node, "name", self.src)?;
        let recv = node.child_by_field_name("receiver")?;
        // `(c *Canvas)` parses as
        //   parameter_declaration name: (identifier) type: (pointer_type (type_identifier))
        // so the owner is specifically the `type` field. Scanning the clause for
        // "some identifier" instead picks up the receiver VARIABLE (`c`) just as
        // readily, and attaches every method to a type named `c`.
        let owner = {
            let mut c = recv.walk();
            let decl = recv
                .named_children(&mut c)
                .find(|n| n.kind() == "parameter_declaration")?;
            let ty = decl.child_by_field_name("type")?;
            type_names_in(ty, self.src).pop()?
        };

        let vis = go_visibility(&name);
        if !vis.is_included(self.include_private) {
            return None;
        }
        let qualified = format!("{owner}::{name}");
        let calls = self.calls_in(node, &qualified);
        Some(PendingImpl {
            self_ty: owner,
            trait_name: None,
            methods: vec![ApiMethod {
                doc: leading_doc(node, self.src, self.lang),
                signature: signature_text(node, self.src),
                visibility: vis,
                span: span_of(node, &self.rel_path),
                name,
            }],
            calls,
            module_path: self.module_path.clone(),
        })
    }

    // ── C++ ────────────────────────────────────────────────────────────────

    /// `void Canvas::draw(int x) { ... }` defined outside the class body.
    fn cpp_out_of_line(&mut self, node: Node) -> Option<PendingImpl> {
        let decl = node.child_by_field_name("declarator")?;
        // function_declarator > qualified_identifier(scope: Canvas, name: draw)
        let inner = decl.child_by_field_name("declarator")?;
        if inner.kind() != "qualified_identifier" {
            return None;
        }
        let owner = child_text(inner, "scope", self.src)?;
        let name = child_text(inner, "name", self.src)?;
        let qualified = format!("{owner}::{name}");
        let calls = self.calls_in(node, &qualified);
        Some(PendingImpl {
            self_ty: owner,
            trait_name: None,
            methods: vec![ApiMethod {
                doc: leading_doc(node, self.src, self.lang),
                signature: signature_text(node, self.src),
                // Out-of-line definitions carry no access specifier; the header
                // owns that. Claiming private here would hide public API.
                visibility: Visibility::Public,
                span: span_of(node, &self.rel_path),
                name,
            }],
            calls,
            module_path: self.module_path.clone(),
        })
    }

    // ── members ────────────────────────────────────────────────────────────

    fn method(&self, node: Node, section: Visibility, forced: bool) -> Option<ApiMethod> {
        let name = child_text(node, "name", self.src).or_else(|| declarator_name(node, self.src))?;
        // C++ groups data members and member-function declarations under the
        // same node kind; only the latter has a parameter list.
        if self.lang == Language::Cpp && !has_parameters(node) {
            return None;
        }
        let vis = if forced { section } else { self.member_visibility(&name, node, section) };
        if !vis.is_included(self.include_private) {
            return None;
        }
        Some(ApiMethod {
            doc: leading_doc(node, self.src, self.lang),
            signature: signature_text(node, self.src),
            visibility: vis,
            span: span_of(node, &self.rel_path),
            name,
        })
    }

    /// Python's data members, which are declared by being assigned.
    ///
    /// There is no `field_declaration` node to list, because the language has no
    /// field declaration. `field_kinds()` was therefore empty for Python and
    /// EVERY Python type came back with no fields at all — a dataclass reported
    /// as having no data, which reads as a fact about the code.
    ///
    /// Two shapes, and taking only one of them would leave most real classes
    /// empty:
    ///
    ///   class Point:          # class body — dataclasses, ClassVars, defaults
    ///       x: int = 0
    ///
    ///   def __init__(self):   # instance attributes, the ordinary case
    ///       self.y = 0
    ///
    /// A `self.y = ...` anywhere in the class IS the declaration of `y`, so
    /// every method is scanned rather than `__init__` alone. Names are deduped
    /// and the first one wins, which prefers the annotated class-body form when
    /// a class has both.
    fn python_fields(&self, member: Node, section: Visibility, item: &mut ApiItem) {
        // (byte offset, name, type). The offset is carried so the list can be
        // put back into source order: the body scan below is a stack, which
        // pops in an order unrelated to how the class reads.
        let mut found: Vec<(usize, String, String)> = Vec::new();

        match member.kind() {
            // Class-level: `x: int = 0`, `x = 0`, `x: int`.
            "expression_statement" => {
                for i in 0..member.named_child_count() as u32 {
                    let Some(n) = member.named_child(i) else { continue };
                    if !matches!(n.kind(), "assignment" | "type_alias_statement") {
                        continue;
                    }
                    let Some(left) = n.child_by_field_name("left") else { continue };
                    if left.kind() != "identifier" {
                        continue;
                    }
                    found.push((
                        left.start_byte(),
                        text(left, self.src).trim().to_string(),
                        normalise_type_text(&child_text(n, "type", self.src).unwrap_or_default()),
                    ));
                }
            }
            // Inside a method: `self.y = 0`, `self.y: int = 0`.
            "function_definition" => {
                let mut stack = vec![member];
                while let Some(n) = stack.pop() {
                    for i in 0..n.named_child_count() as u32 {
                        if let Some(child) = n.named_child(i) {
                            stack.push(child);
                        }
                    }
                    if n.kind() != "assignment" {
                        continue;
                    }
                    let Some(left) = n.child_by_field_name("left") else { continue };
                    if left.kind() != "attribute" {
                        continue;
                    }
                    // `self.y`, and only self: `other.y = 1` assigns to
                    // somebody else's object, not to a member of this one.
                    let is_self = left
                        .child_by_field_name("object")
                        .map(|o| text(o, self.src).trim() == "self")
                        .unwrap_or(false);
                    if !is_self {
                        continue;
                    }
                    let Some(attr) = left.child_by_field_name("attribute") else { continue };
                    found.push((
                        attr.start_byte(),
                        text(attr, self.src).trim().to_string(),
                        normalise_type_text(&child_text(n, "type", self.src).unwrap_or_default()),
                    ));
                }
            }
            _ => return,
        }

        found.sort_by_key(|(at, _, _)| *at);

        for (_, name, ty) in found {
            if name.is_empty() || item.fields.iter().any(|f| f.name == name) {
                continue;
            }
            let visibility = self.member_visibility(&name, member, section);
            if !visibility.is_included(self.include_private) {
                continue;
            }
            item.fields.push(ApiField { name, ty, doc: String::new(), visibility });
        }
    }

    /// Data members of one declaration.
    ///
    /// `section` is the visibility in force here — a C++ `private:` run, a Ruby
    /// `private` marker — and `forced` means the container makes its members
    /// public whatever they say, which is what an interface does.
    ///
    /// Both are honoured per NAME, because in Go visibility is spelled by
    /// capitalising the field and nothing else in the declaration says it.
    fn fields(&self, node: Node, section: Visibility, forced: bool) -> Vec<ApiField> {
        // C# nests a field one level deeper than Java does:
        //
        //   Java  field_declaration -> variable_declarator
        //   C#    field_declaration -> variable_declaration -> variable_declarator
        //
        // and the `type` field sits on that inner node too. Reading only the
        // direct children therefore found no name AND no type, so EVERY plain
        // C# field was dropped — `public int Temperature;` extracted as nothing,
        // while `public int Temperature { get; set; }` worked, because a
        // property_declaration does carry both directly. The type still listed,
        // its properties still listed, and its data members were simply absent:
        // the shape of an answer that looks complete.
        let decl = first_child_of_kind(node, "variable_declaration").unwrap_or(node);
        let ty = normalise_type_text(&child_text(decl, "type", self.src).unwrap_or_default());
        let mut out = Vec::new();

        // One declaration can introduce several names (`int a, b;`).
        let mut names: Vec<String> = Vec::new();
        if let Some(n) = child_text(node, "name", self.src) {
            names.push(n);
        }
        let mut c = decl.walk();
        for child in decl.named_children(&mut c) {
            match child.kind() {
                "variable_declarator" | "field_identifier" | "identifier"
                | "property_identifier" | "field_declarator" => {
                    // A declarator may carry an initialiser (`int count = 0;`).
                    // Prefer the `name` field, then the identifier child, and
                    // only then the whole text — which would otherwise read as
                    // "count = 0".
                    let n = child_text(child, "name", self.src)
                        .or_else(|| {
                            first_child_of_kind(child, "identifier")
                                .map(|id| text(id, self.src).trim().to_string())
                        })
                        .unwrap_or_else(|| text(child, self.src).trim().to_string());
                    if !n.is_empty() && !names.contains(&n) {
                        names.push(n);
                    }
                }
                _ => {}
            }
        }
        for name in names {
            // A field named the same as its own type text is a parse artefact.
            if name == ty || name.is_empty() {
                continue;
            }
            let visibility = if forced {
                section
            } else {
                self.member_visibility(&name, node, section)
            };
            if !visibility.is_included(self.include_private) {
                continue;
            }
            out.push(ApiField { ty: ty.clone(), doc: String::new(), name, visibility });
        }
        out
    }

    /// Declared visibility of a member, honouring each language's own signal.
    fn member_visibility(&self, name: &str, node: Node, section: Visibility) -> Visibility {
        match self.lang {
            Language::Go => go_visibility(name),
            Language::Python => {
                if name.starts_with("__") && !name.ends_with("__") {
                    Visibility::Private
                } else if name.starts_with('_') {
                    Visibility::Crate
                } else {
                    Visibility::Public
                }
            }
            Language::Java | Language::CSharp | Language::Php => {
                let head = self.head_text(node);
                if head.contains("private") {
                    Visibility::Private
                } else if head.contains("protected") || head.contains("internal") {
                    Visibility::Crate
                } else if head.contains("public") {
                    Visibility::Public
                } else {
                    // Java package-private and C# implicit-private are both
                    // narrower than public, and calling them public would pad
                    // every published surface with internals.
                    Visibility::Crate
                }
            }
            // Position-dependent: the last access specifier wins.
            Language::Cpp | Language::Ruby => section,
            _ => {
                let head = self.head_text(node);
                if head.contains("private ") || name.starts_with('#') {
                    Visibility::Private
                } else if head.contains("protected ") || name.starts_with('_') {
                    Visibility::Crate
                } else {
                    Visibility::Public
                }
            }
        }
    }

    /// Text from the start of a declaration up to its name — where modifiers
    /// live in every language here. Cut at the name rather than at a fixed
    /// length so a long return type cannot push `private` out of view.
    fn head_text(&self, node: Node) -> String {
        let whole = text(node, self.src);
        let end = whole
            .find(['(', '{'])
            .unwrap_or_else(|| whole.len().min(160));
        whole[..end].to_string()
    }

    // ── calls ──────────────────────────────────────────────────────────────

    /// Every call made inside this node's body, deduped.
    ///
    /// Resolution is partial in exactly the way the Rust extractor's is: a call
    /// through a variable names the method and admits the receiver is unknown,
    /// rather than guessing an owner and being confidently wrong.
    fn calls_in(&self, node: Node, from: &str) -> Vec<CallEdge> {
        let kinds = self.lang.call_kinds();
        let mut out: Vec<CallEdge> = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            let mut c = n.walk();
            for child in n.named_children(&mut c) {
                stack.push(child);
            }
            let Some((_, field)) = kinds.iter().find(|(k, _)| *k == n.kind()) else { continue };
            let Some(callee) = n.child_by_field_name(field) else { continue };
            // Java and PHP split the receiver and the member into two fields, so
            // the callee field alone is the bare member name and every static
            // call loses its owner. Rejoin them before resolving.
            let joined = n.child_by_field_name("object").map(|o| {
                format!("{}.{}", text(o, self.src).trim(), text(callee, self.src).trim())
            });
            let Some((to, kind)) = (match joined {
                Some(j) => self.resolve_callee(&j),
                None => self.callee_name(callee),
            }) else {
                continue;
            };
            if out.iter().any(|e: &CallEdge| e.to == to && e.kind == kind) {
                continue;
            }
            out.push(CallEdge {
                from: from.to_string(),
                to,
                kind,
                span: span_of(n, &self.rel_path),
            });
        }
        out
    }

    fn callee_name(&self, node: Node) -> Option<(String, CallKind)> {
        self.resolve_callee(text(node, self.src))
    }

    fn resolve_callee(&self, raw: &str) -> Option<(String, CallKind)> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > 120 {
            return None;
        }
        // `Foo::bar` is a qualified call in every language that spells it that
        // way, with no inference needed.
        if raw.contains("::") {
            let segs: Vec<&str> = raw.split("::").filter(|s| !s.is_empty()).collect();
            let n = segs.len();
            if n >= 2 {
                return Some((format!("{}::{}", segs[n - 2], segs[n - 1]), CallKind::Path));
            }
        }
        if let Some((recv, member)) = raw.rsplit_once('.') {
            let member = member.trim();
            if member.is_empty() || !is_ident(member) {
                return None;
            }
            let recv = recv.trim().rsplit('.').next().unwrap_or(recv).trim();
            // In languages that capitalise types and lowercase locals, a
            // capitalised single-segment receiver is a type, so the edge can
            // name its owner. Elsewhere — and for anything more complex than a
            // bare identifier — the receiver's type needs inference we do not
            // perform, so the honest edge is the bare method name.
            if self.lang.types_are_capitalised()
                && is_ident(recv)
                && recv.chars().next().is_some_and(|c| c.is_uppercase())
            {
                return Some((format!("{recv}::{member}"), CallKind::Path));
            }
            return Some((member.to_string(), CallKind::Method));
        }
        if !is_ident(raw) {
            return None;
        }
        Some((raw.to_string(), CallKind::Path))
    }

    // ── functions ──────────────────────────────────────────────────────────

    fn function_item(&mut self, node: Node) -> Option<ApiItem> {
        let name = child_text(node, "name", self.src).or_else(|| declarator_name(node, self.src))?;
        if self.lang == Language::Cpp && !has_parameters(node) {
            return None;
        }
        let vis = match self.lang {
            Language::Go => go_visibility(&name),
            Language::Python if name.starts_with('_') => Visibility::Private,
            _ => Visibility::Public,
        };
        if !vis.is_included(self.include_private) {
            return None;
        }
        let mut item = self.base_item(ItemKind::Function, name.clone(), node);
        item.signature = signature_text(node, self.src);
        item.visibility = vis;
        item.calls = self.calls_in(node, &name);
        Some(item)
    }

    fn base_item(&self, kind: ItemKind, name: String, node: Node) -> ApiItem {
        let visibility = match self.lang {
            Language::Go => go_visibility(&name),
            _ if name.starts_with('_') => Visibility::Private,
            _ => Visibility::Public,
        };
        ApiItem {
            kind,
            doc: leading_doc(node, self.src, self.lang),
            signature: signature_text(node, self.src),
            module_path: self.module_path.clone(),
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: String::new(),
            traits_impl: vec![],
            origin: String::new(),
            visibility,
            span: span_of(node, &self.rel_path),
            // Built in exactly one place so the tag cannot be set on one path
            // and forgotten on another — which is how the previous `ast_only`
            // claim spent a release as a doc comment with no field behind it.
            confidence: Confidence::NameResolved,
            language: self.lang.label().to_string(),
            calls: vec![],
            name,
        }
    }

    /// Ruby writes `class Foo` with the name as a `constant`, not a `name` field.
    fn ruby_class_name(&self, node: Node) -> Option<String> {
        if self.lang != Language::Ruby {
            return None;
        }
        let mut c = node.walk();
        let found = node
            .named_children(&mut c)
            .find(|n| matches!(n.kind(), "constant" | "scope_resolution"));
        found.map(|n| text(n, self.src).trim().to_string())
    }
}

// ── usage mining ────────────────────────────────────────────────────────────

/// Node kinds that count as one statement — a unit of code worth showing as an
/// example of calling something.
///
/// Deliberately a superset across grammars rather than nine separate tables: the
/// caller filters by "does this snippet mention an indexed name", which discards
/// anything irrelevant, so over-collecting here costs nothing and under-
/// collecting silently loses examples.
const STATEMENT_KINDS: &[&str] = &[
    // Shared across nearly every grammar.
    "expression_statement",
    "return_statement",
    "if_statement",
    "for_statement",
    "while_statement",
    "assignment",
    "call",
    // JS / TS
    "lexical_declaration",
    "variable_declaration",
    // Go
    "short_var_declaration",
    "assignment_statement",
    "var_declaration",
    "call_expression",
    // Java / C#
    "local_variable_declaration",
    "local_declaration_statement",
    // C / C++
    "declaration",
    // PHP
    "echo_statement",
];

/// Kinds that DEFINE something rather than call it.
///
/// The Rust path already learned this: harvesting implementation code as
/// "usage" once offered a `Debug` impl body and a trait's default method as
/// examples of using the type in their signature.
const DEFINITION_KINDS: &[&str] = &[
    "function_definition", "function_declaration", "method_definition",
    "method_declaration", "class_definition", "class_declaration",
    "class_specifier", "struct_specifier", "interface_declaration",
    "type_declaration", "enum_declaration", "constructor_declaration",
    "namespace_declaration", "namespace_definition", "module", "class",
    "method", "impl_item", "trait_declaration",
];

/// Line spans (1-based, inclusive) of every statement in a non-Rust file.
///
/// Mirrors what `syn` gives the Rust path, so `usage::harvest` can mine any
/// language through the same pipeline instead of being Rust-only — which is
/// what it was, silently, while advertising worked syntax as a feature.
pub fn statement_spans(source: &str, lang: Language) -> Vec<(usize, usize)> {
    let mut parser = TsParser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else { return Vec::new() };

    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if !STATEMENT_KINDS.contains(&n.kind()) {
            continue;
        }
        // A statement that CONTAINS a definition is a definition's wrapper, not
        // a call site.
        if n.named_child_count() > 0 {
            let mut c2 = n.walk();
            if n.named_children(&mut c2).any(|k| DEFINITION_KINDS.contains(&k.kind())) {
                continue;
            }
        }
        out.push((n.start_position().row + 1, n.end_position().row + 1));
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ── shared helpers ──────────────────────────────────────────────────────────

/// Where a type's members live. Grammars disagree on the field name, and a miss
/// here yields a type with no members and no error.
fn type_body<'t>(node: Node<'t>, lang: Language) -> Option<Node<'t>> {
    for field in ["body", "class_body", "declaration_list", "field_declaration_list"] {
        if let Some(b) = node.child_by_field_name(field) {
            return Some(b);
        }
    }
    let mut c = node.walk();
    let found = node.named_children(&mut c).find(|n| {
        matches!(
            n.kind(),
            "class_body"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_body"
                | "interface_body"
                | "enum_member_declaration_list"
                | "body_statement"
                | "block"
        )
    });
    if found.is_some() {
        return found;
    }
    // Ruby puts members straight in the class node.
    (lang == Language::Ruby).then_some(node)
}

/// Every type name mentioned inside a heritage/base/receiver clause.
fn type_names_in(node: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        // A dotted base (`unittest.TestCase`, `ns.IShape`) names ONE type. Walking
        // into it collects the package as if it were a second base class, which
        // is how `class T(unittest.TestCase)` came out implementing both
        // `TestCase` and `unittest`.
        if matches!(n.kind(), "attribute" | "member_expression" | "scoped_identifier") {
            let last = n
                .child_by_field_name("attribute")
                .or_else(|| n.child_by_field_name("property"))
                .or_else(|| n.child_by_field_name("name"))
                .unwrap_or(n);
            let t = text(last, src).trim();
            if is_ident(t) && !out.contains(&t.to_string()) {
                out.push(t.to_string());
            }
            continue;
        }
        if matches!(
            n.kind(),
            "type_identifier"
                | "identifier"
                | "constant"
                | "qualified_name"
                | "name"
                | "generic_name"
                | "scoped_type_identifier"
                | "type_constraint"
        ) {
            let t = text(n, src).trim();
            // Strip generic arguments: `List<Foo>` is a `List`.
            let t = t.split('<').next().unwrap_or(t).trim();
            let t = t.rsplit(['.', ':']).next().unwrap_or(t).trim();
            if is_ident(t) && !out.contains(&t.to_string()) {
                out.push(t.to_string());
            }
            continue;
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
    out
}

/// Go's visibility rule, which is the identifier's own first letter.
fn go_visibility(name: &str) -> Visibility {
    match name.chars().next() {
        Some(c) if c.is_uppercase() => Visibility::Public,
        _ => Visibility::Crate,
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn has_parameters(node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "parameter_list" {
            return true;
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            // Do not descend into a body looking for a parameter list.
            if child.kind() != "compound_statement" {
                stack.push(child);
            }
        }
    }
    false
}

/// C-family declarations hide the name inside nested declarators.
fn declarator_name(node: Node, src: &str) -> Option<String> {
    let mut n = node.child_by_field_name("declarator")?;
    for _ in 0..6 {
        match n.kind() {
            "identifier" | "field_identifier" | "type_identifier" => {
                return Some(text(n, src).trim().to_string())
            }
            "qualified_identifier" => return child_text(n, "name", src),
            _ => match n.child_by_field_name("declarator") {
                Some(next) => n = next,
                None => return None,
            },
        }
    }
    None
}

/// The declaration, without the body.
///
/// Cut at the body NODE, not at the first `{` or `:`. Both appear inside
/// declarations here — TypeScript writes parameter and return types with `:`,
/// which truncated `async get(id: number): Promise<User>` down to `async get(id`.
fn signature_text(node: Node, src: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .or_else(|| {
            let mut c = node.walk();
            let block = node
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "compound_statement" | "block" | "statement_block"));
            block.map(|n| n.start_byte())
        })
        .unwrap_or_else(|| node.end_byte());
    let start = node.start_byte();
    let head = if end > start && end <= src.len() { &src[start..end] } else { text(node, src) };

    head.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        // Python `def f(x):` keeps its colon; drop the dangling separator.
        .trim_end_matches(':')
        .trim()
        .to_string()
}

/// Python docstrings live inside the body; everything else precedes the node.
fn leading_doc(node: Node, src: &str, lang: Language) -> String {
    if lang == Language::Python {
        if let Some(body) = node.child_by_field_name("body") {
            if let Some(first) = body.named_child(0) {
                if first.kind() == "expression_statement" {
                    let t = text(first, src).trim();
                    if t.starts_with("\"\"\"") || t.starts_with("'''") {
                        return t.trim_matches(|c| c == '"' || c == '\'').trim().to_string();
                    }
                }
            }
        }
    }
    let Some(prev) = node.prev_named_sibling() else { return String::new() };
    if !matches!(prev.kind(), "comment" | "line_comment" | "block_comment" | "doc_comment") {
        return String::new();
    }
    text(prev, src)
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("/**")
                .trim_end_matches("*/")
                .trim_start_matches("///")
                .trim_start_matches("//")
                .trim_start_matches('#')
                .trim_start_matches('*')
                .trim()
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn span_of(node: Node, rel_path: &str) -> Option<SourceSpan> {
    Some(SourceSpan {
        file: rel_path.to_string(),
        // tree-sitter rows are 0-based; editors and our Rust spans are 1-based.
        line: node.start_position().row + 1,
    })
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// A declared type as a reader would write it, without the syntax that attached
/// it to a name.
///
/// In TypeScript the `type` field of a property is a `type_annotation` node,
/// whose text INCLUDES the colon — so every annotated field rendered as
/// `width: : number`. Rendering is where that shows, but the value is also what
/// gets indexed and matched on, so it is fixed at the source.
///
/// A leading `::` is left alone: `::std::string` is a C++ type naming the global
/// scope, not a separator plus a type.
fn normalise_type_text(raw: &str) -> String {
    let t = raw.trim();
    match t.strip_prefix(':') {
        Some(rest) if !rest.starts_with(':') => rest.trim().to_string(),
        _ => t.to_string(),
    }
}

/// First named child of a given kind. Grammars differ in how deeply they nest
/// the same construct, so some lookups have to be by kind rather than by field.
fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    // Indexed rather than via named_children: the cursor that iterator borrows
    // cannot outlive this frame, while the Node it yields must.
    (0..node.named_child_count() as u32)
        .filter_map(|i| node.named_child(i))
        .find(|n| n.kind() == kind)
}

fn child_text(node: Node, field: &str, src: &str) -> Option<String> {
    let c = node.child_by_field_name(field)?;
    let t = text(c, src).trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod tests;
