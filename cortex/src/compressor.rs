/// Compresses source files and API graph items into dense semantic representations.
///
/// The goal: maximum information per token. A 400-line struct becomes ~8 lines
/// of pure signal that Copilot can parse in a fraction of the context cost.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use quote::quote;
use syn::visit::Visit;
use walkdir::WalkDir;

use crate::model::{ApiGraphItem, CodeMember, CodeUnit};

// ── Public entry points ───────────────────────────────────────────────────────

/// Compress all .rs files under `dir` into CodeUnits and their CodeMembers.
/// Members include struct fields, enum variants, and methods for field-level graph inference.
pub fn compress_dir(dir: &Path, scope: Option<&str>) -> Result<(Vec<CodeUnit>, Vec<CodeMember>)> {
    let mut units = Vec::new();
    let mut members = Vec::new();
    let mut pending_impls: Vec<PendingImpl> = Vec::new();

    for entry in WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "rs"))
    {
        let path = entry.path();
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let file = match syn::parse_file(&src) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let raw_module_path = derive_module_path(dir, path);
        let module_path = match scope {
            Some(s) if raw_module_path.is_empty() => s.to_string(),
            Some(s) => format!("{}::{}", s, raw_module_path),
            None => raw_module_path,
        };
        let mut visitor = CompressVisitor {
            units: Vec::new(),
            members: Vec::new(),
            module_path: module_path.clone(),
            pending_impls: Vec::new(),
        };
        visitor.visit_file(&file);
        units.extend(visitor.units);
        members.extend(visitor.members);
        // Impls are NOT attached here: a type's `impl` blocks routinely live in
        // files other than the one declaring it. Collect them and attach in a
        // global pass once the whole tree has been parsed.
        pending_impls.extend(visitor.pending_impls);
    }

    attach_impls(&mut units, pending_impls);

    Ok((units, members))
}

/// Attach collected `impl` blocks to their owning types after the entire source
/// tree has been parsed.
///
/// Rust types routinely spread their `impl` blocks across files — Quartz's
/// `Canvas` declares 115 methods across nine `canvas/*.rs` files. Attaching
/// per file silently discarded every impl whose owning type was declared
/// elsewhere, which left `Canvas` indexed with zero methods and starved
/// `get_item`, `semantic_search` and `uses`-edge inference of its entire
/// callable surface.
///
/// When several indexed types share a name, the impl is attached to the
/// candidate whose module path shares the longest prefix with the impl's own
/// module path, so a same-named type in an unrelated module cannot absorb it.
fn attach_impls(units: &mut [CodeUnit], pending: Vec<PendingImpl>) {
    if pending.is_empty() {
        return;
    }

    // Candidate unit indices by type name, in walk order.
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, unit) in units.iter().enumerate() {
        by_name.entry(unit.name.clone()).or_default().push(i);
    }

    // Accumulate per target unit so a type with nine impl blocks gets one
    // deduped `methods:` line rather than nine partial ones.
    let mut methods_for: HashMap<usize, Vec<String>> = HashMap::new();
    let mut traits_for: HashMap<usize, Vec<String>> = HashMap::new();

    for p in pending {
        let Some(candidates) = by_name.get(&p.self_ty) else { continue };
        let Some(&target) = candidates
            .iter()
            .max_by_key(|&&i| module_prefix_overlap(&units[i].module_path, &p.module_path))
        else {
            continue;
        };

        if !p.methods.is_empty() {
            let acc = methods_for.entry(target).or_default();
            for m in p.methods {
                if !acc.contains(&m) {
                    acc.push(m);
                }
            }
        }
        if let Some(t) = p.trait_impl {
            if !t.is_empty() {
                let acc = traits_for.entry(target).or_default();
                if !acc.contains(&t) {
                    acc.push(t);
                }
            }
        }
    }

    for (i, traits) in &traits_for {
        for t in traits {
            units[*i].compressed.push_str(&format!("impl: {}\n", t));
        }
    }
    for (i, methods) in &methods_for {
        units[*i]
            .compressed
            .push_str(&format!("methods: {}\n", methods.join(" | ")));
    }

    // Term vectors are built from `compressed`, so every augmented unit must be
    // revectorised or semantic search keeps scoring against the pre-impl text.
    for i in methods_for.keys().chain(traits_for.keys()) {
        let compressed = units[*i].compressed.clone();
        units[*i].term_vector = build_term_vector_str(&compressed);
    }
}

/// Number of leading `::`-separated segments two module paths share.
/// Used to pick the right owner when several indexed types share a name.
fn module_prefix_overlap(a: &str, b: &str) -> usize {
    a.split("::")
        .zip(b.split("::"))
        .take_while(|(x, y)| x == y && !x.is_empty())
        .count()
}

/// Ingest an api-graph.json produced by quartz-ctx into CodeUnits.
/// This avoids re-parsing source when quartz-ctx has already done the work, and
/// carries detail cortex's own extractor discards — full method signatures with
/// parameter and return types, per-method docs, and field docs.
///
/// `scope` MUST match the scope the same source is indexed under. quartz-ctx has
/// no notion of cortex's scopes, so its ids are unprefixed: ingesting the synful
/// fork without a scope would emit `canvas::core::Canvas`, which collides with the
/// primary engine's id and silently overwrites Quartz's Canvas with synful's.
pub fn compress_api_graph(items: &[ApiGraphItem], scope: Option<&str>) -> Vec<CodeUnit> {
    items.iter().map(|i| compress_api_item(i, scope)).collect()
}

pub fn compress_api_item(item: &ApiGraphItem, scope: Option<&str>) -> CodeUnit {
    let raw_module = item.module_path.join("::");
    let module_path = match scope {
        Some(s) if raw_module.is_empty() => s.to_string(),
        Some(s) => format!("{}::{}", s, raw_module),
        None => raw_module,
    };

    // quartz-ctx spells kinds `Struct`/`Enum`/`Trait`/`Function`/`Const`; cortex's
    // own extractor and every downstream `kind` filter use `struct`/`enum`/`trait`/
    // `fn`. Ingesting raw would split the vocabulary in two.
    let kind = normalise_api_kind(&item.kind);

    let compressed = render_compressed_api(item, &kind, &module_path);
    let summary = build_summary_api(item, &kind);
    let id = if module_path.is_empty() {
        item.name.clone()
    } else {
        format!("{}::{}", module_path, item.name)
    };

    CodeUnit {
        id,
        kind,
        name: item.name.clone(),
        module_path,
        summary,
        term_vector: build_term_vector_str(&compressed),
        compressed,
        indexed_at: chrono::Utc::now(),
    }
}

/// Map quartz-ctx's `ItemKind` spelling onto cortex's kind vocabulary.
fn normalise_api_kind(kind: &str) -> String {
    match kind {
        "Struct" => "struct".to_string(),
        "Enum" => "enum".to_string(),
        "Trait" => "trait".to_string(),
        "Function" => "fn".to_string(),
        "TypeAlias" => "type".to_string(),
        "Const" => "const".to_string(),
        other => other.to_lowercase(),
    }
}

// ── Compression renderers ─────────────────────────────────────────────────────

fn render_compressed_api(item: &ApiGraphItem, kind: &str, module_path: &str) -> String {
    let mut s = String::new();

    // Header: [kind: Name] (module)
    let module_hint = if module_path.is_empty() {
        String::new()
    } else {
        format!(" ({})", module_path)
    };
    s.push_str(&format!("[{}: {}{}]\n", kind, item.name, module_hint));

    // Doc summary — first line only
    if let Some(doc_line) = item.doc.lines().next() {
        let trimmed = doc_line.trim();
        if !trimmed.is_empty() {
            s.push_str(&format!("// {}\n", trimmed));
        }
    }

    // Source location, so an agent can cite and open the declaration rather
    // than grep for it. Kept on its own line and prefixed like every other
    // field so `uses`-edge inference is unaffected.
    if let Some(span) = &item.span {
        s.push_str(&format!("at: {}:{}\n", span.file, span.line));
    }

    // Visibility — only when it is not the plain public API surface, so the
    // common case costs nothing.
    match item.visibility.as_deref() {
        None | Some("Public") | Some("pub") => {}
        Some(v) => s.push_str(&format!("visibility: {}\n", v)),
    }

    // Signature — compressed
    if !item.signature.is_empty() {
        s.push_str(&format!("sig: {}\n", item.signature.trim()));
    }

    // Fields
    if !item.fields.is_empty() {
        let fields: Vec<String> = item.fields.iter()
            .map(|f| {
                if f.doc.is_empty() {
                    format!("{}: {}", f.name, f.ty)
                } else {
                    format!("{}: {} // {}", f.name, f.ty, first_line(&f.doc))
                }
            })
            .collect();
        s.push_str(&format!("fields: {}\n", fields.join(", ")));
    }

    // Variants (enums — the most critical for Quartz)
    if !item.variants.is_empty() {
        s.push_str("variants:\n");
        for v in &item.variants {
            let fields = if v.fields.is_empty() {
                String::new()
            } else {
                let fstr: Vec<String> = v.fields.iter()
                    .map(|f| {
                        if f.name.starts_with('_') {
                            f.ty.clone()
                        } else {
                            format!("{}: {}", f.name, f.ty)
                        }
                    })
                    .collect();
                format!(" {{ {} }}", fstr.join(", "))
            };
            let doc = if v.doc.is_empty() {
                String::new()
            } else {
                format!(" // {}", first_line(&v.doc))
            };
            s.push_str(&format!("  {}::{}{}{}\n", item.name, v.name, fields, doc));
        }
    }

    // Methods — names + compressed signatures only
    if !item.methods.is_empty() {
        let methods: Vec<String> = item.methods.iter()
            .map(|m| {
                // Strip body keywords for brevity
                let sig = m.signature
                    .replace("pub fn ", "")
                    .replace("fn ", "");
                if m.doc.is_empty() {
                    sig
                } else {
                    format!("{} // {}", sig, first_line(&m.doc))
                }
            })
            .collect();
        s.push_str(&format!("methods: {}\n", methods.join(" | ")));
    }

    // Trait impls
    if !item.traits_impl.is_empty() {
        s.push_str(&format!("impl: {}\n", item.traits_impl.join(", ")));
    }

    s
}

fn build_summary_api(item: &ApiGraphItem, kind: &str) -> String {
    let doc = first_line(&item.doc);
    let variant_count = if item.variants.is_empty() {
        String::new()
    } else {
        format!(" [{} variants]", item.variants.len())
    };
    let field_count = if item.fields.is_empty() {
        String::new()
    } else {
        format!(" [{} fields]", item.fields.len())
    };

    if doc.is_empty() {
        format!("{} `{}`{}{}", kind, item.name, variant_count, field_count)
    } else {
        format!("{} `{}` — {}{}{}", kind, item.name, doc, variant_count, field_count)
    }
}

// ── syn visitor (for raw .rs files not covered by quartz-ctx) ────────────────

struct PendingImpl {
    self_ty: String,
    methods: Vec<String>,
    trait_impl: Option<String>,
    /// Module path the `impl` block itself was found in — used to pick the
    /// right owner when several indexed types share a name.
    module_path: String,
}

struct CompressVisitor {
    units: Vec<CodeUnit>,
    members: Vec<CodeMember>,
    module_path: String,
    pending_impls: Vec<PendingImpl>,
}

impl CompressVisitor {
    fn make_unit(&self, kind: &str, name: &str, compressed: String) -> CodeUnit {
        let summary = format!("{} `{}`", kind, name);
        let id = if self.module_path.is_empty() {
            name.to_string()
        } else {
            format!("{}::{}", self.module_path, name)
        };
        CodeUnit {
            id,
            kind: kind.to_string(),
            name: name.to_string(),
            module_path: self.module_path.clone(),
            summary,
            term_vector: build_term_vector_str(&compressed),
            compressed,
            indexed_at: chrono::Utc::now(),
        }
    }
}

impl<'ast> Visit<'ast> for CompressVisitor {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if !is_pub(&node.vis) { return; }

        let name = node.ident.to_string();
        let doc = extract_doc(&node.attrs);
        let fields: Vec<String> = if let syn::Fields::Named(nf) = &node.fields {
            nf.named.iter()
                .filter(|f| is_pub(&f.vis))
                .map(|f| format!("{}: {}", f.ident.as_ref().unwrap(), ty_str(&f.ty)))
                .collect()
        } else { vec![] };

        let mut compressed = format!("[struct: {}]\n", name);
        if let Some(d) = doc.lines().next() { if !d.trim().is_empty() { compressed.push_str(&format!("// {}\n", d.trim())); } }
        if !fields.is_empty() { compressed.push_str(&format!("fields: {}\n", fields.join(", "))); }

        let parent_id = if self.module_path.is_empty() {
            name.clone()
        } else {
            format!("{}::{}", self.module_path, name)
        };

        // Populate code_members for struct fields (enables field-level graph inference)
        if let syn::Fields::Named(nf) = &node.fields {
            for f in nf.named.iter().filter(|f| is_pub(&f.vis)) {
                let field_name = f.ident.as_ref().unwrap().to_string();
                self.members.push(CodeMember {
                    parent_id: parent_id.clone(),
                    kind: "field".to_string(),
                    name: field_name,
                    type_sig: ty_str(&f.ty),
                    doc: extract_doc(&f.attrs),
                });
            }
        }

        self.units.push(self.make_unit("struct", &name, compressed));
        syn::visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if !is_pub(&node.vis) { return; }

        let name = node.ident.to_string();
        let doc = extract_doc(&node.attrs);
        let mut compressed = format!("[enum: {}]\n", name);
        if let Some(d) = doc.lines().next() { if !d.trim().is_empty() { compressed.push_str(&format!("// {}\n", d.trim())); } }

        let parent_id = if self.module_path.is_empty() {
            name.clone()
        } else {
            format!("{}::{}", self.module_path, name)
        };

        compressed.push_str("variants:\n");
        for v in &node.variants {
            let vdoc = extract_doc(&v.attrs);
            let fields: Vec<String> = match &v.fields {
                syn::Fields::Named(nf) => nf.named.iter()
                    .map(|f| format!("{}: {}", f.ident.as_ref().unwrap(), ty_str(&f.ty)))
                    .collect(),
                syn::Fields::Unnamed(uf) => uf.unnamed.iter()
                    .map(|f| ty_str(&f.ty))
                    .collect(),
                syn::Fields::Unit => vec![],
            };
            let fstr = if fields.is_empty() { String::new() } else { format!(" {{ {} }}", fields.join(", ")) };
            let dstr = if vdoc.is_empty() { String::new() } else { format!(" // {}", first_line(&vdoc)) };
            compressed.push_str(&format!("  {}::{}{}{}\n", name, v.ident, fstr, dstr));

            // Populate code_members for enum variants (enables variant-level graph inference)
            let type_sig = if fields.is_empty() { String::new() } else { format!("{{ {} }}", fields.join(", ")) };
            self.members.push(CodeMember {
                parent_id: parent_id.clone(),
                kind: "variant".to_string(),
                name: v.ident.to_string(),
                type_sig,
                doc: vdoc,
            });
        }

        self.units.push(self.make_unit("enum", &name, compressed));
        syn::visit::visit_item_enum(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_pub(&node.vis) { return; }
        let name = node.sig.ident.to_string();
        let sig = quote!(#(node.sig)).to_string();
        let doc = extract_doc(&node.attrs);
        let mut compressed = format!("[fn: {}]\n", name);
        if let Some(d) = doc.lines().next() { if !d.trim().is_empty() { compressed.push_str(&format!("// {}\n", d.trim())); } }
        compressed.push_str(&format!("sig: {}\n", sig));
        self.units.push(self.make_unit("fn", &name, compressed));
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if !is_pub(&node.vis) { return; }
        let name = node.ident.to_string();
        let doc = extract_doc(&node.attrs);
        let methods: Vec<String> = node.items.iter().filter_map(|item| {
            if let syn::TraitItem::Fn(m) = item {
                Some(m.sig.ident.to_string())
            } else { None }
        }).collect();
        let mut compressed = format!("[trait: {}]\n", name);
        if let Some(d) = doc.lines().next() {
            if !d.trim().is_empty() { compressed.push_str(&format!("// {}\n", d.trim())); }
        }
        if !methods.is_empty() {
            compressed.push_str(&format!("methods: {}\n", methods.join(" | ")));
        }
        self.units.push(self.make_unit("trait", &name, compressed));
        syn::visit::visit_item_trait(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let self_ty = match node.self_ty.as_ref() {
            syn::Type::Path(p) => p.path.segments.last()
                .map(|s| s.ident.to_string()).unwrap_or_default(),
            _ => return,
        };
        let trait_impl: Option<String> = node.trait_.as_ref().and_then(|(_, path, _)| {
            path.segments.last().map(|s| s.ident.to_string())
        });
        let methods: Vec<String> = node.items.iter().filter_map(|item| {
            if let syn::ImplItem::Fn(m) = item {
                if !is_pub(&m.vis) { return None; }
                Some(m.sig.ident.to_string())
            } else { None }
        }).collect();

        self.pending_impls.push(PendingImpl {
            self_ty,
            methods,
            trait_impl,
            module_path: self.module_path.clone(),
        });
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if let Some((_, items)) = &node.content {
            let old = self.module_path.clone();
            if self.module_path.is_empty() {
                self.module_path = node.ident.to_string();
            } else {
                self.module_path = format!("{}::{}", self.module_path, node.ident);
            }
            for item in items { self.visit_item(item); }
            // No flush here — an inline `mod` can impl a type declared in an
            // outer module or another file. Leftovers travel up to the global
            // attach pass in `compress_dir`.
            self.module_path = old;
        }
    }
}

// ── TF-IDF term vectors ───────────────────────────────────────────────────────

/// Builds a normalised TF-IDF-style term vector from text.
/// Stored in the DB; used for cosine similarity search without external ML deps.
pub fn build_term_vector_str(text: &str) -> Vec<(String, f32)> {
    let tokens = tokenise(text);
    if tokens.is_empty() { return vec![]; }

    let mut tf: HashMap<String, f32> = HashMap::new();
    for tok in &tokens {
        *tf.entry(tok.clone()).or_insert(0.0) += 1.0;
    }
    let total = tokens.len() as f32;
    let mut vec: Vec<(String, f32)> = tf.into_iter()
        .map(|(k, v)| (k, v / total))
        .collect();

    // Normalise
    let magnitude = (vec.iter().map(|(_, w)| w * w).sum::<f32>()).sqrt();
    if magnitude > 0.0 {
        for (_, w) in &mut vec { *w /= magnitude; }
    }

    vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    vec
}

/// Cosine similarity between two term vectors.
pub fn cosine_similarity(a: &[(String, f32)], b: &[(String, f32)]) -> f32 {
    let b_map: HashMap<&str, f32> = b.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    a.iter()
        .filter_map(|(k, v)| b_map.get(k.as_str()).map(|bv| v * bv))
        .sum()
}

fn tokenise(text: &str) -> Vec<String> {
    // Split on non-alphanumeric, lowercase, filter stop words and short tokens
    let stop: &[&str] = &[
        "the", "a", "an", "is", "it", "in", "of", "to", "and", "or", "for",
        "on", "at", "be", "as", "by", "fn", "pub", "let", "use", "mut", "self",
    ];
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 3 && !stop.contains(&t.as_str()))
        .collect()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn derive_module_path(base: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(base).unwrap_or(file);
    relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .filter(|s| s != "mod" && s != "lib" && s != "main")
        .collect::<Vec<_>>()
        .join("::")
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

fn extract_doc(attrs: &[syn::Attribute]) -> String {
    attrs.iter().filter_map(|a| {
        if !a.path().is_ident("doc") { return None; }
        if let syn::Meta::NameValue(nv) = &a.meta {
            if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = &nv.value {
                return Some(s.value().trim().to_string());
            }
        }
        None
    }).collect::<Vec<_>>().join("\n")
}

fn ty_str(ty: &syn::Type) -> String {
    quote!(#ty).to_string()
        .replace(" :: ", "::")
        .replace("< ", "<")
        .replace(" >", ">")
}

fn first_line(s: &str) -> &str {
    s.lines().next().map(str::trim).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, src: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, src).unwrap();
    }

    fn methods_of(units: &[CodeUnit], name: &str) -> Vec<String> {
        units
            .iter()
            .find(|u| u.name == name)
            .map(|u| {
                u.compressed
                    .lines()
                    .filter_map(|l| l.strip_prefix("methods:"))
                    .flat_map(|l| l.split('|'))
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Regression: a type declared in one file with `impl` blocks in others must
    /// keep every method. Attaching impls per file dropped all of them, which is
    /// how Quartz's `Canvas` came to be indexed with zero of its 115 methods.
    #[test]
    fn impls_in_other_files_attach_to_their_type() {
        let dir = std::env::temp_dir().join("cortex_test_crossfile_impl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, "core.rs", "pub struct Canvas { pub w: f32 }\n\
                                impl Canvas { pub fn new() -> Self { Self { w: 0.0 } } }\n");
        write(&dir, "actions.rs", "impl crate::Canvas { pub fn run(&mut self) {} }\n");
        write(&dir, "physics.rs", "impl Canvas {\n\
                                   pub fn enable_crystalline(&mut self) {}\n\
                                   pub fn set_gravity_scale(&mut self, s: f32) {}\n}\n");

        let (units, _) = compress_dir(&dir, None).unwrap();
        let methods = methods_of(&units, "Canvas");

        for expected in ["new", "run", "enable_crystalline", "set_gravity_scale"] {
            assert!(
                methods.contains(&expected.to_string()),
                "method `{expected}` was dropped; got {methods:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nine impl blocks must collapse into one deduped `methods:` line, not nine.
    #[test]
    fn repeated_impl_blocks_produce_one_deduped_methods_line() {
        let dir = std::env::temp_dir().join("cortex_test_dedupe_impl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, "a.rs", "pub struct T;\nimpl T { pub fn go(&self) {} }\n");
        write(&dir, "b.rs", "impl T { pub fn go(&self) {} pub fn stop(&self) {} }\n");

        let (units, _) = compress_dir(&dir, None).unwrap();
        let unit = units.iter().find(|u| u.name == "T").unwrap();

        let lines = unit.compressed.lines().filter(|l| l.starts_with("methods:")).count();
        assert_eq!(lines, 1, "expected one methods line, got {lines}");
        assert_eq!(methods_of(&units, "T"), vec!["go", "stop"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A same-named type in an unrelated module must not absorb another's impl.
    #[test]
    fn same_named_types_attach_by_module_proximity() {
        let dir = std::env::temp_dir().join("cortex_test_name_collision");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, "engine/core.rs", "pub struct State { pub a: u8 }\n");
        write(&dir, "engine/ops.rs", "impl State { pub fn engine_only(&self) {} }\n");
        write(&dir, "editor/core.rs", "pub struct State { pub b: u8 }\n");

        let (units, _) = compress_dir(&dir, None).unwrap();
        let engine = units
            .iter()
            .find(|u| u.name == "State" && u.module_path.starts_with("engine"))
            .expect("engine::State missing");
        let editor = units
            .iter()
            .find(|u| u.name == "State" && u.module_path.starts_with("editor"))
            .expect("editor::State missing");

        assert!(engine.compressed.contains("engine_only"), "impl lost from engine::State");
        assert!(
            !editor.compressed.contains("engine_only"),
            "editor::State absorbed an impl belonging to engine::State"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn api_item(kind: &str, module: &[&str], name: &str) -> crate::model::ApiGraphItem {
        crate::model::ApiGraphItem {
            kind: kind.to_string(),
            name: name.to_string(),
            doc: String::new(),
            signature: String::new(),
            module_path: module.iter().map(|s| s.to_string()).collect(),
            methods: vec![],
            variants: vec![],
            fields: vec![],
            generics: String::new(),
            traits_impl: vec![],
            visibility: None,
            span: None,
        }
    }

    /// quartz-ctx ids carry no scope. Ingesting the synful fork unscoped would emit
    /// `canvas::core::Canvas`, colliding with the primary engine's id — and since
    /// persistence is INSERT OR REPLACE, silently overwriting Quartz's Canvas with
    /// synful's.
    #[test]
    fn api_graph_ingestion_applies_the_source_scope_to_ids() {
        let items = vec![api_item("Struct", &["canvas", "core"], "Canvas")];

        let unscoped = compress_api_graph(&items, None);
        assert_eq!(unscoped[0].id, "canvas::core::Canvas");

        let scoped = compress_api_graph(&items, Some("synful"));
        assert_eq!(scoped[0].id, "synful::canvas::core::Canvas");
        assert_eq!(scoped[0].module_path, "synful::canvas::core");
        assert_ne!(
            scoped[0].id, unscoped[0].id,
            "a scoped fork must not share an id with the primary engine"
        );
    }

    /// quartz-ctx spells kinds `Struct`/`Function`; cortex and every downstream
    /// `kind` filter use `struct`/`fn`. Mixing the two splits the vocabulary.
    #[test]
    fn api_graph_kinds_are_normalised_to_cortex_vocabulary() {
        let items = vec![
            api_item("Struct", &["m"], "S"),
            api_item("Enum", &["m"], "E"),
            api_item("Trait", &["m"], "T"),
            api_item("Function", &["m"], "f"),
            api_item("TypeAlias", &["m"], "A"),
            api_item("Const", &["m"], "C"),
        ];
        let kinds: Vec<String> = compress_api_graph(&items, None)
            .iter()
            .map(|u| u.kind.clone())
            .collect();
        assert_eq!(kinds, ["struct", "enum", "trait", "fn", "type", "const"]);
    }

    /// The rendered header must use the normalised kind and the scoped module too,
    /// since that text is what `get_item` shows and what term vectors are built from.
    #[test]
    fn api_graph_rendered_text_uses_normalised_kind_and_scoped_module() {
        let items = vec![api_item("Struct", &["canvas", "core"], "Canvas")];
        let unit = &compress_api_graph(&items, Some("synful"))[0];
        assert!(
            unit.compressed.starts_with("[struct: Canvas (synful::canvas::core)]"),
            "unexpected header: {}",
            unit.compressed.lines().next().unwrap_or("")
        );
        assert!(unit.summary.starts_with("struct `Canvas`"), "summary: {}", unit.summary);
    }

    /// Gate A: against the real Quartz tree, cortex must now agree with
    /// quartz-ctx on the engine's central type. Skipped when the workspace
    /// isn't present (standalone cortex checkouts).
    ///
    /// Run with: `cargo test --bin cortex -- --ignored gate_a`
    #[test]
    #[ignore = "requires the FlowMake workspace at ../quartz/src"]
    fn gate_a_real_quartz_canvas_has_its_methods() {
        let src = Path::new("../quartz/src");
        if !src.exists() {
            eprintln!("skipping: {} not present", src.display());
            return;
        }

        let (units, _) = compress_dir(src, None).unwrap();
        let methods = methods_of(&units, "Canvas");

        assert!(
            methods.len() >= 115,
            "Canvas has {} methods, expected >= 115 (quartz-ctx reports 115)",
            methods.len()
        );
        for expected in ["run", "add_plugin", "pool_acquire", "enable_crystalline", "set_var"] {
            assert!(methods.contains(&expected.to_string()), "missing `{expected}`");
        }

        // GameObject spreads impls across game_object/*.rs the same way and lost
        // 16 of its 48 methods to the per-file attach.
        let go = methods_of(&units, "GameObject");
        assert!(
            go.len() >= 48,
            "GameObject has {} methods, expected >= 48 (quartz-ctx reports 48)",
            go.len()
        );

        let typed: Vec<_> = units
            .iter()
            .filter(|u| matches!(u.kind.as_str(), "struct" | "enum" | "trait"))
            .collect();
        let with_methods = typed
            .iter()
            .filter(|u| u.compressed.contains("methods:"))
            .count();
        let total_methods: usize = typed
            .iter()
            .map(|u| methods_of(&units, &u.name).len())
            .sum();

        eprintln!(
            "Gate A: {} units | {} typed, {} with methods ({:.0}%) | {} methods total | Canvas {}",
            units.len(),
            typed.len(),
            with_methods,
            100.0 * with_methods as f64 / typed.len() as f64,
            total_methods,
            methods.len(),
        );
    }

    /// Augmenting `compressed` without revectorising leaves semantic search
    /// scoring against text that no longer matches the record.
    #[test]
    fn term_vector_reflects_attached_methods() {
        let dir = std::env::temp_dir().join("cortex_test_termvec");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, "t.rs", "pub struct Widget;\n");
        write(&dir, "impls.rs", "impl Widget { pub fn rasterise(&self) {} }\n");

        let (units, _) = compress_dir(&dir, None).unwrap();
        let unit = units.iter().find(|u| u.name == "Widget").unwrap();

        assert!(
            unit.term_vector.iter().any(|(t, _)| t == "rasterise"),
            "term vector missing a method term; got {:?}",
            unit.term_vector
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
