//! One fixture per language, checked against real syntax.
//!
//! An extractor built from a table of guessed node-kind names fails in the worst
//! possible way: it parses cleanly, finds nothing, and reports a healthy empty
//! result. This project has already shipped that bug twice — `Canvas` served
//! with zero of its 115 methods, in two separate extractors, for months.
//!
//! So every language gets a fixture that asserts real content, and the
//! cross-file cases (Go receivers, C++ out-of-line definitions, C# partials) get
//! their own tests, because those are the ones a per-file extractor silently
//! loses.

use super::*;

fn parse(src: &str, lang: Language, private: bool) -> Extracted {
    parse_file(src, lang, &["m".into()], "m", private)
}

fn names(x: &Extracted) -> Vec<&str> {
    x.items.iter().map(|i| i.name.as_str()).collect()
}

fn item<'a>(x: &'a Extracted, name: &str) -> &'a ApiItem {
    x.items
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no item `{name}`; got {:?}", names(x)))
}

fn method_names(i: &ApiItem) -> Vec<&str> {
    i.methods.iter().map(|m| m.name.as_str()).collect()
}

// ── file typing ─────────────────────────────────────────────────────────────

#[test]
fn language_is_chosen_by_extension() {
    for (f, want) in [
        ("a.py", Language::Python),
        ("a.ts", Language::TypeScript),
        ("a.jsx", Language::JavaScript),
        ("a.go", Language::Go),
        ("a.java", Language::Java),
        ("a.cs", Language::CSharp),
        ("a.cpp", Language::Cpp),
        ("a.hpp", Language::Cpp),
        ("a.rb", Language::Ruby),
        ("a.php", Language::Php),
    ] {
        assert_eq!(Language::from_path(Path::new(f)), Some(want), "{f}");
    }
    assert_eq!(Language::from_path(Path::new("a.rs")), None, "Rust must stay with syn");
    assert_eq!(Language::from_path(Path::new("a.txt")), None);
}

/// Every language this module claims must actually load its grammar. A
/// `set_language` failure returns an empty result, which is indistinguishable
/// from a file with no API.
#[test]
fn every_declared_language_has_a_working_grammar() {
    for lang in [
        Language::Python, Language::TypeScript, Language::JavaScript, Language::Go,
        Language::Java, Language::CSharp, Language::Cpp, Language::Ruby, Language::Php,
    ] {
        let mut p = TsParser::new();
        assert!(p.set_language(&lang.ts_language()).is_ok(), "{} grammar failed", lang.label());
    }
}

// ── Python ──────────────────────────────────────────────────────────────────

#[test]
fn python_classes_carry_methods_docstrings_spans_and_bases() {
    let x = parse(
        "\nclass Canvas(Surface, Drawable):\n    \"\"\"A drawing surface.\"\"\"\n    \
         def draw(self, x):\n        helper(x)\n    def _internal(self): pass\n",
        Language::Python,
        false,
    );
    let c = item(&x, "Canvas");
    assert_eq!(c.doc, "A drawing surface.");
    assert_eq!(c.span.as_ref().unwrap().line, 2);
    assert_eq!(method_names(c), vec!["draw"], "underscore member leaked into the public view");
    assert!(c.traits_impl.contains(&"Surface".to_string()), "{:?}", c.traits_impl);
    assert!(c.traits_impl.contains(&"Drawable".to_string()), "{:?}", c.traits_impl);
    assert!(c.calls.iter().any(|e| e.to == "helper"), "no call edges: {:?}", c.calls);
}

#[test]
fn python_signatures_keep_annotations_and_drop_the_colon() {
    let x = parse(
        "def scale(value: float, factor: float = 2.0) -> float:\n    return value * factor\n",
        Language::Python,
        false,
    );
    let sig = &item(&x, "scale").signature;
    assert!(sig.contains("value: float"), "annotation lost: {sig}");
    assert!(sig.contains("-> float"), "return annotation lost: {sig}");
    assert!(!sig.ends_with(':'), "dangling separator: {sig}");
    assert!(!sig.contains("return value"), "body leaked: {sig}");
}

// ── TypeScript / JavaScript ─────────────────────────────────────────────────

#[test]
fn typescript_classes_interfaces_enums_and_heritage() {
    let x = parse(
        "export class Widget extends Base implements Shape {\n  \
           render(): void { draw(this); }\n  private hide(): void {}\n}\n\
         export interface Shape { area(): number; }\n\
         export enum Mode { Fast, Slow }\n",
        Language::TypeScript,
        false,
    );
    let w = item(&x, "Widget");
    assert_eq!(method_names(w), vec!["render"], "private method leaked");
    assert!(w.traits_impl.contains(&"Base".to_string()), "{:?}", w.traits_impl);
    assert!(w.traits_impl.contains(&"Shape".to_string()), "{:?}", w.traits_impl);
    assert!(w.calls.iter().any(|e| e.to == "draw"), "{:?}", w.calls);

    assert_eq!(item(&x, "Shape").kind, ItemKind::Trait);
    assert_eq!(item(&x, "Mode").variants.len(), 2);
}

#[test]
fn typescript_signatures_keep_their_types() {
    let x = parse(
        "class S {\n  async get(id: number): Promise<User> { return null!; }\n}\n",
        Language::TypeScript,
        false,
    );
    let sig = &item(&x, "S").methods[0].signature;
    assert!(sig.contains("id: number"), "parameter type lost: {sig}");
    assert!(sig.contains("Promise<User>"), "return type lost: {sig}");
    assert!(!sig.contains("return null"), "body leaked into the signature: {sig}");
}

// ── Go ──────────────────────────────────────────────────────────────────────

/// The case that matters most. A Go method lives outside its type's declaration
/// and usually outside its file, so a per-file extractor reports every Go type
/// as data with no behaviour — and looks perfectly healthy doing it.
#[test]
fn a_go_method_becomes_an_orphan_naming_its_receiver_type() {
    let x = parse(
        "package canvas\n\nfunc (c *Canvas) Draw(x int) error {\n\treturn c.flush()\n}\n",
        Language::Go,
        false,
    );
    assert_eq!(x.orphans.len(), 1, "the receiver method was dropped");
    let o = &x.orphans[0];
    assert_eq!(o.self_ty, "Canvas", "wrong owner");
    assert_eq!(o.methods[0].name, "Draw");
    assert!(o.calls.iter().any(|e| e.to == "flush"), "call edges lost: {:?}", o.calls);
}

#[test]
fn go_structs_and_interfaces_are_told_apart() {
    let x = parse(
        "package p\n\ntype Canvas struct {\n\tWidth int\n\theight int\n}\n\
         \ntype Drawable interface {\n\tDraw() error\n}\n",
        Language::Go,
        false,
    );
    assert_eq!(item(&x, "Canvas").kind, ItemKind::Struct);
    let d = item(&x, "Drawable");
    assert_eq!(d.kind, ItemKind::Trait, "an interface is the declared shape");
    assert_eq!(method_names(d), vec!["Draw"]);
    assert!(
        item(&x, "Canvas").fields.iter().any(|f| f.name == "Width"),
        "{:?}",
        item(&x, "Canvas").fields
    );
}

/// Go has no `pub`: the identifier's own first letter is the visibility rule.
/// Getting this wrong publishes every internal helper as API.
#[test]
fn go_visibility_follows_capitalisation() {
    let public = parse("package p\ntype Canvas struct{}\ntype hidden struct{}\n", Language::Go, false);
    assert_eq!(names(&public), vec!["Canvas"], "unexported type leaked");

    let all = parse("package p\ntype Canvas struct{}\ntype hidden struct{}\n", Language::Go, true);
    assert_eq!(all.items.len(), 2, "project view should show both");

    let m = parse("package p\nfunc (c *Canvas) draw() {}\n", Language::Go, false);
    assert!(m.orphans.is_empty(), "unexported method leaked into the public view");
}

// ── Java ────────────────────────────────────────────────────────────────────

#[test]
fn java_classes_carry_members_interfaces_and_calls() {
    let x = parse(
        "public class Widget extends Base implements Shape, Cloneable {\n\
         \tprivate int count;\n\
         \tpublic void render() { Helper.log(this); paint(); }\n\
         \tprivate void hide() {}\n}\n",
        Language::Java,
        false,
    );
    let w = item(&x, "Widget");
    assert_eq!(method_names(w), vec!["render"], "private method leaked");
    for want in ["Base", "Shape", "Cloneable"] {
        assert!(w.traits_impl.contains(&want.to_string()), "missing {want}: {:?}", w.traits_impl);
    }
    // A capitalised receiver in Java is a type, so the edge can name its owner.
    assert!(
        w.calls.iter().any(|e| e.to == "Helper::log" && e.kind == CallKind::Path),
        "static call not resolved to its owner: {:?}",
        w.calls
    );
    assert!(w.calls.iter().any(|e| e.to == "paint"), "{:?}", w.calls);
}

#[test]
fn java_interfaces_and_enums_are_extracted() {
    let x = parse(
        "public interface Shape { double area(); }\npublic enum Mode { FAST, SLOW }\n",
        Language::Java,
        false,
    );
    assert_eq!(item(&x, "Shape").kind, ItemKind::Trait);
    assert_eq!(method_names(item(&x, "Shape")), vec!["area"]);
    let m = item(&x, "Mode");
    let vs: Vec<&str> = m.variants.iter().map(|v| v.name.as_str()).collect();
    assert!(vs.contains(&"FAST") && vs.contains(&"SLOW"), "{vs:?}");
}

// ── C# ──────────────────────────────────────────────────────────────────────

/// A C# file whose classes sit inside `namespace Foo { ... }` — which is nearly
/// all of them — extracts as EMPTY unless the namespace node is descended into,
/// and an empty result looks exactly like a file with no classes.
#[test]
fn csharp_declarations_inside_a_namespace_are_found() {
    let x = parse(
        "namespace App.Core {\n  public class Widget : Base, IShape {\n    \
         public void Render() { Helper.Log(); }\n    private void Hide() {}\n  }\n}\n",
        Language::CSharp,
        false,
    );
    let w = item(&x, "Widget");
    assert_eq!(method_names(w), vec!["Render"], "private method leaked");
    assert!(w.traits_impl.contains(&"IShape".to_string()), "{:?}", w.traits_impl);
    assert!(w.calls.iter().any(|e| e.to == "Helper::Log"), "{:?}", w.calls);
}

/// C# nests a field one level deeper than Java does — `field_declaration ->
/// variable_declaration -> variable_declarator` — so reading only the direct
/// children found neither the name nor the type, and every plain C# field was
/// dropped. Properties still worked, which is what made it invisible: the type
/// listed, its `{ get; set; }` members listed, and its data members were simply
/// absent.
#[test]
fn csharp_plain_fields_are_extracted_not_only_properties() {
    let x = parse(
        "public class Box {\n  public int Width;\n  private string _name;\n  \
         public int Counted = 0;\n  public int A, B;\n  \
         public double Scale { get; set; }\n}\n",
        Language::CSharp,
        true,
    );
    let b = item(&x, "Box");
    let fields: Vec<(&str, &str)> =
        b.fields.iter().map(|f| (f.name.as_str(), f.ty.as_str())).collect();

    for expected in [
        ("Width", "int"),
        ("_name", "string"),
        ("Scale", "double"),
        // An initialiser must not become part of the name ("Counted = 0"), and
        // one declaration can introduce several names.
        ("Counted", "int"),
        ("A", "int"),
        ("B", "int"),
    ] {
        assert!(fields.contains(&expected), "field {expected:?} missing; got {fields:?}");
    }
}

/// The Java shape this used to be written against, kept beside it so a future
/// change to the shared path cannot fix one and break the other.
#[test]
fn java_fields_still_extract_from_their_flatter_shape() {
    let x = parse(
        "public class Box { public int width; private String name; }\n",
        Language::Java,
        true,
    );
    let b = item(&x, "Box");
    let fields: Vec<(&str, &str)> =
        b.fields.iter().map(|f| (f.name.as_str(), f.ty.as_str())).collect();
    assert!(fields.contains(&("width", "int")), "{fields:?}");
    assert!(fields.contains(&("name", "String")), "{fields:?}");
}

/// `partial` is declared in the source, so splitting a type across files is a
/// checkable fact rather than a guess. Merging same-named types on sight is how
/// `editor::State` absorbs `engine::State`.
#[test]
fn a_csharp_partial_class_is_reported_as_splittable() {
    let x = parse(
        "public partial class Canvas { public void Draw() {} }\n",
        Language::CSharp,
        false,
    );
    assert_eq!(x.partial_types, vec!["Canvas".to_string()]);
    // It is NOT an orphan: an orphan resolves to the nearest same-named type,
    // and each half of a partial is nearest to itself, so that route reunites
    // every half with the one place it already was. The fold happens in the
    // global pass instead — see `csharp_partial_halves_rejoin`.
    assert!(x.orphans.is_empty(), "{:?}", x.orphans.len());

    let plain = parse("public class Canvas { public void Draw() {} }\n", Language::CSharp, false);
    assert!(
        plain.partial_types.is_empty(),
        "a non-partial class must never be treated as a fragment"
    );
}

// ── C++ ─────────────────────────────────────────────────────────────────────

/// The .cpp half of a class. Defining members out of line is the normal way to
/// write C++, so dropping these leaves every class holding only whatever its
/// header happened to inline.
#[test]
fn a_cpp_out_of_line_definition_becomes_an_orphan() {
    let x = parse(
        "#include \"canvas.h\"\nvoid Canvas::draw(int x) {\n  flush();\n}\n",
        Language::Cpp,
        false,
    );
    assert_eq!(x.orphans.len(), 1, "out-of-line member definition dropped");
    assert_eq!(x.orphans[0].self_ty, "Canvas");
    assert_eq!(x.orphans[0].methods[0].name, "draw");
    assert!(x.orphans[0].calls.iter().any(|e| e.to == "flush"), "{:?}", x.orphans[0].calls);
}

/// C++ visibility is a section marker, not a per-member keyword: everything
/// after `private:` is private until the next specifier.
#[test]
fn cpp_access_specifiers_apply_to_everything_after_them() {
    let x = parse(
        "class Canvas {\npublic:\n  void draw();\n  void flush();\nprivate:\n  void reset();\n};\n",
        Language::Cpp,
        false,
    );
    let c = item(&x, "Canvas");
    let ms = method_names(c);
    assert!(ms.contains(&"draw") && ms.contains(&"flush"), "{ms:?}");
    assert!(!ms.contains(&"reset"), "private section leaked: {ms:?}");
}

#[test]
fn a_cpp_class_defaults_to_private_and_a_struct_to_public() {
    let c = parse("class C { void hidden(); };\n", Language::Cpp, false);
    assert!(method_names(item(&c, "C")).is_empty(), "class members default to private");

    let s = parse("struct S { void shown(); };\n", Language::Cpp, false);
    assert_eq!(method_names(item(&s, "S")), vec!["shown"], "struct members default to public");
}

// ── Ruby ────────────────────────────────────────────────────────────────────

#[test]
fn ruby_classes_carry_methods_and_a_superclass() {
    let x = parse(
        "class Canvas < Surface\n  def draw(x)\n    flush(x)\n  end\nend\n",
        Language::Ruby,
        false,
    );
    let c = item(&x, "Canvas");
    assert_eq!(method_names(c), vec!["draw"]);
    assert!(c.traits_impl.contains(&"Surface".to_string()), "{:?}", c.traits_impl);
}

// ── PHP ─────────────────────────────────────────────────────────────────────

#[test]
fn php_classes_carry_members_and_interfaces() {
    let x = parse(
        "<?php\nclass Widget extends Base implements Shape {\n  \
         public function render() { Helper::log(); }\n  private function hide() {}\n}\n",
        Language::Php,
        false,
    );
    let w = item(&x, "Widget");
    assert_eq!(method_names(w), vec!["render"], "private method leaked");
    for want in ["Base", "Shape"] {
        assert!(w.traits_impl.contains(&want.to_string()), "missing {want}: {:?}", w.traits_impl);
    }
}

// ── cross-cutting guarantees ────────────────────────────────────────────────

/// The claim this module makes about itself, pinned.
///
/// This started as a doc-comment promise with no field behind it: 283 items from
/// a JavaScript frontend reached cortex tagged exactly like resolved Rust. A
/// property stated only in prose is not a property.
#[test]
fn every_item_from_this_module_is_marked_name_resolved() {
    let sources = [
        ("class A:\n    def m(self): pass\n", Language::Python),
        ("export class B { m() {} }\n", Language::TypeScript),
        ("package p\ntype C struct{}\n", Language::Go),
        ("public class D { public void m() {} }\n", Language::Java),
        ("public class E { public void M() {} }\n", Language::CSharp),
        ("struct F { void m(); };\n", Language::Cpp),
        ("class G\n  def m\n  end\nend\n", Language::Ruby),
        ("<?php\nclass H { public function m() {} }\n", Language::Php),
    ];
    for (src, lang) in sources {
        let x = parse(src, lang, true);
        assert!(!x.items.is_empty(), "{} fixture produced nothing", lang.label());
        for i in &x.items {
            assert_eq!(
                i.confidence,
                Confidence::NameResolved,
                "{} item {} carries the wrong confidence",
                lang.label(),
                i.name
            );
            assert!(i.confidence.is_ast_only(), "must not be trusted as exact");
            assert!(!i.confidence.is_fully_resolved(), "must not claim Rust-grade resolution");
        }
    }
}

/// A call through a variable cannot name its owner without inference this
/// extractor does not perform, so it must say so rather than guess.
#[test]
fn a_call_on_a_lowercase_receiver_admits_the_receiver_is_unknown() {
    let x = parse(
        "public class T { public void go() { canvas.addPlugin(p); } }\n",
        Language::Java,
        false,
    );
    let e = item(&x, "T")
        .calls
        .iter()
        .find(|e| e.to == "addPlugin")
        .unwrap_or_else(|| panic!("{:?}", item(&x, "T").calls));
    assert_eq!(e.kind, CallKind::Method, "must not be claimed as a resolved path");
}

#[test]
fn a_call_repeated_in_a_loop_is_one_edge() {
    let x = parse(
        "public class T { public void go() { for (int i=0;i<10;i++) { run(); } run(); } }\n",
        Language::Java,
        false,
    );
    assert_eq!(item(&x, "T").calls.iter().filter(|c| c.to == "run").count(), 1);
}

/// A syntax error in one file must never take down an index.
#[test]
fn malformed_source_yields_no_panic_in_any_language() {
    for lang in [
        Language::Python, Language::TypeScript, Language::JavaScript, Language::Go,
        Language::Java, Language::CSharp, Language::Cpp, Language::Ruby, Language::Php,
    ] {
        let _ = parse("class ??? {{{ ((( unterminated", lang, false);
        let _ = parse("", lang, false);
        let _ = parse("\u{0}\u{1}\u{2}", lang, false);
    }
}

/// The coverage claim, stated as a test so it cannot quietly regress to the
/// state this module shipped in: three languages, no cross-file linking, no
/// call edges, and five of nine tools returning nothing.
#[test]
fn every_language_yields_a_type_with_behaviour() {
    let cases = [
        ("class A:\n    def m(self): pass\n", Language::Python, "A"),
        ("export class B { m() {} }\n", Language::TypeScript, "B"),
        ("export class J { m() {} }\n", Language::JavaScript, "J"),
        ("public class D { public void m() {} }\n", Language::Java, "D"),
        ("public class E { public void M() {} }\n", Language::CSharp, "E"),
        ("struct F { void m(); };\n", Language::Cpp, "F"),
        ("class G\n  def m\n  end\nend\n", Language::Ruby, "G"),
        ("<?php\nclass H { public function m() {} }\n", Language::Php, "H"),
    ];
    for (src, lang, name) in cases {
        let x = parse(src, lang, true);
        let i = item(&x, name);
        assert!(
            !i.methods.is_empty(),
            "{} extracted `{name}` with no methods — the exact shape of the Canvas bug",
            lang.label()
        );
    }
    // Go is the exception BY DESIGN: its methods are never inside the type, so
    // they arrive as orphans and are attached by the shared pass.
    let go = parse("package p\ntype C struct{}\nfunc (c *C) M() {}\n", Language::Go, true);
    assert_eq!(go.orphans.len(), 1, "Go behaviour must reach the resolution pass");
    assert_eq!(go.orphans[0].self_ty, "C");
}

/// Print the parse tree for a snippet, to read the real node-kind and field
/// names rather than guessing them.
///
///     cargo test dump_trees -- --ignored --nocapture
///
/// Kept because every failure in this module's first run was a wrong guess about
/// a node kind, and each took one look at the s-expression to fix: Go's receiver
/// type hides behind a `type` field (scanning for identifiers finds the receiver
/// VARIABLE first), and Java splits a call into `object` and `name` fields (so
/// the callee field alone is the bare method name). Edit the list when adding a
/// language.
#[test]
#[ignore]
fn dump_trees() {
    for (src, lang) in [
        ("package p\nfunc (c *Canvas) Draw() error { return nil }\n", Language::Go),
        ("public interface Shape { double area(); }\npublic enum Mode { FAST, SLOW }\n", Language::Java),
        ("public class D { public void m() { Helper.log(); } }\n", Language::Java),
    ] {
        let mut p = TsParser::new();
        p.set_language(&lang.ts_language()).unwrap();
        let t = p.parse(src, None).unwrap();
        println!("=== {} ===\n{}\n", lang.label(), t.root_node().to_sexp());
    }
}

// ── build output is not API ─────────────────────────────────────────────────

/// Measured on this workspace: `dist`, `build` and `out` were all on the
/// directory blocklist, and 2,900 of 3,271 extracted items still came from Vite
/// bundles in `static/assets/`. A blocklist cannot know every folder name a
/// bundler might use; the file itself can be asked.
#[test]
fn generated_bundles_are_recognised_by_name_and_by_shape() {
    use crate::parser::looks_generated;
    let p = Path::new;

    // Real filenames from this workspace's own editor build.
    assert!(looks_generated(p("static/assets/main-CqJdblBk.js"), "x"));
    assert!(looks_generated(p("static/assets/theme-CVGumU7S.js"), "x"));
    assert!(looks_generated(p("lib/jquery.min.js"), "x"));
    assert!(looks_generated(p("app.bundle.js"), "x"));

    // No convention at all — caught by shape alone, so an unfamiliar bundler
    // cannot slip past.
    assert!(looks_generated(p("weird.js"), &format!("var a={};", "x".repeat(6000))));

    // And ordinary source must survive all of it, including hyphenated names
    // and long-ish lines.
    assert!(!looks_generated(p("frontend/src/lib/fingerHandles.js"), "export function a() {}\n"));
    assert!(!looks_generated(p("scene-editor.js"), "const x = 1;\n"));
    assert!(!looks_generated(p("my-component.tsx"), &format!("// {}\n", "y".repeat(400))));
    assert!(!looks_generated(p("server.py"), "def main():\n    pass\n"));
}

/// Vite hashes are base64url, so they can contain a hyphen themselves —
/// `iesTextureLoader-BkJ8-dRq.js` split at the LAST hyphen yields a 3-character
/// tail and sailed through the first version of this filter.
#[test]
fn a_hash_containing_a_hyphen_is_still_a_hash() {
    use crate::parser::looks_generated;
    assert!(looks_generated(Path::new("static/assets/iesTextureLoader-BkJ8-dRq.js"), "x"));
    assert!(looks_generated(Path::new("static/assets/tools.functions-BHN4804L.js"), "x"));

    // …and a real 8-character word after a hyphen is a name, not a hash. One
    // leading capital is how people write filenames.
    for ok in ["my-Renderer.js", "scene-Selector.ts", "use-Provider.jsx", "grip-Gizmo.js"] {
        assert!(!looks_generated(Path::new(ok), "export const a = 1;\n"), "{ok}");
    }
}

/// `class T(unittest.TestCase)` declares ONE base. Walking into the dotted name
/// reported the package as a second one.
#[test]
fn a_dotted_base_class_names_one_type_not_two() {
    let x = parse("class T(unittest.TestCase):\n    def m(self): pass\n", Language::Python, false);
    assert_eq!(item(&x, "T").traits_impl, vec!["TestCase".to_string()]);
}
