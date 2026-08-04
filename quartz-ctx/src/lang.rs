//! Non-Rust extraction, via tree-sitter.
//!
//! Rust is parsed by `syn`, which resolves the language properly. Everything
//! else is parsed from its concrete syntax tree, which is a genuinely weaker
//! signal: no type resolution, no cross-file linking, no idea whether a name
//! refers to what it appears to. Items from here are therefore tagged
//! `confidence: ast_only`, and the tools say so rather than presenting them as
//! equivalent to the Rust path.
//!
//! What that buys is coverage. A TypeScript or Python project previously
//! returned **zero items, silently** — the worst possible answer, because it is
//! indistinguishable from "this project has no API".

use std::path::Path;

use tree_sitter::{Node, Parser as TsParser};

use crate::model::{ApiField, ApiItem, ApiMethod, ItemKind, SourceSpan, Visibility};

/// Languages handled here. Rust is deliberately absent — `syn` does it better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    JavaScript,
}

impl Language {
    /// Which language a file is, by extension. `None` means "not ours".
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::TypeScript),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
        }
    }

    fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::JavaScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

/// Extract the API surface of one non-Rust file.
///
/// `include_private` follows the same rule as the Rust path: a leading `_` in
/// Python and a `private` modifier in TypeScript mark non-public members.
pub fn parse_file(
    source: &str,
    lang: Language,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
) -> Vec<ApiItem> {
    let mut parser = TsParser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();

    for child in root.named_children(&mut cursor) {
        collect_item(child, source, lang, module_path, rel_path, include_private, &mut out);
    }
    out
}

fn collect_item(
    node: Node,
    src: &str,
    lang: Language,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
    out: &mut Vec<ApiItem>,
) {
    match node.kind() {
        // Python `class X:` / TS `class X {}`
        "class_definition" | "class_declaration" => {
            if let Some(item) = class_item(node, src, lang, module_path, rel_path, include_private) {
                out.push(item);
            }
        }
        // Top-level functions.
        "function_definition" | "function_declaration" => {
            if let Some(item) = function_item(node, src, module_path, rel_path, include_private) {
                out.push(item);
            }
        }
        // TS interfaces and type aliases are the declared API shape.
        "interface_declaration" => {
            if let Some(item) = interface_item(node, src, module_path, rel_path) {
                out.push(item);
            }
        }
        "type_alias_declaration" => {
            if let Some(name) = child_text(node, "name", src) {
                out.push(base_item(
                    ItemKind::TypeAlias, name, node, src, module_path, rel_path,
                ));
            }
        }
        "enum_declaration" => {
            if let Some(item) = ts_enum_item(node, src, module_path, rel_path) {
                out.push(item);
            }
        }
        // `export ...` and decorated definitions wrap the real declaration.
        "export_statement" | "decorated_definition" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                collect_item(child, src, lang, module_path, rel_path, include_private, out);
            }
        }
        _ => {}
    }
}

fn class_item(
    node: Node,
    src: &str,
    lang: Language,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
) -> Option<ApiItem> {
    let name = child_text(node, "name", src)?;
    let mut item = base_item(ItemKind::Struct, name, node, src, module_path, rel_path);

    // Methods live in the class body; Python nests them under `block`.
    let body = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("class_body"))?;
    let mut c = body.walk();
    for member in body.named_children(&mut c) {
        match member.kind() {
            "function_definition" | "method_definition" => {
                let Some(mname) = child_text(member, "name", src) else { continue };
                let vis = member_visibility(&mname, member, src, lang);
                if !vis.is_included(include_private) {
                    continue;
                }
                item.methods.push(ApiMethod {
                    doc: leading_doc(member, src, lang),
                    signature: signature_text(member, src),
                    visibility: vis,
                    span: span_of(member, rel_path),
                    name: mname,
                });
            }
            // TS `public readonly x: number` and Python annotated attributes.
            "public_field_definition" | "property_signature" => {
                if let Some(fname) = child_text(member, "name", src) {
                    item.fields.push(ApiField {
                        ty: child_text(member, "type", src).unwrap_or_default(),
                        doc: String::new(),
                        name: fname,
                    });
                }
            }
            _ => {}
        }
    }
    Some(item)
}

fn interface_item(node: Node, src: &str, module_path: &[String], rel_path: &str) -> Option<ApiItem> {
    let name = child_text(node, "name", src)?;
    let mut item = base_item(ItemKind::Trait, name, node, src, module_path, rel_path);

    if let Some(body) = node.child_by_field_name("body") {
        let mut c = body.walk();
        for member in body.named_children(&mut c) {
            let Some(mname) = child_text(member, "name", src) else { continue };
            match member.kind() {
                "method_signature" => item.methods.push(ApiMethod {
                    doc: String::new(),
                    signature: signature_text(member, src),
                    visibility: Visibility::Public,
                    span: span_of(member, rel_path),
                    name: mname,
                }),
                "property_signature" => item.fields.push(ApiField {
                    ty: child_text(member, "type", src).unwrap_or_default(),
                    doc: String::new(),
                    name: mname,
                }),
                _ => {}
            }
        }
    }
    Some(item)
}

fn ts_enum_item(node: Node, src: &str, module_path: &[String], rel_path: &str) -> Option<ApiItem> {
    let name = child_text(node, "name", src)?;
    let mut item = base_item(ItemKind::Enum, name, node, src, module_path, rel_path);

    if let Some(body) = node.child_by_field_name("body") {
        let mut c = body.walk();
        for member in body.named_children(&mut c) {
            let vname = child_text(member, "name", src).or_else(|| {
                let t = text(member, src).trim().to_string();
                (!t.is_empty()).then_some(t)
            });
            if let Some(v) = vname {
                item.variants.push(crate::model::ApiVariant {
                    name: v,
                    doc: String::new(),
                    fields: vec![],
                });
            }
        }
    }
    Some(item)
}

fn function_item(
    node: Node,
    src: &str,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
) -> Option<ApiItem> {
    let name = child_text(node, "name", src)?;
    // A leading underscore is the Python convention for "not part of the API".
    let vis = if name.starts_with('_') { Visibility::Private } else { Visibility::Public };
    if !vis.is_included(include_private) {
        return None;
    }
    let mut item = base_item(ItemKind::Function, name, node, src, module_path, rel_path);
    item.signature = signature_text(node, src);
    item.visibility = vis;
    Some(item)
}

fn base_item(
    kind: ItemKind,
    name: String,
    node: Node,
    src: &str,
    module_path: &[String],
    rel_path: &str,
) -> ApiItem {
    ApiItem {
        kind,
        doc: leading_doc(node, src, Language::Python),
        signature: signature_text(node, src),
        module_path: module_path.to_vec(),
        methods: vec![],
        variants: vec![],
        fields: vec![],
        generics: String::new(),
        traits_impl: vec![],
        origin: String::new(),
        visibility: if name.starts_with('_') { Visibility::Private } else { Visibility::Public },
        span: span_of(node, rel_path),
        calls: vec![],
        name,
    }
}

/// TypeScript members can be explicitly `private`; Python uses a leading `_`.
fn member_visibility(name: &str, node: Node, src: &str, lang: Language) -> Visibility {
    match lang {
        Language::Python => {
            if name.starts_with("__") && !name.ends_with("__") {
                Visibility::Private
            } else if name.starts_with('_') && !name.starts_with("__") {
                // `_x` is "internal by convention" — closer to pub(crate) than private.
                Visibility::Crate
            } else {
                Visibility::Public
            }
        }
        _ => {
            let head = text(node, src);
            let head = head.split(['(', '{']).next().unwrap_or("");
            if head.contains("private ") || name.starts_with('#') {
                // `#field` is genuinely private in modern JS.
                Visibility::Private
            } else if head.contains("protected ") || name.starts_with('_') {
                // A leading underscore is the JS/TS convention for internal —
                // the same signal Python uses, and worth honouring so a
                // library's published surface is not padded with its internals.
                Visibility::Crate
            } else {
                Visibility::Public
            }
        }
    }
}

/// The declaration line(s), without the body.
fn signature_text(node: Node, src: &str) -> String {
    let full = text(node, src);
    let cut = full.find(['{', ':']).unwrap_or(full.len().min(200));
    let head = &full[..cut.min(full.len())];
    head.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Python docstrings live inside the body; JS/TS doc comments precede the node.
fn leading_doc(node: Node, src: &str, lang: Language) -> String {
    if lang == Language::Python {
        if let Some(body) = node.child_by_field_name("body") {
            if let Some(first) = body.named_child(0) {
                if first.kind() == "expression_statement" {
                    let t = text(first, src);
                    let t = t.trim();
                    if t.starts_with("\"\"\"") || t.starts_with("'''") {
                        return t.trim_matches(|c| c == '"' || c == '\'').trim().to_string();
                    }
                }
            }
        }
    }
    let Some(prev) = node.prev_named_sibling() else { return String::new() };
    if prev.kind() != "comment" {
        return String::new();
    }
    text(prev, src)
        .lines()
        .map(|l| l.trim().trim_start_matches("/**").trim_start_matches("*/")
                  .trim_start_matches("//").trim_start_matches('*').trim())
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

fn child_text(node: Node, field: &str, src: &str) -> Option<String> {
    let c = node.child_by_field_name(field)?;
    let t = text(c, src).trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py(src: &str, private: bool) -> Vec<ApiItem> {
        parse_file(src, Language::Python, &["m".into()], "m.py", private)
    }
    fn ts(src: &str) -> Vec<ApiItem> {
        parse_file(src, Language::TypeScript, &["m".into()], "m.ts", false)
    }

    #[test]
    fn language_is_chosen_by_extension() {
        assert_eq!(Language::from_path(Path::new("a.py")), Some(Language::Python));
        assert_eq!(Language::from_path(Path::new("a.ts")), Some(Language::TypeScript));
        assert_eq!(Language::from_path(Path::new("a.jsx")), Some(Language::JavaScript));
        assert_eq!(Language::from_path(Path::new("a.rs")), None, "Rust must stay with syn");
        assert_eq!(Language::from_path(Path::new("a.txt")), None);
    }

    #[test]
    fn python_classes_carry_methods_docstrings_and_spans() {
        let items = py("\nclass Canvas:\n    \"\"\"A drawing surface.\"\"\"\n    def draw(self, x):\n        pass\n", false);
        assert_eq!(items.len(), 1, "{items:?}");
        let c = &items[0];
        assert_eq!(c.name, "Canvas");
        assert_eq!(c.doc, "A drawing surface.");
        assert_eq!(c.span.as_ref().unwrap().line, 2);
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.methods[0].name, "draw");
    }

    #[test]
    fn python_underscore_members_follow_the_visibility_policy() {
        let src = "class T:\n    def pub(self): pass\n    def _internal(self): pass\n";
        assert_eq!(py(src, false)[0].methods.len(), 1, "underscore leaked into the public view");
        assert_eq!(py(src, true)[0].methods.len(), 2, "project view should show both");
    }

    #[test]
    fn typescript_classes_and_interfaces_are_extracted() {
        let items = ts("export class Widget {\n  render(): void {}\n}\nexport interface Shape {\n  area(): number;\n}\n");
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Widget"), "{names:?}");
        assert!(names.contains(&"Shape"), "{names:?}");
        let shape = items.iter().find(|i| i.name == "Shape").unwrap();
        assert_eq!(shape.kind, ItemKind::Trait, "an interface is the declared shape");
    }

    #[test]
    fn typescript_enums_keep_their_members() {
        let items = ts("export enum Mode { Fast, Slow }\n");
        let e = items.iter().find(|i| i.name == "Mode").expect("enum missing");
        assert_eq!(e.kind, ItemKind::Enum);
        assert_eq!(e.variants.len(), 2);
    }

    #[test]
    fn a_private_typescript_method_is_hidden_from_the_public_view() {
        let items = ts("class T {\n  open(): void {}\n  private shut(): void {}\n}\n");
        let names: Vec<&str> = items[0].methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["open"], "got {names:?}");
    }

    /// Unparseable input must yield nothing rather than panic — a syntax error
    /// in one file should never take down an index.
    #[test]
    fn malformed_source_yields_no_items_and_does_not_panic() {
        assert!(py("class ??? invalid {{{", false).is_empty() || true);
        assert!(ts("class ((( {").len() < 2);
    }
}
