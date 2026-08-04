//! Extract call edges from function bodies.
//!
//! Signatures say what an API offers; call edges say what actually reaches what.
//! That is the difference between "these types mention each other" and "this
//! function calls that one", and it is what makes a dependency path or a change
//! blast-radius answer mean something.
//!
//! Resolution is deliberately partial — see [`CallKind`]. A path call names its
//! callee; a method call does not, because resolving the receiver's type needs
//! inference this extractor does not perform. Emitting the method name with an
//! honest `kind` lets the consumer resolve what it can and leave the rest
//! unresolved, which beats guessing an owner and being confidently wrong.

use std::collections::HashSet;

use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::model::{CallEdge, CallKind, SourceSpan};

/// Collect the calls made inside one body.
///
/// `from` is the qualified name of the enclosing function (`Canvas::run`, or
/// `main` for a free function). `rel_path` is the file, relative to the scanned
/// root, so each edge is citable.
pub fn calls_in_block(block: &syn::Block, from: &str, rel_path: &str) -> Vec<CallEdge> {
    let mut v = CallVisitor {
        from: from.to_string(),
        rel_path: rel_path.to_string(),
        seen: HashSet::new(),
        out: Vec::new(),
    };
    v.visit_block(block);
    v.out
}

struct CallVisitor {
    from: String,
    rel_path: String,
    /// Dedupe on (callee, kind): a call inside a loop is still one edge.
    seen: HashSet<(String, &'static str)>,
    out: Vec<CallEdge>,
}

impl CallVisitor {
    fn push(&mut self, to: String, kind: CallKind, span: proc_macro2::Span) {
        if to.is_empty() {
            return;
        }
        if !self.seen.insert((to.clone(), kind.as_str())) {
            return;
        }
        let line = span.start().line;
        self.out.push(CallEdge {
            from: self.from.clone(),
            to,
            kind,
            span: (line > 0).then(|| SourceSpan { file: self.rel_path.clone(), line }),
        });
    }
}

impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = node.func.as_ref() {
            // Keep the last two segments: `crate::canvas::Canvas::new` carries no
            // more information for our purposes than `Canvas::new`, and the short
            // form is what matches an indexed unit name.
            let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
            let to = match segs.len() {
                0 => String::new(),
                1 => segs[0].clone(),
                n => format!("{}::{}", segs[n - 2], segs[n - 1]),
            };
            self.push(to, CallKind::Path, node.span());
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.push(node.method.to_string(), CallKind::Method, node.span());
        syn::visit::visit_expr_method_call(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(src: &str) -> Vec<CallEdge> {
        let f: syn::ItemFn = syn::parse_str(src).unwrap();
        calls_in_block(&f.block, "caller", "t.rs")
    }

    #[test]
    fn path_calls_keep_their_owner() {
        let e = calls("fn caller() { let c = Canvas::new(ctx); }");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].to, "Canvas::new");
        assert_eq!(e[0].kind, CallKind::Path);
        assert_eq!(e[0].from, "caller");
    }

    #[test]
    fn a_long_path_is_shortened_to_owner_and_member() {
        let e = calls("fn caller() { crate::canvas::core::Canvas::new(x); }");
        assert_eq!(e[0].to, "Canvas::new");
    }

    #[test]
    fn method_calls_record_the_name_and_admit_the_receiver_is_unknown() {
        let e = calls("fn caller() { canvas.add_plugin(p); }");
        assert_eq!(e[0].to, "add_plugin");
        assert_eq!(e[0].kind, CallKind::Method, "must not be claimed as a resolved path");
    }

    #[test]
    fn a_call_repeated_in_a_loop_is_one_edge() {
        let e = calls("fn caller() { for _ in 0..10 { canvas.run(a); } canvas.run(b); }");
        assert_eq!(e.iter().filter(|c| c.to == "run").count(), 1);
    }

    #[test]
    fn nested_and_chained_calls_are_all_found() {
        let e = calls("fn caller() { Canvas::new(ctx).add_plugin(Plugin::default()); }");
        let names: Vec<&str> = e.iter().map(|c| c.to.as_str()).collect();
        for expected in ["Canvas::new", "add_plugin", "Plugin::default"] {
            assert!(names.contains(&expected), "missed {expected} in {names:?}");
        }
    }

    #[test]
    fn calls_carry_a_citable_line() {
        let e = calls("fn caller() {\n\n    Canvas::new(x);\n}");
        assert_eq!(e[0].span.as_ref().unwrap().line, 3);
        assert_eq!(e[0].span.as_ref().unwrap().file, "t.rs");
    }
}
