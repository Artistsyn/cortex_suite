#![allow(dead_code, unused_imports, unused_variables)]

mod bridge;
mod calls;
mod discover;
mod helpers;
mod lang;
mod mcp;
mod model;
mod parser;
mod usage;
mod render;

use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{error::ErrorKind, Parser, Subcommand};
use serde_json::json;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(name = "quartz-ctx", version, about = "API context tool and MCP skill server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate static markdown documentation tree at docs/<scraped-directory>/.
    /// 
    /// Run this once after API changes to refresh the docs that Copilot reads.
    /// Output includes INDEX.md (entry point), vocabulary.md (enums), types.md, traits.md,
    /// functions.md, and api-graph.json.
    /// 
    /// Example:
    ///   quartz-ctx generate --source quartz/src --name Quartz
    /// 
    /// Then add to .github/copilot-instructions.md:
    ///   Before writing Quartz code, review docs/quartz/INDEX.md
    Generate(GenerateArgs),

    /// Run as an MCP stdio skill server for live API lookups.
    /// 
    /// Copilot calls this in real-time during chat to look up exact signatures,
    /// list available variants, search for APIs, etc. All data is loaded once at startup.
    /// 
    /// Configure in .vscode/mcp.json:
    ///   {
    ///     "servers": {
    ///       "quartz-ctx": {
    ///         "type": "stdio",
    ///         "command": "quartz-ctx",
    ///         "args": ["serve", "--source", "quartz/src", "--name", "Quartz"]
    ///       }
    ///     }
    ///   }
    /// 
    /// Then Copilot can call tools like:
    ///   - get_variants({\"name\": \"Action\"})
    ///   - search_items({\"query\": \"gravity\"})
    ///   - list_items({\"kind\": \"enum\"})
    Serve(ServeArgs),

    /// Run startup diagnostics and source validation.
    ///
    /// Helpful when MCP fails to boot or source paths are incorrect.
    ///
    /// Example:
    ///   quartz-ctx selfcheck --source quartz/src --name Quartz
    Selfcheck(SelfcheckArgs),

    /// Show where one language calls another, and where it fails to.
    ///
    /// Joins HTTP routes to the fetch/axios/requests calls that hit them, and
    /// wasm/FFI exports to the code that imports them. Reports the unmatched
    /// halves too — a call with no route behind it is invisible to every
    /// single-language tool, including the compiler, because neither side is
    /// wrong on its own.
    ///
    /// Example:
    ///   quartz-ctx boundaries --source vr_workspace/scene_editor_web
    Boundaries(BoundariesArgs),
}

#[derive(Parser, Debug)]
struct BoundariesArgs {
    /// Root to scan (recursively).
    #[arg(short, long, default_value = ".")]
    source: PathBuf,

    /// Only boundaries whose key contains this substring.
    #[arg(long)]
    filter: Option<String>,
}

// ── generate ──────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
struct GenerateArgs {
    /// Source directory to analyse (scanned recursively for .rs files).
    #[arg(short, long, default_value = "src")]
    source: PathBuf,

    /// Output root. Context tree lands at <output>/docs/<context-dir>/.
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// Engine / stack name used in file headers.
    #[arg(short, long, default_value = "Quartz")]
    name: String,

    /// Subdirectory name under docs/ for the context tree.
    /// Defaults to the scraped directory name, e.g. "quartz" for `--source ../quartz/src`.
    #[arg(long)]
    context_dir: Option<String>,

    /// Only write INDEX.md, vocabulary.md, and api-graph.json.
    #[arg(long)]
    minimal: bool,

    /// Print extracted items and exit without writing any files.
    #[arg(long)]
    dry_run: bool,

    /// Index items that are not `pub`. See `serve --include-private`.
    #[arg(long)]
    include_private: bool,
}

// ── serve ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
struct ServeArgs {
    /// Source directory to load. REPEATABLE — pass multiple --source flags to
    /// serve several roots from one server (e.g. quartz/src, synful_quartz/quartz/src,
    /// path_forge/src). The first source is the primary engine; items from every
    /// root are tagged with an origin slug so lookups can tell them apart.
    #[arg(short, long)]
    source: Vec<PathBuf>,

    /// Load source roots from a JSON manifest instead of listing them by hand.
    ///
    /// Expects `{ "targets": [ { "source": "...", "scope": "..." }, ... ] }` —
    /// the same shape as cortex's `.cortex/index-sources.json`, so one file can
    /// drive both the indexer and this server and the two cannot drift apart.
    /// When a target has a `scope`, it is used as the origin tag, keeping
    /// quartz-ctx origins identical to cortex scopes.
    ///
    /// Roots given with --source are loaded first (so the primary engine stays
    /// primary), then any from the manifest that are not already present.
    ///
    /// Example:
    ///   quartz-ctx serve --sources-from .cortex/index-sources.json --name Quartz
    #[arg(long)]
    sources_from: Option<PathBuf>,

    /// Engine / stack name reported in the MCP server info.
    #[arg(short, long, default_value = "Quartz")]
    name: String,

    /// Index items that are not `pub`, not just the public API surface.
    ///
    /// A library publishes its API through `pub`, so the default view is the
    /// right one for an engine or crate. An application or binary crate
    /// publishes almost nothing, so without this an app indexes to near-nothing.
    /// Turn it on when pointing quartz-ctx at a project rather than a library.
    #[arg(long)]
    include_private: bool,

    /// Find every crate under this directory and serve them all.
    ///
    /// A directory counts as a crate when it holds both `Cargo.toml` and `src/`,
    /// which covers a real Cargo workspace and a folder of standalone crates
    /// alike. Each crate's package name becomes its origin tag, and build output
    /// is skipped, so a vendored copy under `target/` is never picked up.
    ///
    /// Combines with --source and --sources-from; roots already listed are not
    /// added twice.
    ///
    /// Example:
    ///   quartz-ctx serve --discover . --name MyWorkspace
    #[arg(long)]
    discover: Option<PathBuf>,
}

/// One entry of a sources manifest (cortex's `index-sources.json` shape).
#[derive(serde::Deserialize)]
struct SourceTarget {
    source: String,
    #[serde(default)]
    scope: Option<String>,
    /// Index this target's non-`pub` items too.
    ///
    /// Set it for applications and binaries, which publish almost nothing:
    /// measured on this workspace, `pub`-only shows 19% of quartz_forge and 33%
    /// of cortex, against 55% of the quartz engine. Libraries should leave it
    /// off so their indexed surface stays the API they actually promise.
    #[serde(default)]
    include_private: bool,
}

#[derive(serde::Deserialize)]
struct SourceManifest {
    targets: Vec<SourceTarget>,
}

/// Read a sources manifest into (path, origin-tag) pairs.
/// A target's `scope` becomes its origin tag so quartz-ctx origins line up with
/// cortex scopes; unscoped targets fall back to the directory-derived slug.
fn load_source_manifest(path: &Path) -> Result<Vec<(PathBuf, Option<String>, bool)>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read sources manifest: {}", path.display()))?;
    let manifest: SourceManifest = serde_json::from_str(&raw)
        .with_context(|| format!("malformed sources manifest: {}", path.display()))?;

    Ok(manifest
        .targets
        .into_iter()
        .map(|t| {
            (
                PathBuf::from(t.source),
                t.scope.filter(|s| !s.is_empty()),
                t.include_private,
            )
        })
        .collect())
}

// ── selfcheck ────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
struct SelfcheckArgs {
    /// Source directory to validate (must contain .rs files).
    #[arg(short, long, default_value = "src")]
    source: PathBuf,

    /// Engine / stack name shown in startup recommendations.
    #[arg(short, long, default_value = "Quartz")]
    name: String,

    /// Emit machine-readable diagnostics to stdout.
    #[arg(long)]
    json: bool,

    /// Index items that are not `pub`. See `serve --include-private`.
    #[arg(long)]
    include_private: bool,
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = parse_cli_with_diagnostics()?;

    match cli.command {
        Command::Generate(args) => run_generate(args),
        Command::Serve(args)    => run_serve(args),
        Command::Selfcheck(args)=> run_selfcheck(args),
        Command::Boundaries(args)=> run_boundaries(args),
    }
}

fn parse_cli_with_diagnostics() -> Result<Cli> {
    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(err) => {
            let kind = err.kind();
            let argv: Vec<String> = std::env::args().collect();
            let has_mode = argv.iter().any(|a| a == "serve" || a == "generate" || a == "selfcheck");
            let has_serve_flags = argv.iter().any(|a| a == "--source" || a == "-s" || a == "--name" || a == "-n");

            let _ = err.print();

            if !has_mode && has_serve_flags {
                eprintln!(
                    "hint: quartz-ctx requires an explicit subcommand. For MCP use:\n  quartz-ctx serve --source <path> --name <engine>"
                );
                eprintln!(
                    "hint: in .vscode/mcp.json, args should start with \"serve\" before --source/--name"
                );
            }

            let exit_code = match kind {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
            std::process::exit(exit_code);
        }
    }
}

fn run_generate(args: GenerateArgs) -> Result<()> {
    eprintln!("quartz-ctx generate: scanning {}", args.source.display());

    let opts = parser::ParseOptions { include_private: args.include_private };
    let items = parser::parse_dir_with(&args.source, opts)
        .with_context(|| format!("failed to parse source dir: {}", args.source.display()))?;

    let counts = summarise(&items);
    eprintln!(
        "  found {} items  (structs: {}  enums: {}  traits: {}  fns: {}  other: {})",
        items.len(), counts.structs, counts.enums, counts.traits, counts.fns, counts.other,
    );

    if args.dry_run {
        eprintln!("\ndry-run: listing extracted items\n");
        for item in &items {
            println!("  {:10}  {:30}  {}", item.kind.label(), item.name, item.doc_summary());
        }
        return Ok(());
    }

    if items.is_empty() {
        eprintln!("  warning: no public API items found — nothing to write.");
        return Ok(());
    }

    let ctx_dir_name = args.context_dir.unwrap_or_else(|| default_context_dir_name(&args.source));
    let ctx_dir = args.output.join("docs").join(&ctx_dir_name);

    // Mine worked syntax from example programs, tests and benches beside the
    // source root, so the sheets show how the API is called and not only what
    // it looks like.
    let names: std::collections::HashSet<String> =
        items.iter().map(|i| i.name.clone()).collect();
    let usage_sources = usage::discover_sources(&args.source);
    let usage = usage::harvest(&usage_sources, &names);
    let with_usage = usage.len();
    eprintln!("  usage: {} item(s) have worked examples (from {} source path(s))",
              with_usage, usage_sources.len());

    let context = render::context::render(&items, &args.name, &ctx_dir, &usage)?;

    for (path, content) in &context.files {
        if args.minimal {
            let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if matches!(fname, "types.md" | "traits.md" | "functions.md" | "misc.md") {
                continue;
            }
        }
        write_file(path, content.clone())?;
        eprintln!("  wrote {}", path.display());
    }

    eprintln!("\ndone. Add to your .github/copilot-instructions.md:\n");
    eprintln!("  Before writing {} code, review `docs/{}/INDEX.md`", args.name, ctx_dir_name);
    eprintln!("  and the relevant files in `docs/{}/` for available types,", ctx_dir_name);
    eprintln!("  enum variants, and API constraints.");
    eprintln!();
    eprintln!("  To also enable live skill access, add to .vscode/mcp.json:");
    eprintln!("  {{");
    eprintln!("    \"servers\": {{");
    eprintln!("      \"{}\": {{", ctx_dir_name);
    eprintln!("        \"type\": \"stdio\",");
    eprintln!("        \"command\": \"quartz-ctx\",");
    eprintln!("        \"args\": [\"serve\", \"--source\", \"{}\", \"--name\", \"{}\"]", args.source.display(), args.name);
    eprintln!("      }}");
    eprintln!("    }}");
    eprintln!("  }}");

    Ok(())
}

/// Everything `serve` was told to load, before any of it is checked against
/// the disk.
///
/// Kept whole rather than resolved once and discarded, because a source root
/// can arrive after the server does: a workspace wired up before its code is
/// cloned, a manifest edited mid-session, a tree checked out while the editor
/// stays open. Holding the plan lets the running server resolve it again and
/// pick those up without a restart.
#[derive(Clone)]
pub struct SourcePlan {
    /// Roots named with `--source`. First, so the primary engine stays primary.
    explicit: Vec<PathBuf>,
    manifest: Option<PathBuf>,
    discover: Option<PathBuf>,
    include_private: bool,
}

/// A `SourcePlan` measured against what is actually on disk right now.
pub struct Resolved {
    /// Roots that exist: (path, origin tag, include non-pub items).
    pub sources: Vec<(PathBuf, String, bool)>,
    /// Roots that were configured and are not there.
    pub missing: Vec<PathBuf>,
    /// Why the manifest contributed nothing, when one was named.
    pub manifest_error: Option<String>,
}

impl SourcePlan {
    fn from_args(args: &ServeArgs) -> Self {
        Self {
            explicit: args.source.clone(),
            manifest: args.sources_from.clone(),
            discover: args.discover.clone(),
            include_private: args.include_private,
        }
    }

    /// Resolve against the disk. Never fails: a plan that resolves to nothing
    /// is a state the server reports, not a reason to refuse to start.
    pub fn resolve(&self) -> Resolved {
        // Per-root: (path, origin tag, include non-pub items).
        // A --source root inherits the command-line --include-private; a
        // manifest root uses its own declaration, so a library and an
        // application can be served from one server with the correct view of
        // each.
        let mut requested: Vec<(PathBuf, Option<String>, bool)> = self
            .explicit
            .iter()
            .map(|p| (p.clone(), None, self.include_private))
            .collect();

        // A manifest that cannot be read or parsed is reported, not fatal.
        //
        // It used to be `?`, which took the whole server down before the MCP
        // handshake — and the host shows that as a bare connection failure with
        // no cause. The two ways to get there are both ordinary: a workspace
        // set up before `.cortex/index-sources.json` was written, and a manifest
        // someone is part-way through editing. Neither should cost you every
        // tool on the server; the text below says exactly what is wrong.
        let mut manifest_error = None;
        if let Some(manifest_path) = &self.manifest {
            match load_source_manifest(manifest_path) {
                Ok(from_manifest) => {
                    for (path, scope, include_private) in from_manifest {
                        let already = requested.iter().any(|(p, _, _)| paths_equal(p, &path));
                        if !already {
                            requested.push((path, scope, include_private || self.include_private));
                        }
                    }
                }
                Err(e) => manifest_error = Some(format!("{e:#}")),
            }
        }

        if let Some(root) = &self.discover {
            for c in discover::discover_crates(root) {
                if requested.iter().any(|(p, _, _)| paths_equal(p, &c.src)) {
                    continue;
                }
                requested.push((c.src, Some(c.scope), self.include_private));
            }
        }

        if requested.is_empty() {
            requested.push((PathBuf::from("src"), None, self.include_private));
        }

        resolve_requested(&requested, manifest_error)
    }
}

/// Existence-check every requested root and assign each survivor an origin tag.
fn resolve_requested(
    requested: &[(PathBuf, Option<String>, bool)],
    manifest_error: Option<String>,
) -> Resolved {
    let mut sources: Vec<(PathBuf, String, bool)> = Vec::new();
    let mut missing: Vec<PathBuf> = Vec::new();
    for (src, scope, include_private) in requested.iter() {
        // Every missing root is recorded and skipped, wherever it sits in the
        // list — and a plan where NONE of them exist is still a running server.
        //
        // This used to make index 0 alone fatal, which gave a manifest's ORDER a
        // meaning it does not have: a workspace part-way through a migration --
        // some trees checked out, others not yet -- got a server that exited
        // before the MCP handshake purely because an absent root happened to be
        // listed first. Making every root non-fatal fixed that case and left the
        // harder one: a workspace where nothing is checked out yet, which is
        // every FRESH INSTALL, because the shipped template manifest lists
        // example paths (my_engine/src, ...) that exist nowhere. Setup would
        // finish, print success, and hand over a server that could not start.
        //
        // The host reports either as a vague connection failure, and nothing
        // else disagrees: `cortex reindex` warns and carries on over the same
        // manifest, and `check-mcp` passes because both config files really are
        // identical. Nothing on disk looks wrong; the server is simply dead.
        //
        // So an empty resolve is a STATE, not an error: the server starts, and
        // every tool answers with what is missing and how to fix it (see
        // degraded_notice). An empty index that says nothing is the one outcome
        // worse than not starting, because "no such item" then reads as "your
        // code does not contain that".
        if !src.exists() {
            missing.push(src.clone());
            continue;
        }
        // A manifest scope is an explicit, stable origin tag. Use it verbatim
        // rather than slugifying: a scope is already an identifier, and slugify
        // rewrites `_` to `-`, which would turn cortex's `path_forge` scope into
        // origin `path-forge` and break the very correspondence this is for.
        let mut tag = match scope {
            Some(s) => s.trim().to_lowercase(),
            None => default_context_dir_name(src),
        };
        if sources.iter().any(|(_, t, _)| *t == tag) {
            // Two roots want the same tag — e.g. quartz/src and
            // synful_quartz/quartz/src both resolve to "quartz", or two branches
            // each containing an `avatar_ik` crate.
            //
            // QUALIFY with the parent directory rather than REPLACING the tag
            // with it. Replacing produced `pug-branch` for pug_branch/avatar_ik,
            // then `pug-branch-11` for the next collision — names that identify
            // the branch but not the crate, which is backwards. Keep the crate
            // name and prefix its parent: `pug_branch_avatar_ik`.
            //
            // Sanitised, not slugified: these tags become cortex scopes and are
            // prefixed onto module paths, so they must stay identifier-safe.
            if let Some(parent) = src
                .components()
                .rev()
                .filter(|c| c.as_os_str() != "src")
                .nth(1)
                .map(|c| identifier_safe(&c.as_os_str().to_string_lossy()))
                .filter(|s| !s.is_empty())
            {
                tag = format!("{parent}_{tag}");
            }
            // Still colliding: fall back to a numeric suffix rather than looping.
            let mut n = 2;
            let base = tag.clone();
            while sources.iter().any(|(_, t, _)| *t == tag) {
                tag = format!("{base}_{n}");
                n += 1;
            }
        }
        sources.push((src.clone(), tag, *include_private));
    }

    Resolved { sources, missing, manifest_error }
}

/// What to tell an agent when the index is empty, or `None` when it is not.
///
/// This is the whole reason an empty resolve is allowed to start: served as the
/// answer to every tool call, it turns "no item named `Canvas`" — which reads as
/// a fact about the user's code — into the configuration problem it actually is.
/// Paths are shown next to the working directory they resolve against, because
/// the usual cause is an MCP host whose cwd is not what the config assumed.
pub fn degraded_notice(resolved: &Resolved, items_len: usize) -> Option<String> {
    if items_len > 0 {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());

    let mut out = String::from(
        "quartz-ctx is running with an EMPTY index — this answer says nothing \
         about your code.\n\n",
    );

    if let Some(err) = &resolved.manifest_error {
        out.push_str(&format!("The sources manifest could not be used:\n  {err}\n\n"));
    }

    if resolved.sources.is_empty() {
        out.push_str("No configured source root exists. Tried:\n");
        for p in &resolved.missing {
            out.push_str(&format!("  {}\n", p.display()));
        }
        out.push_str(&format!(
            "\nPaths resolve against the working directory, not the config file.\n\
             Working directory: {cwd}\n\n\
             Fix: point .cortex/index-sources.json at roots that exist here (the\n\
             shipped template lists placeholders like my_engine/src), or pass\n\
             --source/--discover. New roots are picked up within a few seconds;\n\
             no restart needed.\n",
        ));
    } else {
        out.push_str("These roots exist but yielded no API items:\n");
        for (p, tag, private) in &resolved.sources {
            let view = if *private { "project view" } else { "public API only" };
            out.push_str(&format!("  {} (origin: {tag}, {view})\n", p.display()));
        }
        out.push_str(
            "\nFix: for an application, a binary, or any non-Rust root, set\n\
             \"include_private\": true — JavaScript and Python have no `pub`, so a\n\
             public-API-only scan of them returns almost nothing.\n",
        );
    }
    Some(out)
}

fn run_serve(args: ServeArgs) -> Result<()> {
    // All diagnostic output goes to stderr so stdout stays clean for JSON-RPC.
    let plan = SourcePlan::from_args(&args);
    let resolved = plan.resolve();

    if let Some(manifest_path) = &args.sources_from {
        match &resolved.manifest_error {
            Some(e) => eprintln!("warn: sources manifest unusable ({}): {e}", manifest_path.display()),
            None => eprintln!("quartz-ctx serve: manifest {}", manifest_path.display()),
        }
    }
    for path in &resolved.missing {
        eprintln!("warn: skipping missing source: {}", path.display());
    }
    for (path, tag, include_private) in &resolved.sources {
        let view = if *include_private { " [project view: incl. non-pub]" } else { "" };
        eprintln!("quartz-ctx serve: loading {} (origin: {tag}){view}", path.display());
    }

    // A parse failure is per-root and already reported by the parser; an empty
    // result is a state the notice explains, not a reason to exit.
    let items = match parser::load_sources_with(&resolved.sources) {
        Ok(items) => items,
        Err(e) => {
            eprintln!("warn: failed to parse source dirs: {e:#}");
            Vec::new()
        }
    };

    if items.is_empty() {
        eprintln!("warn: empty index — serving diagnostics until a source root appears");
    } else {
        eprintln!(
            "  loaded {} API items from {} source(s) — listening on stdio",
            items.len(),
            resolved.sources.len()
        );
    }

    mcp::serve(items, &args.name, resolved, plan)
}

fn run_selfcheck(args: SelfcheckArgs) -> Result<()> {
    let source_exists = args.source.exists();
    // Same exclusions the parser applies, so the reported file count matches
    // what actually gets parsed.
    let rs_files = if source_exists {
        parser::count_source_files(&args.source)
    } else {
        0
    };

    let (items_count, counts, parse_error) = if source_exists {
        match parser::parse_dir_with(&args.source, parser::ParseOptions { include_private: args.include_private }) {
            Ok(items) => {
                let count = items.len();
                (count, Some(summarise(&items)), None)
            }
            Err(err) => (0, None, Some(err.to_string())),
        }
    } else {
        (0, None, Some("source path does not exist".to_owned()))
    };

    let ok = source_exists && rs_files > 0 && parse_error.is_none() && items_count > 0;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "source": args.source,
                "source_exists": source_exists,
                "rs_files": rs_files,
                "items": items_count,
                "counts": counts.as_ref().map(|c| json!({
                    "structs": c.structs,
                    "enums": c.enums,
                    "traits": c.traits,
                    "functions": c.fns,
                    "other": c.other,
                })),
                "error": parse_error,
                "mcp_args_recommended": ["serve", "--source", args.source.display().to_string(), "--name", args.name],
            }))?
        );
    } else {
        eprintln!("quartz-ctx selfcheck");
        eprintln!("  source: {}", args.source.display());
        eprintln!("  source exists: {}", source_exists);
        eprintln!("  rust files found: {}", rs_files);
        if let Some(c) = counts {
            eprintln!(
                "  api items: {} (structs: {} enums: {} traits: {} fns: {} other: {})",
                items_count, c.structs, c.enums, c.traits, c.fns, c.other
            );
        }
        if let Some(err) = parse_error {
            eprintln!("  parse error: {err}");
        }
        eprintln!(
            "  recommended MCP args: [\"serve\", \"--source\", \"{}\", \"--name\", \"{}\"]",
            args.source.display(),
            args.name
        );
        eprintln!("  status: {}", if ok { "OK" } else { "FAIL" });
    }

    if ok {
        Ok(())
    } else {
        Err(anyhow!("quartz-ctx selfcheck failed"))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn write_file(path: &std::path::Path, content: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create dir: {}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("could not write: {}", path.display()))
}

struct Counts { structs: usize, enums: usize, traits: usize, fns: usize, other: usize }

fn summarise(items: &[model::ApiItem]) -> Counts {
    use model::ItemKind::*;
    let mut c = Counts { structs: 0, enums: 0, traits: 0, fns: 0, other: 0 };
    for i in items {
        match i.kind {
            Struct   => c.structs += 1,
            Enum     => c.enums   += 1,
            Trait    => c.traits  += 1,
            Function => c.fns     += 1,
            _        => c.other   += 1,
        }
    }
    c
}

fn default_context_dir_name(source: &Path) -> String {
    let candidate = if source.file_name().and_then(|name| name.to_str()) == Some("src") {
        source
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .map(|name| name.to_owned())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|cwd| {
                        cwd.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_owned())
                    })
            })
    } else {
        source.file_name().and_then(|name| name.to_str()).map(|name| name.to_owned())
    };

    slugify(candidate.as_deref().unwrap_or("docs"))
}

/// Compare two source paths for "same root" purposes.
/// Normalises separators and trailing slashes so `quartz/src`, `quartz\src` and
/// `quartz/src/` all match — an explicit --source must suppress the manifest's
/// duplicate of the same root regardless of how either was spelled.
fn paths_equal(a: &Path, b: &Path) -> bool {
    fn norm(p: &Path) -> String {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    }
    norm(a) == norm(b)
}

/// Identifier-safe form of a name: lowercase, non-alphanumerics to underscore.
/// Used for origin tags, which become cortex scopes and are prefixed onto module
/// paths — so unlike  this preserves `_` rather than turning it into `-`.
fn identifier_safe(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn slugify(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '-' => ch,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("quartz-ctx-tests").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_parses_cortex_index_sources_shape() {
        let dir = tmp("manifest");
        let path = dir.join("index-sources.json");
        std::fs::write(&path, r#"{
            "targets": [
                { "source": "quartz/src", "name": "FlowMake", "scope": null },
                { "source": "path_forge/src", "name": "PF", "scope": "path_forge" }
            ]
        }"#).unwrap();

        let got = load_source_manifest(&path).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, PathBuf::from("quartz/src"));
        assert_eq!(got[0].1, None, "a null scope must not become an origin tag");
        assert_eq!(got[1].1, Some("path_forge".to_string()));
    }

    /// A scope is already an identifier. Slugifying it would rewrite `_` to `-`
    /// and break the correspondence with cortex's scope names.
    #[test]
    fn scope_is_not_slugified_into_a_different_identifier() {
        assert_eq!(slugify("path_forge"), "path-forge", "slugify still mangles underscores");
        // The serve path lowercases and trims instead — verify the intended result.
        let tag = "path_forge".trim().to_lowercase();
        assert_eq!(tag, "path_forge");
    }

    #[test]
    fn path_comparison_ignores_separator_and_trailing_slash() {
        assert!(paths_equal(Path::new("quartz/src"), Path::new(r"quartz\src")));
        assert!(paths_equal(Path::new("quartz/src/"), Path::new("quartz/src")));
        assert!(paths_equal(Path::new("Quartz/Src"), Path::new("quartz/src")));
        assert!(!paths_equal(Path::new("quartz/src"), Path::new("path_forge/src")));
    }

    #[test]
    fn malformed_manifest_reports_the_file_it_failed_on() {
        let dir = tmp("bad");
        let path = dir.join("broken.json");
        std::fs::write(&path, "{ not json").unwrap();

        let err = load_source_manifest(&path).unwrap_err().to_string();
        assert!(err.contains("broken.json"), "error should name the file: {err}");
    }

    /// Helper: a plan naming one manifest and nothing else.
    fn plan_for(manifest: Option<PathBuf>, explicit: Vec<PathBuf>) -> SourcePlan {
        SourcePlan { explicit, manifest, discover: None, include_private: false }
    }

    /// A workspace where nothing the manifest names is checked out — which is
    /// every fresh install, because the shipped template lists placeholders.
    /// It must resolve to a servable (if empty) state, never to an error.
    #[test]
    fn a_plan_whose_roots_all_missing_still_resolves() {
        let dir = tmp("all-missing");
        let path = dir.join("index-sources.json");
        std::fs::write(
            &path,
            r#"{ "targets": [ { "source": "my_engine/src" }, { "source": "my_cli/src" } ] }"#,
        )
        .unwrap();

        let resolved = plan_for(Some(path), vec![]).resolve();
        assert!(resolved.sources.is_empty());
        assert_eq!(resolved.missing.len(), 2, "both roots should be recorded as missing");
        assert!(resolved.manifest_error.is_none(), "the manifest itself parsed fine");
    }

    /// An empty index must answer with what is wrong, not with silence: a bare
    /// "no such item" reads as a fact about the user's code.
    #[test]
    fn an_empty_index_produces_an_actionable_notice() {
        let resolved = Resolved {
            sources: vec![],
            missing: vec![PathBuf::from("my_engine/src")],
            manifest_error: None,
        };
        let notice = degraded_notice(&resolved, 0).expect("empty index must explain itself");
        assert!(notice.contains("my_engine/src"), "names the root it tried: {notice}");
        assert!(notice.contains("EMPTY index"), "says the index is empty: {notice}");

        // ...and stays out of the way as soon as there is anything to serve.
        assert!(degraded_notice(&resolved, 1).is_none());
    }

    /// A manifest that cannot be read is reported through the notice, not by
    /// exiting before the MCP handshake — the host shows that as a bare
    /// connection failure with no cause.
    #[test]
    fn an_unreadable_manifest_is_reported_not_fatal() {
        let dir = tmp("no-manifest");
        let resolved = plan_for(Some(dir.join("does-not-exist.json")), vec![]).resolve();

        let err = resolved.manifest_error.clone().expect("should record why");
        assert!(err.contains("does-not-exist.json"), "names the file: {err}");

        let notice = degraded_notice(&resolved, 0).expect("still degraded");
        assert!(notice.contains("does-not-exist.json"), "surfaces it to the agent: {notice}");
    }

    /// Re-resolving picks up a root that arrives after the server started, so a
    /// workspace wired up before its code is cloned recovers without a restart.
    #[test]
    fn a_root_that_appears_later_is_picked_up_on_re_resolve() {
        let dir = tmp("late-root");
        let src = dir.join("late_crate").join("src");
        let plan = plan_for(None, vec![src.clone()]);

        assert!(plan.resolve().sources.is_empty(), "not there yet");

        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub struct Late;\n").unwrap();

        let after = plan.resolve();
        assert_eq!(after.sources.len(), 1, "the same plan now resolves the root");
        assert!(after.missing.is_empty());
    }

    /// The parser must attach impls written in a different file from the type —
    /// the regression that left Canvas with zero of its 115 methods.
    #[test]
    fn parser_attaches_impls_declared_in_other_files() {
        let dir = tmp("crossfile");
        std::fs::write(dir.join("core.rs"),
            "pub struct Widget { pub w: f32 }\nimpl Widget { pub fn new() -> Self { Self { w: 0.0 } } }\n").unwrap();
        std::fs::write(dir.join("ops.rs"),
            "impl Widget { pub fn draw(&self) {} pub fn resize(&mut self, w: f32) {} }\n").unwrap();

        let items = parser::parse_dir(&dir).unwrap();
        let widget = items.iter().find(|i| i.name == "Widget").expect("Widget missing");
        let names: Vec<&str> = widget.methods.iter().map(|m| m.name.as_str()).collect();

        for expected in ["new", "draw", "resize"] {
            assert!(names.contains(&expected), "method `{expected}` dropped; got {names:?}");
        }
    }
}

/// `quartz-ctx boundaries` — the cross-language map, from the terminal.
fn run_boundaries(args: BoundariesArgs) -> Result<()> {
    let boundaries = parser::scan_boundaries(&args.source);
    let links = bridge::link(&boundaries);

    let keep = |l: &bridge::Link| args.filter.as_ref().map_or(true, |f| l.matches(f));

    let joined: Vec<_> = links.iter()
        .filter(|l| l.provider.is_some() && !l.consumers.is_empty() && keep(l))
        .collect();
    let dangling: Vec<_> = links.iter()
        .filter(|l| l.provider.is_none() && !l.consumers.is_empty() && keep(l))
        .collect();
    let unused: Vec<_> = links.iter()
        .filter(|l| l.provider.is_some() && l.consumers.is_empty() && keep(l))
        .collect();

    println!("{} joined, {} calls with no route, {} routes with no caller
",
             joined.len(), dangling.len(), unused.len());

    for l in &joined {
        let p = l.provider.as_ref().expect("filtered on provider");
        let flag = if l.method_mismatch { "  [METHOD MISMATCH]" } else { "" };
        println!("{}{}", l.label(), flag);
        println!("    served by {} ({})", p.span, p.language);
        for c in &l.consumers {
            println!("    called from {} ({})", c.span, c.language);
        }
    }
    if !dangling.is_empty() {
        println!("
CALLS WITH NO MATCHING ROUTE");
        for l in &dangling {
            for c in &l.consumers {
                println!("  {}  <- {} ({})", l.label(), c.span, c.language);
            }
        }
    }
    if !unused.is_empty() {
        println!("
ROUTES NOT CALLED FROM INDEXED CODE ({})", unused.len());
        for l in unused.iter().take(30) {
            let p = l.provider.as_ref().expect("filtered on provider");
            println!("  {}  ({})", l.label(), p.span);
        }
    }
    Ok(())
}
