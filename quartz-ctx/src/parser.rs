use std::path::Path;

use anyhow::Result;
use quote::quote;
use syn::visit::Visit;
use walkdir::WalkDir;

use crate::model::*;

// ── Entry point ──────────────────────────────────────────────────────────────

/// How much of a codebase to extract.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    /// Include items that are not `pub`.
    ///
    /// A library publishes its API through `pub`, so the default (false) is the
    /// right view of one. An application or binary crate publishes almost
    /// nothing, so extracting only `pub` returns a nearly empty index for it —
    /// enable this to index a project rather than a library.
    pub include_private: bool,
}

/// Recursively parse all `.rs` files under `dir` and return every public API item found.
pub fn parse_dir(dir: &Path) -> Result<Vec<ApiItem>> {
    parse_dir_with(dir, ParseOptions::default())
}

/// Recursively parse all `.rs` files under `dir` using explicit options.
pub fn parse_dir_with(dir: &Path, opts: ParseOptions) -> Result<Vec<ApiItem>> {
    let mut all_items: Vec<ApiItem> = Vec::new();
    let mut orphan_impls: Vec<PendingImpl> = Vec::new();

    for entry in WalkDir::new(dir)
        .into_iter()
        // Prune whole directories rather than filtering files, so we never
        // descend into build output or vendored dependencies at all.
        .filter_entry(|e| !is_excluded_dir(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let p = e.path();
            p.extension().map_or(false, |ext| ext == "rs")
                || crate::lang::Language::from_path(p).is_some()
        })
    {
        let path = entry.path();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warn: could not read {}: {}", path.display(), e);
                continue;
            }
        };

        // Non-Rust files go through tree-sitter. Their items are AST-only —
        // no type resolution — which is a weaker signal than the `syn` path and
        // is labelled as such, but vastly better than the previous behaviour of
        // returning nothing at all and looking like an empty project.
        if let Some(lang) = crate::lang::Language::from_path(path) {
            let module_path = derive_module_path(dir, path);
            let rel_path = path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            all_items.extend(crate::lang::parse_file(
                &content,
                lang,
                &module_path,
                &rel_path,
                opts.include_private,
            ));
            continue;
        }

        match syn::parse_file(&content) {
            Ok(file) => {
                let module_path = derive_module_path(dir, path);
                let rel_path = path
                    .strip_prefix(dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let (items, leftovers) =
                    extract_items(&file, &module_path, &rel_path, opts.include_private);
                all_items.extend(items);
                orphan_impls.extend(leftovers);
            }
            Err(e) => {
                eprintln!("warn: could not parse {}: {}", path.display(), e);
            }
        }
    }

    // Global second pass: attach impl blocks whose owning type lives in a
    // DIFFERENT file. Quartz spreads `impl Canvas` across 9 files
    // (canvas/actions.rs, conditions.rs, physics.rs, ...) — the old per-file
    // attachment silently discarded all of them, serving Canvas with zero
    // methods and gutting plugin surfaces.
    // When several indexed types share a name, attach to the candidate whose
    // module path shares the longest prefix with the impl's own module path.
    // Taking the first name match instead lets `editor::State` absorb the
    // methods of `engine::State` — silently, and in the direction that makes
    // the wrong type look richer.
    for pending in orphan_impls {
        let target = all_items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.name == pending.self_ty)
            .max_by_key(|(_, i)| module_prefix_overlap(&i.module_path, &pending.module_path))
            .map(|(idx, _)| idx);

        let Some(idx) = target else { continue };
        let owner = &mut all_items[idx];

        for method in pending.methods {
            // Dedupe by name: the same method can appear via re-parse or
            // cfg-gated duplicate definitions.
            if !owner.methods.iter().any(|m| m.name == method.name) {
                owner.methods.push(method);
            }
        }
        for call in pending.calls {
            if !owner.calls.iter().any(|c| c.from == call.from && c.to == call.to) {
                owner.calls.push(call);
            }
        }
        if let Some(tr) = pending.trait_name {
            if !tr.is_empty() && !owner.traits_impl.contains(&tr) {
                owner.traits_impl.push(tr);
            }
        }
        // Types from outside the scanned roots (e.g. std types) stay unattached.
    }

    Ok(all_items)
}

/// Parse multiple source roots, tagging every item with its origin slug.
/// Order matters: the FIRST source is the primary engine — lookups that match
/// multiple origins prefer it.
pub fn load_sources(sources: &[(std::path::PathBuf, String)]) -> Result<Vec<ApiItem>> {
    let with_policy: Vec<(std::path::PathBuf, String, bool)> = sources
        .iter()
        .map(|(p, t)| (p.clone(), t.clone(), false))
        .collect();
    load_sources_with(&with_policy)
}

/// Load several roots, each with its own extraction policy.
///
/// The policy is per root, not per server: a workspace can hold a library whose
/// `pub` surface is the whole story alongside an application whose structure is
/// almost entirely private, and both must be served correctly at once.
pub fn load_sources_with(
    sources: &[(std::path::PathBuf, String, bool)],
) -> Result<Vec<ApiItem>> {
    let mut all = Vec::new();
    for (path, tag, include_private) in sources {
        let mut items = parse_dir_with(path, ParseOptions { include_private: *include_private })?;
        for item in &mut items {
            item.origin = tag.clone();
        }
        all.extend(items);
    }
    Ok(all)
}

/// Directory names never worth indexing: build output, dependency caches and
/// virtualenvs. These hold generated and vendored code that is not the project's
/// own API.
///
/// Without this, pointing quartz-ctx at a project *root* rather than its `src`
/// silently ingests build artefacts — scanning `quartz_forge` picked up
/// `target/debug/build/*/out/*.rs` and reported 1,239 items where the real API
/// surface is 99. The plan's operational default ("exclude generated and vendor
/// directories") lives here.
const EXCLUDED_DIRS: &[&str] = &[
    "target",       // cargo
    "node_modules", // npm/yarn/pnpm
    "vendor",       // vendored deps
    "dist",
    "build",
    "out",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".next",
    ".svelte-kit",
];

/// Count the `.rs` files `parse_dir` would actually read under `dir`.
/// Diagnostics must apply the same exclusions as the parser, or `selfcheck`
/// reports a file count the parser never touches.
pub fn count_source_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let p = e.path();
            p.extension().and_then(|x| x.to_str()) == Some("rs")
                || crate::lang::Language::from_path(p).is_some()
        })
        .count()
}

/// True when this entry is a directory that should not be descended into.
/// The walk root itself is never excluded — asking for `./target` explicitly is
/// a deliberate choice and should be honoured.
pub fn is_excluded_walk_entry(entry: &walkdir::DirEntry) -> bool {
    is_excluded_dir(entry)
}

fn is_excluded_dir(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .map(|n| EXCLUDED_DIRS.contains(&n) || n.starts_with(".cargo"))
        .unwrap_or(false)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Convert a file path into a Rust module path relative to `base`.
/// `src/game_object/sprite.rs` → `["game_object", "sprite"]`
fn derive_module_path(base: &Path, file: &Path) -> Vec<String> {
    let relative = file.strip_prefix(base).unwrap_or(file);
    relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .filter(|s| s != "mod" && s != "lib" && s != "main")
        .collect()
}

fn is_public(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

/// Number of leading module segments two paths share.
fn module_prefix_overlap(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Classify a syn visibility into the canonical `Visibility` ladder.
fn visibility_of(vis: &syn::Visibility) -> Visibility {
    match vis {
        syn::Visibility::Public(_) => Visibility::Public,
        syn::Visibility::Restricted(r) => {
            if r.path.is_ident("crate") {
                Visibility::Crate
            } else {
                Visibility::Restricted
            }
        }
        syn::Visibility::Inherited => Visibility::Private,
    }
}

/// Build a `file:line` span for a syn node.
///
/// Always pass the declaration's IDENTIFIER, not the item. A `syn` item span
/// starts at its first attribute, so `#[derive(Clone)] pub struct Canvas` would
/// cite the derive line rather than the declaration. The ident lands exactly on
/// the name.
///
/// `file` is relative to the scanned root with forward slashes, so the same item
/// reads identically regardless of platform or absolute checkout location.
fn span_of<T: syn::spanned::Spanned>(node: &T, file: &str) -> Option<SourceSpan> {
    let line = node.span().start().line;
    if line == 0 {
        return None;
    }
    Some(SourceSpan { file: file.to_string(), line })
}

fn extract_docs(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter_map(|a| {
            if !a.path().is_ident("doc") {
                return None;
            }
            if let syn::Meta::NameValue(nv) = &a.meta {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    return Some(s.value().trim().to_string());
                }
            }
            None
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn type_to_string(ty: &syn::Type) -> String {
    // quote! preserves spaces; clean them for readability
    quote!(#ty)
        .to_string()
        .replace(" :: ", "::")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
}

fn generics_to_string(generics: &syn::Generics) -> String {
    if generics.params.is_empty() {
        String::new()
    } else {
        quote!(#generics).to_string()
    }
}

fn sig_to_string(sig: &syn::Signature) -> String {
    quote!(#sig)
        .to_string()
        .replace(" :: ", "::")
        .replace("< ", "<")
        .replace(" >", ">")
}

// ── Visitor ──────────────────────────────────────────────────────────────────

fn extract_items(
    file: &syn::File,
    module_path: &[String],
    rel_path: &str,
    include_private: bool,
) -> (Vec<ApiItem>, Vec<PendingImpl>) {
    let mut visitor = ApiVisitor {
        items: Vec::new(),
        module_path: module_path.to_vec(),
        pending_impls: Vec::new(),
        rel_path: rel_path.to_string(),
        include_private,
    };
    visitor.visit_file(file);
    let leftovers = visitor.flush_impls();
    (visitor.items, leftovers)
}

struct PendingImpl {
    self_ty: String,
    trait_name: Option<String>,
    methods: Vec<ApiMethod>,
    /// Calls made from this impl's method bodies, attached to the owning type
    /// alongside the methods themselves.
    calls: Vec<CallEdge>,
    /// Module path the `impl` block itself was found in — used to pick the
    /// right owner when several indexed types share a name.
    module_path: Vec<String>,
}

struct ApiVisitor {
    items: Vec<ApiItem>,
    module_path: Vec<String>,
    /// `impl` blocks collected before the owning type may have been seen.
    pending_impls: Vec<PendingImpl>,
    /// Path of the file being visited, relative to the scanned root.
    rel_path: String,
    /// When false, only `pub` items are kept — the library view. When true,
    /// every visibility is kept, which is the only useful view of an
    /// application or binary crate.
    include_private: bool,
}

impl ApiVisitor {
    /// Whether an item with this visibility should be extracted.
    fn keep(&self, vis: &syn::Visibility) -> bool {
        visibility_of(vis).is_included(self.include_private)
    }
}

impl ApiVisitor {
    /// Attach collected impl blocks to items in THIS file (fast path).
    /// Impls whose owning type lives in another file are RETURNED so the
    /// caller can attach them in a global pass — never discarded.
    fn flush_impls(&mut self) -> Vec<PendingImpl> {
        let mut leftovers = Vec::new();
        for pending in self.pending_impls.drain(..) {
            if let Some(owner) = self.items.iter_mut().find(|i| i.name == pending.self_ty) {
                for method in pending.methods {
                    if !owner.methods.iter().any(|m| m.name == method.name) {
                        owner.methods.push(method);
                    }
                }
                for call in pending.calls {
                    if !owner.calls.iter().any(|c| c.from == call.from && c.to == call.to) {
                        owner.calls.push(call);
                    }
                }
                if let Some(tr) = pending.trait_name {
                    if !tr.is_empty() && !owner.traits_impl.contains(&tr) {
                        owner.traits_impl.push(tr);
                    }
                }
            } else {
                leftovers.push(pending);
            }
        }
        leftovers
    }
}

impl<'ast> Visit<'ast> for ApiVisitor {
    // ── struct ────────────────────────────────────────────────────────────────
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if !self.keep(&node.vis) {
            syn::visit::visit_item_struct(self, node);
            return;
        }

        let fields: Vec<ApiField> = match &node.fields {
            syn::Fields::Named(named) => named
                .named
                .iter()
                .filter(|f| self.keep(&f.vis))
                .map(|f| ApiField {
                    name: f.ident.as_ref().map_or("_".into(), |i| i.to_string()),
                    ty: type_to_string(&f.ty),
                    doc: extract_docs(&f.attrs),
                })
                .collect(),
            _ => vec![],
        };

        let name = node.ident.to_string();
        let generics = generics_to_string(&node.generics);
        let sig = if fields.is_empty() {
            format!("pub struct {}{};", name, generics)
        } else {
            let field_strs: Vec<String> = fields.iter()
                .map(|f| format!("    pub {}: {},", f.name, f.ty))
                .collect();
            format!("pub struct {}{} {{\n{}\n}}", name, generics, field_strs.join("\n"))
        };

        self.items.push(ApiItem {
            kind: ItemKind::Struct,
            name,
            doc: extract_docs(&node.attrs),
            signature: sig,
            module_path: self.module_path.clone(),
            methods: vec![],
            variants: vec![],
            fields,
            generics,
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.ident, &self.rel_path),
            calls: Vec::new(),
        });

        syn::visit::visit_item_struct(self, node);
    }

    // ── enum ──────────────────────────────────────────────────────────────────
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if !self.keep(&node.vis) {
            syn::visit::visit_item_enum(self, node);
            return;
        }

        let variants: Vec<ApiVariant> = node
            .variants
            .iter()
            .map(|v| {
                let fields = match &v.fields {
                    syn::Fields::Named(named) => named
                        .named
                        .iter()
                        .map(|f| ApiField {
                            name: f.ident.as_ref().map_or("_".into(), |i| i.to_string()),
                            ty: type_to_string(&f.ty),
                            doc: extract_docs(&f.attrs),
                        })
                        .collect(),
                    syn::Fields::Unnamed(unnamed) => unnamed
                        .unnamed
                        .iter()
                        .enumerate()
                        .map(|(i, f)| ApiField {
                            name: format!("_{}", i),
                            ty: type_to_string(&f.ty),
                            doc: String::new(),
                        })
                        .collect(),
                    syn::Fields::Unit => vec![],
                };
                ApiVariant {
                    name: v.ident.to_string(),
                    doc: extract_docs(&v.attrs),
                    fields,
                }
            })
            .collect();

        let name = node.ident.to_string();
        let generics = generics_to_string(&node.generics);

        self.items.push(ApiItem {
            kind: ItemKind::Enum,
            name,
            doc: extract_docs(&node.attrs),
            signature: format!("pub enum {}{}", node.ident, generics),
            module_path: self.module_path.clone(),
            methods: vec![],
            variants,
            fields: vec![],
            generics,
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.ident, &self.rel_path),
            calls: Vec::new(),
        });

        syn::visit::visit_item_enum(self, node);
    }

    // ── free function ─────────────────────────────────────────────────────────
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !self.keep(&node.vis) {
            return;
        }

        self.items.push(ApiItem {
            kind: ItemKind::Function,
            name: node.sig.ident.to_string(),
            doc: extract_docs(&node.attrs),
            signature: sig_to_string(&node.sig),
            module_path: self.module_path.clone(),
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: generics_to_string(&node.sig.generics),
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.sig.ident, &self.rel_path),
            calls: crate::calls::calls_in_block(
                &node.block,
                &node.sig.ident.to_string(),
                &self.rel_path,
            ),
        });
    }

    // ── trait ─────────────────────────────────────────────────────────────────
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if !self.keep(&node.vis) {
            syn::visit::visit_item_trait(self, node);
            return;
        }

        let methods: Vec<ApiMethod> = node
            .items
            .iter()
            .filter_map(|item| {
                if let syn::TraitItem::Fn(m) = item {
                    Some(ApiMethod {
                        name: m.sig.ident.to_string(),
                        doc: extract_docs(&m.attrs),
                        signature: sig_to_string(&m.sig),
                        // Trait items carry no visibility of their own: they are
                        // exactly as reachable as the trait that declares them.
                        visibility: visibility_of(&node.vis),
                        span: span_of(&m.sig.ident, &self.rel_path),
                    })
                } else {
                    None
                }
            })
            .collect();

        let name = node.ident.to_string();
        let generics = generics_to_string(&node.generics);

        self.items.push(ApiItem {
            kind: ItemKind::Trait,
            name,
            doc: extract_docs(&node.attrs),
            signature: format!("pub trait {}{}", node.ident, generics),
            module_path: self.module_path.clone(),
            methods,
            variants: vec![],
            fields: vec![],
            generics,
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.ident, &self.rel_path),
            calls: Vec::new(),
        });

        syn::visit::visit_item_trait(self, node);
    }

    // ── impl block ────────────────────────────────────────────────────────────
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        // Only care about impl blocks for named types (not `impl Trait for &dyn …`)
        let self_ty_name = match node.self_ty.as_ref() {
            syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default(),
            _ => return,
        };

        if self_ty_name.is_empty() {
            return;
        }

        let mut impl_calls: Vec<CallEdge> = Vec::new();
        let methods: Vec<ApiMethod> = node
            .items
            .iter()
            .filter_map(|item| {
                if let syn::ImplItem::Fn(m) = item {
                    if !self.keep(&m.vis) {
                        return None;
                    }
                    // Qualify the caller with its type, so an edge reads
                    // `Canvas::run -> add_plugin` rather than a bare `run`.
                    let qualified = format!("{}::{}", self_ty_name, m.sig.ident);
                    impl_calls.extend(crate::calls::calls_in_block(
                        &m.block,
                        &qualified,
                        &self.rel_path,
                    ));
                    Some(ApiMethod {
                        name: m.sig.ident.to_string(),
                        doc: extract_docs(&m.attrs),
                        signature: sig_to_string(&m.sig),
                        visibility: visibility_of(&m.vis),
                        span: span_of(&m.sig.ident, &self.rel_path),
                    })
                } else {
                    None
                }
            })
            .collect();

        let trait_name = node.trait_.as_ref().map(|(_, path, _)| {
            path.segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default()
        });

        self.pending_impls.push(PendingImpl {
            self_ty: self_ty_name,
            trait_name,
            methods,
            calls: impl_calls,
            module_path: self.module_path.clone(),
        });

        // Don't recurse into impl — we've handled it manually.
    }

    // ── type alias ────────────────────────────────────────────────────────────
    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if !self.keep(&node.vis) {
            return;
        }

        let ty = &node.ty;
        self.items.push(ApiItem {
            kind: ItemKind::TypeAlias,
            name: node.ident.to_string(),
            doc: extract_docs(&node.attrs),
            signature: format!(
                "pub type {} = {};",
                node.ident,
                type_to_string(ty)
            ),
            module_path: self.module_path.clone(),
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: generics_to_string(&node.generics),
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.ident, &self.rel_path),
            calls: Vec::new(),
        });
    }

    // ── const ─────────────────────────────────────────────────────────────────
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        if !self.keep(&node.vis) {
            return;
        }

        let ty = &node.ty;
        // Capture the actual value expression (truncated) — agents asking for
        // engine constants need the real number, not an ellipsis.
        let expr = &node.expr;
        let mut value = quote!(#expr).to_string().replace(" :: ", "::");
        if value.len() > 60 {
            value.truncate(57);
            value.push_str("...");
        }
        self.items.push(ApiItem {
            kind: ItemKind::Const,
            name: node.ident.to_string(),
            doc: extract_docs(&node.attrs),
            signature: format!(
                "pub const {}: {} = {};",
                node.ident,
                type_to_string(ty),
                value
            ),
            module_path: self.module_path.clone(),
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: String::new(),
            traits_impl: vec![],
            origin: String::new(),
            visibility: visibility_of(&node.vis),
            span: span_of(&node.ident, &self.rel_path),
            calls: Vec::new(),
        });
    }

    // ── inline module ─────────────────────────────────────────────────────────
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if let Some((_, items)) = &node.content {
            let old_path = self.module_path.clone();
            self.module_path.push(node.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
            self.module_path = old_path;
            // NOTE: no flush here — the single file-level flush in extract_items
            // attaches everything and returns cross-file leftovers to the caller.
            // An inner flush would drop leftover impls from this module.
        }
    }
}

#[cfg(test)]
mod exclusion_tests {
    use super::*;

    fn write(dir: &Path, rel: &str, src: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, src).unwrap();
    }

    /// Pointing at a project root must index the project, not its build output.
    #[test]
    fn build_output_and_vendored_code_are_not_indexed() {
        let dir = std::env::temp_dir().join("quartz-ctx-exclude-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, "src/lib.rs", "pub struct RealApi;\n");
        write(&dir, "target/debug/build/dep-123/out/bindings.rs", "pub struct GeneratedJunk;\n");
        write(&dir, "node_modules/pkg/thing.rs", "pub struct VendoredJunk;\n");
        write(&dir, ".venv/lib/mod.rs", "pub struct VenvJunk;\n");

        let items = parse_dir(&dir).unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        assert!(names.contains(&"RealApi"), "the project's own API was missed: {names:?}");
        for junk in ["GeneratedJunk", "VendoredJunk", "VenvJunk"] {
            assert!(!names.contains(&junk), "indexed excluded dir content: {junk}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Asking for an excluded directory by name is deliberate and must still work.
    #[test]
    fn an_explicitly_requested_excluded_dir_is_still_scanned() {
        let dir = std::env::temp_dir().join("quartz-ctx-exclude-root-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "target/thing.rs", "pub struct Deliberate;\n");

        let items = parse_dir(&dir.join("target")).unwrap();
        assert!(
            items.iter().any(|i| i.name == "Deliberate"),
            "explicitly scanning a normally-excluded dir must still work"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod visibility_span_tests {
    use super::*;

    fn fixture(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("quartz-ctx-vis").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, src) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        }
        dir
    }

    const APP: &str = "\
struct Internal { pub a: u8 }
pub(crate) struct CrateWide;
pub struct Exported;
";

    /// The library view: only `pub`. This is the default and must not change.
    #[test]
    fn default_extracts_only_public_items() {
        let dir = fixture("lib_view", &[("src/lib.rs", APP)]);
        let items = parse_dir(&dir).unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Exported"]);
    }

    /// The project view: an application declares almost nothing `pub`, so
    /// without this an app indexes to near-nothing.
    #[test]
    fn include_private_extracts_the_whole_project_surface() {
        let dir = fixture("app_view", &[("src/lib.rs", APP)]);
        let items = parse_dir_with(&dir, ParseOptions { include_private: true }).unwrap();
        let mut names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["CrateWide", "Exported", "Internal"]);
    }

    #[test]
    fn visibility_is_recorded_not_just_filtered_on() {
        let dir = fixture("vis_tags", &[("src/lib.rs", APP)]);
        let items = parse_dir_with(&dir, ParseOptions { include_private: true }).unwrap();
        let vis = |n: &str| items.iter().find(|i| i.name == n).unwrap().visibility;

        assert_eq!(vis("Exported"), Visibility::Public);
        assert_eq!(vis("CrateWide"), Visibility::Crate);
        assert_eq!(vis("Internal"), Visibility::Private);
    }

    /// Every item must carry a citable `file:line`, relative to the scanned root
    /// and slash-normalised so it reads the same on every platform.
    #[test]
    fn items_carry_a_relative_forward_slash_span() {
        let dir = fixture("spans", &[
            ("src/deep/mod.rs", "\n\npub struct OnLineThree;\n"),
        ]);
        let items = parse_dir(&dir).unwrap();
        let item = items.iter().find(|i| i.name == "OnLineThree").unwrap();
        let span = item.span.as_ref().expect("span missing");

        assert_eq!(span.file, "src/deep/mod.rs", "must be relative with / separators");
        assert_eq!(span.line, 3);
        assert_eq!(span.to_string(), "src/deep/mod.rs:3");
    }

    #[test]
    fn methods_carry_their_own_visibility_and_span() {
        let dir = fixture("method_spans", &[
            ("src/lib.rs", "pub struct T;\nimpl T {\n    pub fn shown(&self) {}\n    fn hidden(&self) {}\n}\n"),
        ]);

        let public_only = parse_dir(&dir).unwrap();
        let t = public_only.iter().find(|i| i.name == "T").unwrap();
        let names: Vec<&str> = t.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["shown"], "private method leaked into the library view");

        let all = parse_dir_with(&dir, ParseOptions { include_private: true }).unwrap();
        let t = all.iter().find(|i| i.name == "T").unwrap();
        let hidden = t.methods.iter().find(|m| m.name == "hidden").expect("hidden method missing");
        assert_eq!(hidden.visibility, Visibility::Private);
        assert_eq!(hidden.span.as_ref().unwrap().line, 4);
    }
}

#[cfg(test)]
mod impl_attach_tests {
    use super::*;

    /// A same-named type in an unrelated module must not absorb another's impl.
    /// Taking the first name match instead made the wrong type look richer, and
    /// did so silently — nothing errors, the methods simply land on `editor::State`.
    #[test]
    fn same_named_types_attach_by_module_proximity() {
        let dir = std::env::temp_dir().join("quartz-ctx-attach-proximity");
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, src) in [
            ("engine/core.rs", "pub struct State { pub a: u8 }\n"),
            ("engine/ops.rs", "impl State { pub fn engine_only(&self) {} }\n"),
            ("editor/core.rs", "pub struct State { pub b: u8 }\n"),
        ] {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        }

        let items = parse_dir(&dir).unwrap();
        let find = |m: &str| {
            items
                .iter()
                .find(|i| i.name == "State" && i.module_path.first().map(String::as_str) == Some(m))
                .unwrap_or_else(|| panic!("{m}::State missing"))
        };

        assert!(
            find("engine").methods.iter().any(|m| m.name == "engine_only"),
            "impl lost from engine::State"
        );
        assert!(
            !find("editor").methods.iter().any(|m| m.name == "engine_only"),
            "editor::State absorbed an impl belonging to engine::State"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
