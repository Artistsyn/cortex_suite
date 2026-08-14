//! Where one language calls another.
//!
//! A call graph that stops at the language boundary stops exactly where the
//! interesting questions start. This workspace's own editor is the case in
//! point: `frontend/src/lib/animationState.js` calls
//! `fetch(\`/api/models/${modelPath}/animation\`)`, and `server.py` declares
//! `@app.post("/api/models/{model_path:path}/animation")`. Both sides are
//! indexed. Nothing joined them, so "what breaks if I rename this endpoint"
//! had no answer, in either direction.
//!
//! This module finds the two halves and joins them. It works on raw text rather
//! than on parsed items because a boundary is a STRING — a route path, an
//! exported symbol name — and the string is what has to match. Parsing tells you
//! `save_animation` is a function; only the literal tells you it answers
//! `POST /api/models/*/animation`.
//!
//! # What it deliberately does not do
//!
//! It does not guess. A client call whose path is built by string concatenation
//! at runtime is recorded as unresolved rather than approximated, and an
//! endpoint nothing calls is reported as exactly that. Both of those are
//! findings — an unmatched client call is usually a typo or a deleted route, and
//! surfacing it is worth more than a plausible-looking edge that is wrong.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;

use crate::model::{CallEdge, CallKind, SourceSpan};

/// Which side of a boundary, and what kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Declares an endpoint or exports a symbol across the boundary.
    Provides,
    /// Calls an endpoint or imports a symbol from across the boundary.
    Consumes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeKind {
    /// An HTTP route: `@app.get("/api/x")` ↔ `fetch("/api/x")`.
    Http,
    /// A wasm-bindgen or `extern "C"` symbol crossing the FFI boundary.
    Ffi,
}

/// One half of a cross-language link.
#[derive(Debug, Clone)]
pub struct Boundary {
    pub kind: BridgeKind,
    pub side: Side,
    /// Normalised join key: `GET /api/models/*/animation`, or `ffi:solve_ik`.
    pub key: String,
    /// The HTTP method, when the source stated one. `None` means "unstated",
    /// which is NOT the same as GET — a `fetch` with its options object built
    /// elsewhere genuinely does not say.
    pub method: Option<String>,
    /// Path as written, before normalisation — so a human can recognise it.
    pub raw: String,
    pub language: String,
    pub span: SourceSpan,
}

impl Boundary {
    /// The path part of the key, without the method.
    fn path_key(&self) -> &str {
        self.key.rsplit(' ').next().unwrap_or(&self.key)
    }
}

/// A joined cross-language link, or a half that found no partner.
#[derive(Debug, Clone)]
pub struct Link {
    /// The join key. For FFI this is case- and underscore-folded, which makes it
    /// match but makes it unreadable — use [`Link::label`] for display.
    pub key: String,
    pub kind: BridgeKind,
    pub provider: Option<Boundary>,
    pub consumers: Vec<Boundary>,
    /// Client and server agree on the path but not the verb.
    pub method_mismatch: bool,
}

impl Link {
    /// How to show this link to a human.
    ///
    /// The folded FFI key (`ffi:autodetectchains`) is a matching artefact and
    /// appears nowhere in the source, so printing it makes the reader hunt for
    /// a name that does not exist. Show the spelling the provider actually used,
    /// falling back to a caller's when nothing provides it.
    pub fn label(&self) -> String {
        match self.kind {
            BridgeKind::Http => self.key.clone(),
            BridgeKind::Ffi => {
                let name = self
                    .provider
                    .as_ref()
                    .or_else(|| self.consumers.first())
                    .map(|b| b.raw.clone())
                    .unwrap_or_else(|| self.key.clone());
                format!("ffi:{name}")
            }
        }
    }
}

// ── patterns ────────────────────────────────────────────────────────────────

struct Patterns {
    /// (regex, method-group, path-group, side). Group 0 is the whole match.
    http: Vec<(Regex, Option<usize>, usize, Side)>,
    wasm_export: Regex,
    ffi_export: Regex,
    wasm_import: Regex,
}

fn patterns() -> &'static Patterns {
    use std::sync::OnceLock;
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let r = |s: &str| Regex::new(s).expect("bridge pattern must compile");
        Patterns {
            http: vec![
                // ── servers ──
                // Python FastAPI / Flask / Starlette decorators.
                (r(r#"@\w+\.(get|post|put|patch|delete|head|options|websocket)\s*\(\s*["']([^"']+)["']"#), Some(1), 2, Side::Provides),
                (r(r#"@\w+\.route\s*\(\s*["']([^"']+)["']"#), None, 1, Side::Provides),
                // Rust: Axum/Actix `.route("/x", get(h))`, Rocket/Actix attributes.
                (r(r#"\.route\s*\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete)"#), Some(2), 1, Side::Provides),
                (r(r#"#\[(get|post|put|patch|delete)\s*\(\s*"([^"]+)"\s*\)\]"#), Some(1), 2, Side::Provides),
                // Express / Koa / Fastify. The receiver name is what separates a
                // server route from a client call — `app.get` declares,
                // `axios.get` calls, and the rest of the line is identical.
                (r(r#"\b(?:app|router|server|mux|api_router)\.(get|post|put|patch|delete|all|use)\s*\(\s*["'`]([^"'`]+)["'`]"#), Some(1), 2, Side::Provides),
                // Go net/http and chi.
                (r(r#"HandleFunc\s*\(\s*"([^"]+)""#), None, 1, Side::Provides),
                // Spring.
                (r(r#"@(Get|Post|Put|Patch|Delete|Request)Mapping\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#), Some(1), 2, Side::Provides),
                // ASP.NET.
                (r(r#"\[Http(Get|Post|Put|Patch|Delete)\s*\(\s*"([^"]+)"\s*\)\]"#), Some(1), 2, Side::Provides),

                // ── clients ──
                (r(r#"\bfetch\s*\(\s*["'`]([^"'`]+)["'`]"#), None, 1, Side::Consumes),
                (r(r#"\b(?:axios|http|client|api)\.(get|post|put|patch|delete)\s*\(\s*["'`]([^"'`]+)["'`]"#), Some(1), 2, Side::Consumes),
                (r(r#"\brequests\.(get|post|put|patch|delete)\s*\(\s*["']([^"']+)["']"#), Some(1), 2, Side::Consumes),
                (r(r#"\bnew WebSocket\s*\(\s*["'`]([^"'`]+)["'`]"#), None, 1, Side::Consumes),
            ],
            // `#[wasm_bindgen]` sits on its own line above the item, so the
            // export name is found by looking ahead rather than on this line.
            wasm_export: r(r#"#\[wasm_bindgen"#),
            ffi_export: r(r#"extern\s+"C"\s+fn\s+(\w+)"#),
            // `import { solve_ik, init } from './avatar_ik_wasm.js'`
            wasm_import: r(r#"import\s*\{([^}]+)\}\s*from\s*["'`]([^"'`]*(?:wasm|_bg|ffi)[^"'`]*)["'`]"#),
        }
    })
}

/// Join key for a symbol crossing the FFI boundary.
///
/// Rust exports `auto_detect_chains`; the JavaScript that calls it says
/// `autoDetectChains`. That is not a mistake on either side — wasm-bindgen
/// renames snake_case to camelCase in the bindings it generates, and
/// hand-written shims follow the same convention. Measured on this workspace:
/// matching the names literally reported all five `avatar_ik_wasm` exports as
/// uncalled AND all three of their callers as calls into nothing, which is the
/// worst possible answer — two lists of false findings describing one working
/// boundary.
///
/// So the key ignores case and underscores. The name as written is kept in
/// `raw`, because the caller still needs to see which spelling each side uses.
pub fn ffi_key(name: &str) -> String {
    format!("ffi:{}", name.replace('_', "").to_lowercase())
}

/// Reduce a URL to something both sides can agree on.
///
/// The server writes `/api/models/{model_path:path}/animation` and the client
/// writes `` `/api/models/${modelPath}/animation` ``. They describe the same
/// endpoint and share no path parameter syntax at all, so every parameter
/// segment collapses to `*`.
pub fn normalise_path(raw: &str) -> String {
    // Drop scheme and host, and anything after `?` or `#`.
    let s = raw.split(['?', '#']).next().unwrap_or(raw);
    let s = match s.find("://") {
        Some(i) => {
            let rest = &s[i + 3..];
            rest.find('/').map(|j| &rest[j..]).unwrap_or("/")
        }
        None => s,
    };
    let segments: Vec<String> = s
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let is_param = seg.contains('{')      // FastAPI, Spring
                || seg.contains('<')              // Flask
                || seg.contains('$')              // JS template literal
                || seg.starts_with(':')           // Express, Rails
                || seg.contains('*')
                || seg.contains('+');             // encodeURIComponent(...) fragments
            if is_param { "*".to_string() } else { seg.to_lowercase() }
        })
        .collect();
    format!("/{}", segments.join("/"))
}

/// Extract every boundary half in one file.
pub fn scan_file(path: &Path, text: &str, language: &str) -> Vec<Boundary> {
    let p = patterns();
    let rel = path.to_string_lossy().replace('\\', "/");
    let mut out = Vec::new();

    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;

        for (re, method_group, path_group, side) in &p.http {
            for caps in re.captures_iter(line) {
                let Some(raw) = caps.get(*path_group) else { continue };
                let raw = raw.as_str();
                // A path that is not a path — a CSS selector, a mime type, an
                // event name — would produce a junk key that can never match.
                if !raw.starts_with('/') && !raw.contains("://") {
                    continue;
                }
                let method = method_group
                    .and_then(|g| caps.get(g))
                    .map(|m| m.as_str().to_uppercase())
                    .filter(|m| m != "ALL" && m != "USE" && m != "REQUEST");
                let norm = normalise_path(raw);
                out.push(Boundary {
                    kind: BridgeKind::Http,
                    side: *side,
                    key: match &method {
                        Some(m) => format!("{m} {norm}"),
                        None => norm,
                    },
                    method,
                    raw: raw.to_string(),
                    language: language.to_string(),
                    span: SourceSpan { file: rel.clone(), line: line_no },
                });
            }
        }

        // Rust exports across the FFI boundary.
        if p.wasm_export.is_match(line) {
            // The attribute is on its own line; the item follows it.
            if let Some(name) = text
                .lines()
                .skip(n + 1)
                .take(4)
                .find_map(|l| exported_fn_name(l))
            {
                out.push(Boundary {
                    kind: BridgeKind::Ffi,
                    side: Side::Provides,
                    key: ffi_key(&name),
                    method: None,
                    raw: name.clone(),
                    language: language.to_string(),
                    span: SourceSpan { file: rel.clone(), line: line_no },
                });
            }
        }
        if let Some(caps) = p.ffi_export.captures(line) {
            let name = caps[1].to_string();
            out.push(Boundary {
                kind: BridgeKind::Ffi,
                side: Side::Provides,
                key: ffi_key(&name),
                method: None,
                raw: name,
                language: language.to_string(),
                span: SourceSpan { file: rel.clone(), line: line_no },
            });
        }
        // JS importing those exports.
        if let Some(caps) = p.wasm_import.captures(line) {
            for sym in caps[1].split(',') {
                // `init as wasmInit` — the imported name is what the other side
                // exported, not the local alias.
                let sym = sym.split(" as ").next().unwrap_or(sym).trim();
                if sym.is_empty() || sym == "default" || sym.starts_with('*') {
                    continue;
                }
                out.push(Boundary {
                    kind: BridgeKind::Ffi,
                    side: Side::Consumes,
                    key: ffi_key(sym),
                    method: None,
                    raw: sym.to_string(),
                    language: language.to_string(),
                    span: SourceSpan { file: rel.clone(), line: line_no },
                });
            }
        }
    }
    // Patterns overlap by design — `@app.get("/x")` is caught by both the
    // FastAPI decorator rule and the Express receiver rule, and both are right.
    // Deduping here is cheaper and safer than trying to make every pattern
    // mutually exclusive, which would mean one framework's rule silently
    // excluding another's.
    out.sort_by(|a, b| {
        (a.span.line, &a.key, a.side as u8).cmp(&(b.span.line, &b.key, b.side as u8))
    });
    out.dedup_by(|a, b| a.span.line == b.span.line && a.key == b.key && a.side == b.side);
    out
}

/// `pub fn solve_ik(` → `solve_ik`, on the line after a `#[wasm_bindgen]`.
fn exported_fn_name(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("pub async fn "))?;
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    (!name.is_empty()).then_some(name)
}

/// Join the halves.
///
/// Matching is on the normalised path. The method is compared but never used to
/// reject a match: a client POSTing to a route declared GET is a real defect,
/// and dropping the edge would hide the very thing worth seeing.
pub fn link(boundaries: &[Boundary]) -> Vec<Link> {
    let mut by_path: BTreeMap<(BridgeKindKey, String), Link> = BTreeMap::new();

    for b in boundaries {
        let entry = by_path
            .entry((BridgeKindKey(b.kind), b.path_key().to_string()))
            .or_insert_with(|| Link {
                key: b.path_key().to_string(),
                kind: b.kind,
                provider: None,
                consumers: Vec::new(),
                method_mismatch: false,
            });
        match b.side {
            // Several files can declare the same route (a dev proxy, a mock);
            // the first is kept and the rest are not lost, because a duplicate
            // provider is itself worth noticing.
            Side::Provides => {
                if entry.provider.is_none() {
                    entry.provider = Some(b.clone());
                }
            }
            Side::Consumes => entry.consumers.push(b.clone()),
        }
    }

    for l in by_path.values_mut() {
        if let Some(p) = &l.provider {
            if let Some(pm) = &p.method {
                l.method_mismatch = l
                    .consumers
                    .iter()
                    .any(|c| c.method.as_ref().is_some_and(|cm| cm != pm));
            }
        }
    }
    by_path.into_values().collect()
}

/// `BridgeKind` needs `Ord` to be a map key; deriving it on the public enum
/// would imply an ordering that means nothing.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BridgeKindKey(BridgeKind);

impl PartialEq<BridgeKind> for BridgeKindKey {
    fn eq(&self, other: &BridgeKind) -> bool {
        self.0 == *other
    }
}

impl PartialOrd for BridgeKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BridgeKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

/// Turn joined links into call edges, so a cross-language hop appears in the
/// same graph as an ordinary call rather than in a separate report nobody reads.
pub fn edges(links: &[Link]) -> Vec<CallEdge> {
    let mut out = Vec::new();
    for l in links {
        let Some(p) = &l.provider else { continue };
        for c in &l.consumers {
            out.push(CallEdge {
                from: format!("{}:{}", c.span.file, c.span.line),
                to: format!("{}:{}", p.span.file, p.span.line),
                kind: CallKind::CrossLanguage,
                span: Some(c.span.clone()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(name: &str, text: &str, lang: &str) -> Vec<Boundary> {
        scan_file(Path::new(name), text, lang)
    }

    // ── normalisation ──────────────────────────────────────────────────────

    /// The two sides share no path-parameter syntax whatsoever, which is the
    /// entire reason a naive string match finds nothing.
    #[test]
    fn every_parameter_syntax_normalises_to_the_same_key() {
        let want = "/api/models/*/animation";
        for raw in [
            "/api/models/{model_path:path}/animation", // FastAPI
            "/api/models/{modelPath}/animation",       // Spring
            "/api/models/:modelPath/animation",        // Express
            "/api/models/<path:model>/animation",      // Flask
            "/api/models/${modelPath}/animation",      // JS template literal
            "/api/models/*/animation",                 // wildcard
        ] {
            assert_eq!(normalise_path(raw), want, "{raw}");
        }
    }

    #[test]
    fn host_query_and_trailing_slash_are_stripped() {
        assert_eq!(normalise_path("https://api.example.com/v1/scenes?full=1"), "/v1/scenes");
        assert_eq!(normalise_path("/api/scenes/"), "/api/scenes");
        assert_eq!(normalise_path("/API/Scenes"), "/api/scenes");
    }

    // ── detection, on real syntax from this workspace ──────────────────────

    #[test]
    fn fastapi_decorators_are_read_as_providers() {
        let b = scan("server.py", "@app.get(\"/api/scenes\")\n@app.put(\"/api/scenes/{name}\")\n", "python");
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!(b[0].side, Side::Provides);
        assert_eq!(b[0].key, "GET /api/scenes");
        assert_eq!(b[1].key, "PUT /api/scenes/*");
    }

    #[test]
    fn a_javascript_fetch_is_read_as_a_consumer() {
        let b = scan(
            "animationState.js",
            "const r = await fetch(`/api/models/${modelPath}/animations`);\n",
            "javascript",
        );
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(b[0].side, Side::Consumes);
        assert_eq!(b[0].key, "/api/models/*/animations");
        assert_eq!(b[0].span.line, 1);
    }

    /// `app.get` declares a route and `axios.get` calls one. The rest of the
    /// line is identical, so the receiver is the only thing that tells them
    /// apart — and getting it backwards inverts every edge.
    #[test]
    fn a_server_route_and_a_client_call_are_told_apart_by_receiver() {
        let server = scan("routes.js", "app.get('/api/health', handler);\n", "javascript");
        assert_eq!(server[0].side, Side::Provides, "{server:?}");

        let client = scan("api.js", "axios.get('/api/health');\n", "javascript");
        assert_eq!(client[0].side, Side::Consumes, "{client:?}");
    }

    #[test]
    fn rust_axum_and_spring_and_aspnet_routes_are_found() {
        assert_eq!(
            scan("main.rs", "    .route(\"/api/bake\", post(bake_handler))\n", "rust")[0].key,
            "POST /api/bake"
        );
        assert_eq!(
            scan("C.java", "@GetMapping(\"/api/scenes\")\n", "java")[0].key,
            "GET /api/scenes"
        );
        assert_eq!(
            scan("C.cs", "[HttpPost(\"/api/bake\")]\n", "csharp")[0].key,
            "POST /api/bake"
        );
        assert_eq!(
            scan("main.go", "http.HandleFunc(\"/api/ping\", pingHandler)\n", "go")[0].side,
            Side::Provides
        );
    }

    #[test]
    fn a_string_that_is_not_a_path_is_ignored() {
        // A mime type and a CSS selector both sit inside a `.get(`-shaped call
        // in real code, and would produce keys that can never match anything.
        let b = scan("x.js", "client.get('application/json');\nfetch('data:text/plain');\n", "javascript");
        assert!(b.is_empty(), "{b:?}");
    }

    // ── FFI ────────────────────────────────────────────────────────────────

    #[test]
    fn a_wasm_export_and_its_javascript_import_join() {
        let rust = scan(
            "lib.rs",
            "#[wasm_bindgen]\npub fn solve_ik(x: f32) -> f32 { x }\n",
            "rust",
        );
        assert_eq!(rust.len(), 1, "{rust:?}");
        assert_eq!(rust[0].key, ffi_key("solve_ik"));
        assert_eq!(rust[0].side, Side::Provides);

        let js = scan(
            "ik.js",
            "import { solve_ik, init as wasmInit } from './avatar_ik_wasm.js';\n",
            "javascript",
        );
        let keys: Vec<&str> = js.iter().map(|b| b.key.as_str()).collect();
        assert!(keys.contains(&ffi_key("solve_ik").as_str()), "{keys:?}");
        assert!(keys.contains(&ffi_key("init").as_str()), "an aliased import must key on the EXPORTED name: {keys:?}");

        let mut all = rust;
        all.extend(js);
        let links = link(&all);
        let joined: Vec<&Link> =
            links.iter().filter(|l| l.provider.is_some() && !l.consumers.is_empty()).collect();
        assert_eq!(joined.len(), 1, "wasm boundary did not join: {links:?}");
        assert_eq!(joined[0].key, ffi_key("solve_ik"));
    }

    // ── joining ────────────────────────────────────────────────────────────

    #[test]
    fn a_python_route_joins_a_javascript_fetch_across_the_boundary() {
        let mut all = scan(
            "server.py",
            "@app.post(\"/api/models/{model_path:path}/animation\")\ndef save(): pass\n",
            "python",
        );
        all.extend(scan(
            "frontend/src/lib/animationState.js",
            "await fetch(`/api/models/${modelPath}/animation`, { method: 'POST' });\n",
            "javascript",
        ));

        let links = link(&all);
        let joined = links.iter().find(|l| l.provider.is_some() && !l.consumers.is_empty());
        let joined = joined.unwrap_or_else(|| panic!("no cross-language link: {links:?}"));
        assert_eq!(joined.key, "/api/models/*/animation");
        assert_eq!(joined.provider.as_ref().unwrap().language, "python");
        assert_eq!(joined.consumers[0].language, "javascript");

        let e = edges(&links);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, CallKind::CrossLanguage);
        assert!(e[0].to.contains("server.py"), "{:?}", e[0]);
        assert!(e[0].from.contains("animationState.js"), "{:?}", e[0]);
    }

    /// An unmatched client call is the most useful thing this module finds: a
    /// call to an endpoint that does not exist. It must survive as a finding
    /// rather than being quietly dropped or matched to something plausible.
    #[test]
    fn a_call_to_a_nonexistent_endpoint_is_kept_as_an_orphan() {
        let mut all = scan("server.py", "@app.get(\"/api/scenes\")\n", "python");
        all.extend(scan("app.js", "fetch('/api/scenez');\n", "javascript"));

        let links = link(&all);
        let orphan = links
            .iter()
            .find(|l| l.provider.is_none() && !l.consumers.is_empty())
            .unwrap_or_else(|| panic!("orphan call was swallowed: {links:?}"));
        assert_eq!(orphan.key, "/api/scenez");
        assert!(edges(&links).is_empty(), "an orphan must not produce an edge");
    }

    /// Client and server agreeing on the path but not the verb is a defect that
    /// only a joined view can see. Dropping the edge would hide it.
    #[test]
    fn a_method_mismatch_is_reported_not_dropped() {
        let mut all = scan("server.py", "@app.get(\"/api/bake\")\n", "python");
        all.extend(scan("app.js", "axios.post('/api/bake');\n", "javascript"));
        let links = link(&all);
        let l = links.iter().find(|l| l.key == "/api/bake").unwrap();
        assert!(l.provider.is_some() && !l.consumers.is_empty(), "the link must still exist");
        assert!(l.method_mismatch, "POST against a GET route was not flagged");
    }
}

#[cfg(test)]
mod ffi_naming_tests {
    use super::*;

    /// wasm-bindgen renames snake_case to camelCase in the bindings it
    /// generates, and hand-written shims follow suit. Measured on this
    /// workspace, matching literally reported all five `avatar_ik_wasm` exports
    /// as uncalled AND all three callers as calls into nothing — two lists of
    /// false findings describing one working boundary.
    #[test]
    fn snake_case_exports_and_camel_case_imports_are_the_same_symbol() {
        assert_eq!(ffi_key("auto_detect_chains"), ffi_key("autoDetectChains"));
        assert_eq!(ffi_key("solve_body_pose"), ffi_key("solveBodyPose"));
        assert_eq!(ffi_key("neutral_device_rotations"), ffi_key("neutralDeviceRotations"));
        // …but genuinely different symbols must stay different.
        assert_ne!(ffi_key("solve_body_pose"), ffi_key("solve_arm_bones"));
    }

    /// End to end, on the real shape of avatar_ik_wasm and its editor caller.
    #[test]
    fn a_rust_export_joins_its_camel_case_javascript_caller() {
        let mut all = scan_file(
            Path::new("avatar_ik_wasm/src/lib.rs"),
            "#[wasm_bindgen]\npub fn auto_detect_chains(j: JsValue) -> JsValue { j }\n",
            "rust",
        );
        all.extend(scan_file(
            Path::new("frontend/src/lib/boneAutoDetect.js"),
            "import { autoDetectChains } from './wasmIk.js';\n",
            "javascript",
        ));
        let links = link(&all);
        let joined: Vec<&Link> =
            links.iter().filter(|l| l.provider.is_some() && !l.consumers.is_empty()).collect();
        assert_eq!(joined.len(), 1, "the boundary did not join: {links:?}");
        // Each side keeps the spelling it actually uses.
        assert_eq!(joined[0].provider.as_ref().unwrap().raw, "auto_detect_chains");
        assert_eq!(joined[0].consumers[0].raw, "autoDetectChains");
    }
}
