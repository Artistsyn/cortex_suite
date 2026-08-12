/// K/V cache layer with two responsibilities:
///
/// 1. **Response cache** — hashes (tool + args + index_version) → cached response text.
///    Invalidated atomically when the index version changes (i.e. after `cortex index`).
///    Bounded by entry count; LRU eviction when full.
///
/// 2. **Content store** — content-addressed gzip blob store for compressed unit text.
///    Units reference their content by hash rather than storing text inline in every
///    MCP response. Deduplicates near-identical items automatically.
///
/// 3. **Session registry** — in-memory set of (session_id, content_hash) pairs tracking
///    what has already been sent to Copilot this session. Subsequent responses for the
///    same content emit a short reference token instead of re-sending full text.
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

// ── Content store ─────────────────────────────────────────────────────────────

/// Store compressed text content-addressed by SHA-256.
/// Returns the hex hash. If content already exists, just increments ref_count.
pub fn store_content(conn: &Connection, text: &str) -> Result<String> {
    let hash = sha256_hex(text.as_bytes());
    let compressed = gzip(text.as_bytes())?;

    conn.execute(
        "INSERT INTO content_store (hash, content, ref_count)
         VALUES (?1, ?2, 1)
         ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        params![hash, compressed],
    )?;

    Ok(hash)
}

/// Retrieve content by hash. Returns None if not found.
pub fn fetch_content(conn: &Connection, hash: &str) -> Result<Option<String>> {
    let result: Option<Vec<u8>> = conn
        .query_row(
            "SELECT content FROM content_store WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .optional()?;

    match result {
        Some(bytes) => Ok(Some(gunzip(&bytes)?)),
        None => Ok(None),
    }
}

/// Decrement ref_count; delete if it hits zero.
pub fn release_content(conn: &Connection, hash: &str) -> Result<()> {
    conn.execute(
        "UPDATE content_store SET ref_count = ref_count - 1 WHERE hash = ?1",
        params![hash],
    )?;
    conn.execute(
        "DELETE FROM content_store WHERE hash = ?1 AND ref_count <= 0",
        params![hash],
    )?;
    Ok(())
}

// ── Response cache ────────────────────────────────────────────────────────────

/// Cache a tool response. Key is derived from tool name + args + current index version.
/// Evicts oldest entries when over `max_entries`.
pub fn cache_response(
    conn: &Connection,
    tool: &str,
    args_json: &str,
    index_version: &str,
    response: &str,
    max_entries: usize,
) -> Result<()> {
    let key = cache_key(tool, args_json, index_version);
    let compressed = gzip(response.as_bytes())?;

    conn.execute(
        "INSERT INTO response_cache (key, response, index_ver, created_at, hit_count)
         VALUES (?1, ?2, ?3, datetime('now'), 0)
         ON CONFLICT(key) DO UPDATE SET
           response = excluded.response,
           index_ver = excluded.index_ver,
           created_at = datetime('now'),
           hit_count = 0",
        params![key, compressed, index_version],
    )?;

    // LRU eviction: delete oldest entries beyond max_entries
    conn.execute(
        "DELETE FROM response_cache WHERE key IN (
            SELECT key FROM response_cache
            ORDER BY created_at ASC
            LIMIT MAX(0, (SELECT COUNT(*) FROM response_cache) - ?1)
         )",
        params![max_entries as i64],
    )?;

    Ok(())
}

/// Look up a cached response. Returns None on miss or version mismatch.
/// Increments hit_count on hit.
pub fn get_cached_response(
    conn: &Connection,
    tool: &str,
    args_json: &str,
    index_version: &str,
) -> Result<Option<String>> {
    let key = cache_key(tool, args_json, index_version);

    let result: Option<(Vec<u8>, String)> = conn
        .query_row(
            "SELECT response, index_ver FROM response_cache WHERE key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match result {
        Some((bytes, ver)) if ver == index_version => {
            conn.execute(
                "UPDATE response_cache SET hit_count = hit_count + 1 WHERE key = ?1",
                params![key],
            )?;
            Ok(Some(gunzip(&bytes)?))
        }
        _ => Ok(None),
    }
}

/// Flush all cache entries that don't match the current index version.
pub fn invalidate_stale(conn: &Connection, current_version: &str) -> Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM response_cache WHERE index_ver != ?1",
        params![current_version],
    )?;
    Ok(deleted)
}

/// Stats about the cache.
pub struct CacheStats {
    pub entries: i64,
    pub total_hits: i64,
    pub content_blobs: i64,
    pub approx_bytes: i64,
}

pub fn cache_stats(conn: &Connection) -> Result<CacheStats> {
    let entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM response_cache", [], |r| r.get(0))?;
    let total_hits: i64 = conn
        .query_row("SELECT COALESCE(SUM(hit_count), 0) FROM response_cache", [], |r| r.get(0))?;
    let content_blobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_store", [], |r| r.get(0))?;
    let approx_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(response)), 0) FROM response_cache",
            [],
            |r| r.get(0),
        )?;

    Ok(CacheStats { entries, total_hits, content_blobs, approx_bytes })
}

// ── Index version ─────────────────────────────────────────────────────────────

/// Compute a version hash from all indexed unit IDs and their timestamps,
/// plus pattern and anti-pattern row counts (so adding a pattern invalidates cache).
/// Changes whenever anything is re-indexed or a pattern/anti-pattern is added.
/// Cheap to compute.

/// The index version as of now, recomputed at most once a second.
///
/// The version must be sampled PER REQUEST, not once at startup: computed once,
/// it records the state the server booted in, so an edit or a reindex during
/// the session is invisible and every cached answer stays keyed to a world that
/// no longer exists. That is the bug this whole mechanism exists to close, and
/// leaving the call outside the request loop makes the rest of it inert.
///
/// Debounced because the two halves cost ~5.5 ms (hashing code_units) and ~8 ms
/// (walking the source) -- fine once, wasteful on every call. One second bounds
/// staleness far below the ~5 s quartz-ctx takes to notice an edit, so the
/// debounce is never the weakest link in the chain.
pub fn current_index_version(conn: &Connection, repo_root: Option<&std::path::Path>) -> String {
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);
    static LAST: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);

    if let Ok(guard) = LAST.lock() {
        if let Some((at, ref v)) = *guard {
            if at.elapsed() < DEBOUNCE {
                return v.clone();
            }
        }
    }
    let v = compute_index_version(conn, repo_root)
        .unwrap_or_else(|_| "unknown".to_string());
    if let Ok(mut guard) = LAST.lock() {
        *guard = Some((std::time::Instant::now(), v.clone()));
    }
    v
}

/// A fingerprint of the SOURCE ON DISK, as opposed to what was last ingested.
///
/// `index_version` alone hashes `code_units.indexed_at` -- a record of when
/// cortex was last TOLD about the code, not when the code last CHANGED. Edit a
/// source file and skip `reindex` and the version is unchanged, so every cached
/// answer stays valid and cortex reports no staleness. The store cannot detect
/// that it is behind, which is strictly worse than a cache miss: it is a wrong
/// answer delivered with confidence.
///
/// This reaches outside the database and asks the filesystem instead. Per root:
/// the number of `.rs` files and the newest mtime among them. Adding, removing,
/// or touching a file moves the fingerprint; nothing else does.
///
/// The manifest's own bytes are hashed too, so adding or removing a root is
/// itself a staleness event -- indexing is INSERT OR REPLACE with no delete, so
/// a dropped source otherwise leaves orphaned units behind with no signal.
///
/// Returns None when the manifest is missing or unreadable, so a repo that does
/// not use one behaves exactly as it did before.
pub fn source_fingerprint(repo_root: &std::path::Path) -> Option<String> {
    let manifest_path = repo_root.join(".cortex").join("index-sources.json");
    let raw = std::fs::read(&manifest_path).ok()?;

    let mut hasher = Sha256::new();
    hasher.update(b"manifest:");
    hasher.update(&raw);
    hasher.update(b"
");

    let parsed: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let targets = parsed.get("targets").and_then(|t| t.as_array())?;

    for target in targets {
        let Some(src) = target.get("source").and_then(|s| s.as_str()) else { continue };
        let root = repo_root.join(src);
        let (count, newest) = walk_source(&root);
        hasher.update(src.as_bytes());
        hasher.update(b":");
        hasher.update(count.to_string().as_bytes());
        hasher.update(b"@");
        hasher.update(newest.to_string().as_bytes());
        hasher.update(b"
");
    }

    Some(hex::encode(hasher.finalize()))
}

/// The fingerprint of ONE root, for recording what was actually ingested.
///
/// Stamped per root at index time so staleness is attributable: a change in one
/// crate should re-ingest that crate, not rescan the other ten.
pub fn root_fingerprint(root: &std::path::Path) -> String {
    let (count, newest) = walk_source(root);
    let mut h = Sha256::new();
    h.update(count.to_string().as_bytes());
    h.update(b"@");
    h.update(newest.to_string().as_bytes());
    hex::encode(h.finalize())
}

/// `stale_roots`, debounced, plus a notice to show only when the answer changes.
///
/// Returns Some(message) the first time a given stale set is seen and nothing
/// thereafter. That cadence is the whole design:
///
/// - Per call would be noise. Any session that edits code makes a root stale
///   within seconds, so the warning would ride along on nearly every lookup,
///   spending tokens forever and getting tuned out -- the same fate as an
///   instruction that lives only in documentation.
/// - Once per SESSION would be missed. The edit that invalidates the index
///   usually happens mid-session, after any startup notice has scrolled away.
///
/// Once per *change* is timely (the next lookup after the edit says so), honest
/// (it names which roots, so an answer about an untouched crate is not smeared),
/// and cheap (a line, not a tax). Recovering -- reindexing -- resets it, so the
/// next drift is reported again.
pub fn staleness_notice(conn: &Connection, repo_root: &std::path::Path) -> Option<String> {
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);
    // Keyed by repo root. A server serves one repo, but unkeyed process-global
    // state is the kind that works until something else shares the process and
    // then fails as cross-talk that looks like a logic bug.
    type Notified = HashMap<std::path::PathBuf, Vec<String>>;
    static LAST_CHECK: Mutex<Option<(std::time::Instant, Vec<String>)>> = Mutex::new(None);
    static NOTIFIED: Mutex<Option<Notified>> = Mutex::new(None);

    let stale = {
        let cached = LAST_CHECK.lock().ok().and_then(|g| {
            g.as_ref().filter(|(at, _)| at.elapsed() < DEBOUNCE).map(|(_, v)| v.clone())
        });
        match cached {
            Some(v) => v,
            None => {
                let v = stale_roots(conn, repo_root);
                if let Ok(mut g) = LAST_CHECK.lock() {
                    *g = Some((std::time::Instant::now(), v.clone()));
                }
                v
            }
        }
    };

    let mut guard = NOTIFIED.lock().ok()?;
    let notified = guard.get_or_insert_with(HashMap::new);
    let key = repo_root.to_path_buf();
    if stale.is_empty() {
        // Reindexed. Forget, so the next drift is reported rather than swallowed.
        notified.remove(&key);
        return None;
    }
    if notified.get(&key) == Some(&stale) {
        return None;
    }
    notified.insert(key, stale.clone());

    Some(format!(
        "

[stale index] {} changed since it was indexed - answers about it may          predate your edits. Refresh with `.cortex/cortex.ps1 reindex`.          (shown once per change)",
        stale.join(", "),
    ))
}

/// Roots whose source has moved since it was last ingested.
///
/// Compares the live tree against the stamp `cortex index` wrote. An empty list
/// means the store genuinely reflects the code; a non-empty one names exactly
/// which sources to re-ingest.
pub fn stale_roots(conn: &Connection, repo_root: &std::path::Path) -> Vec<String> {
    let manifest = repo_root.join(".cortex").join("index-sources.json");
    let Ok(raw) = std::fs::read(&manifest) else { return Vec::new() };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&raw) else { return Vec::new() };
    let Some(targets) = parsed.get("targets").and_then(|t| t.as_array()) else { return Vec::new() };

    let mut stale = Vec::new();
    for target in targets {
        let Some(src) = target.get("source").and_then(|s| s.as_str()) else { continue };
        let dir = repo_root.join(src);
        if !dir.exists() { continue; }
        let live = root_fingerprint(&dir);
        let key = format!("source_fp:{src}");
        let stamped: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1", rusqlite::params![key], |r| r.get(0),
        ).ok();
        // No stamp means this root predates the mechanism -- silent, not stale,
        // so an existing store does not start shouting on first run.
        if let Some(prev) = stamped {
            if prev != live { stale.push(src.to_string()); }
        }
    }
    stale
}

/// Count `.rs` files under `root` and find the newest mtime, in nanoseconds.
///
/// Build output and vendored trees are pruned. A scanner without ignore rules
/// cannot be pointed at a project root -- it reads generated bindings under
/// `target/` as if they were the project's own API.
fn walk_source(root: &std::path::Path) -> (u64, u128) {
    const PRUNE: &[&str] = &[
        "target", "node_modules", "vendor", "dist", "build", "out",
        ".git", ".venv", "__pycache__", ".next",
    ];
    let mut count: u64 = 0;
    let mut newest: u128 = 0;

    let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        // Never prune the walk root itself, so an explicit --source ./target works.
        if e.depth() == 0 {
            return true;
        }
        !e.file_name().to_str().is_some_and(|n| PRUNE.contains(&n))
    });

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        count += 1;
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                    newest = newest.max(since.as_nanos());
                }
            }
        }
    }
    (count, newest)
}

pub fn compute_index_version(
    conn: &Connection,
    repo_root: Option<&std::path::Path>,
) -> Result<String> {
    let mut hasher = Sha256::new();

    // Hash code_units (existing behavior).
    if let Ok(mut stmt) = conn.prepare("SELECT id, indexed_at FROM code_units ORDER BY id") {
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            if let Ok((id, ts)) = row {
                hasher.update(id.as_bytes());
                hasher.update(b"|");
                hasher.update(ts.as_bytes());
                hasher.update(b"\n");
            }
        }
    }

    // Hash pattern count + latest approved_at — invalidates when a pattern is added.
    if let Ok((count, latest)) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(approved_at), '') FROM patterns",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    ) {
        hasher.update(b"patterns:");
        hasher.update(count.to_string().as_bytes());
        hasher.update(b"@");
        hasher.update(latest.as_bytes());
        hasher.update(b"\n");
    }

    // Hash anti-pattern count + latest added_at.
    if let Ok((count, latest)) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(added_at), '') FROM anti_patterns",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    ) {
        hasher.update(b"anti_patterns:");
        hasher.update(count.to_string().as_bytes());
        hasher.update(b"@");
        hasher.update(latest.as_bytes());
        hasher.update(b"\n");
    }

    // Hash the build identity.
    //
    // Without this the key describes only the DATA, so changing how a tool
    // RENDERS that data leaves every cached answer valid and the rebuilt binary
    // replays the old output. That cost a full false-negative debug cycle on
    // 2026-08-03: a `get_item` ranking fix was verified by unit test, deployed,
    // and appeared not to work over MCP until three stale rows were deleted by
    // hand. A cache keyed on inputs alone cannot see a change in the function.
    // The source on disk, which is the only thing that knows whether the ingest
    // above is current. See source_fingerprint.
    if let Some(root) = repo_root {
        if let Some(fp) = source_fingerprint(root) {
            hasher.update(b"source:");
            hasher.update(fp.as_bytes());
            hasher.update(b"
");
        }
    }

    hasher.update(b"build:");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(b"@");
    hasher.update(build_stamp().as_bytes());
    hasher.update(b"\n");

    Ok(hex::encode(hasher.finalize()))
}

/// A value that changes whenever this binary is rebuilt.
///
/// The executable's own modification time is the most reliable signal available
/// without a build script: it moves on every relink, including a rebuild from
/// identical sources, which is the conservative direction for a cache key.
/// Falls back to the compile timestamp constant if the path cannot be read.
fn build_stamp() -> String {
    std::env::current_exe()
        .and_then(|p| std::fs::metadata(p))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|| "unknown-build".to_string())
}

// ── Session registry (in-memory) ──────────────────────────────────────────────

/// Tracks what content hashes have been sent to each active session.
/// Purely in-memory — sessions are ephemeral and don't need persistence.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if this content hash has already been sent to this session.
    pub fn already_sent(&self, session_id: &str, content_hash: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(session_id)
            .map_or(false, |s| s.contains(content_hash))
    }

    /// Mark content hash as sent for this session.
    pub fn mark_sent(&self, session_id: &str, content_hash: &str) {
        self.inner
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .insert(content_hash.to_string());
    }

    /// Clear a session (called when session ends or on explicit reset).
    pub fn clear_session(&self, session_id: &str) {
        self.inner.lock().unwrap().remove(session_id);
    }

    /// Active session count.
    pub fn session_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

// ── Session-aware response helper ─────────────────────────────────────────────

/// Wraps a list of (hash, compressed_text) pairs into a response,
/// substituting a short reference token for content already seen this session.
///
/// The reference token is:
///   [ref: <8-char prefix of hash>]
///
/// Copilot treats refs as "already in context — no need to re-read."
pub fn render_with_session(
    items: &[(String, String)], // (hash, text)
    session: &SessionRegistry,
    session_id: &str,
) -> String {
    let mut out = String::new();
    let mut new_count = 0;
    let mut ref_count = 0;

    for (hash, text) in items {
        if session.already_sent(session_id, hash) {
            out.push_str(&format!("[ref:{}]\n", &hash[..8]));
            ref_count += 1;
        } else {
            out.push_str(text);
            out.push('\n');
            session.mark_sent(session_id, hash);
            new_count += 1;
        }
    }

    if ref_count > 0 {
        out.push_str(&format!(
            "\n[{} item(s) already in context this session — {} new]\n",
            ref_count, new_count
        ));
    }

    out
}

// ── DB schema additions ───────────────────────────────────────────────────────

/// Additional tables managed by the cache layer.
/// Called during Store::migrate().
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS content_store (
            hash        TEXT PRIMARY KEY,
            content     BLOB NOT NULL,   -- gzip-compressed text
            ref_count   INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS response_cache (
            key         TEXT PRIMARY KEY,
            response    BLOB NOT NULL,   -- gzip-compressed response
            index_ver   TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            hit_count   INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_cache_ver ON response_cache(index_ver);
    ")?;
    Ok(())
}

// ── Maintenance ───────────────────────────────────────────────────────────────

/// Prune MCP call log, keeping only the last `keep` entries.
/// The call log is the main source of unbounded growth.
pub fn prune_call_log(conn: &Connection, keep: usize) -> Result<usize> {
    let pruned = conn.execute(
        "DELETE FROM mcp_calls WHERE id NOT IN (
            SELECT id FROM mcp_calls ORDER BY called_at DESC LIMIT ?1
         )",
        params![keep as i64],
    )?;
    Ok(pruned)
}

/// Run VACUUM to reclaim freed pages. Call after bulk deletes.
pub fn vacuum(conn: &Connection) -> Result<()> {
    conn.execute_batch("VACUUM;")?;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn cache_key(tool: &str, args_json: &str, index_version: &str) -> String {
    let mut h = Sha256::new();
    h.update(tool.as_bytes());
    h.update(b":");
    h.update(args_json.as_bytes());
    h.update(b":");
    h.update(index_version.as_bytes());
    hex::encode(h.finalize())
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn gunzip(data: &[u8]) -> Result<String> {
    let mut dec = GzDecoder::new(data);
    let mut out = String::new();
    dec.read_to_string(&mut out)?;
    Ok(out)
}

// Extension trait so we can call .optional() on rusqlite queries
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod source_fingerprint_tests {
    use super::*;
    use std::path::PathBuf;

    /// The real workspace, so the number reported is the one that matters.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    #[test]
    fn fingerprints_the_live_workspace_and_reports_the_cost() {
        let root = repo_root();
        if !root.join(".cortex").join("index-sources.json").exists() {
            eprintln!("skipped: no index-sources.json at {}", root.display());
            return;
        }

        // Warm the directory cache, then measure the steady state -- that is
        // what a per-request call actually pays.
        let _ = source_fingerprint(&root);

        let runs = 20;
        let start = std::time::Instant::now();
        let mut last = None;
        for _ in 0..runs {
            last = source_fingerprint(&root);
        }
        let per_call = start.elapsed() / runs;

        assert!(last.is_some(), "fingerprint should resolve on the live workspace");
        eprintln!("source_fingerprint: {:?} per call (mean of {runs})", per_call);

        // A per-request cost only makes sense if it is far below the latency of
        // the tool call it guards. 50 ms would be indefensible; single-digit ms
        // is free next to a DB query plus JSON serialisation.
        assert!(
            per_call < std::time::Duration::from_millis(50),
            "too slow to run per request: {per_call:?}"
        );
    }

    #[test]
    fn the_same_tree_fingerprints_the_same() {
        let root = repo_root();
        if !root.join(".cortex").join("index-sources.json").exists() {
            return;
        }
        // Spurious churn would invalidate the cache constantly and cost more
        // than the staleness it is meant to catch.
        assert_eq!(source_fingerprint(&root), source_fingerprint(&root));
    }

    #[test]
    fn a_touched_source_file_moves_the_fingerprint() {
        let dir = std::env::temp_dir().join(format!("cortex_fp_{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":null}]}"#,
        ).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();

        let before = source_fingerprint(&dir).expect("fingerprint");

        // A new file must register: this is the case index_version misses
        // entirely, because nothing has been ingested.
        std::fs::write(src.join("b.rs"), "fn b() {}").unwrap();
        let after_add = source_fingerprint(&dir).expect("fingerprint");
        assert_ne!(before, after_add, "adding a source file must move the fingerprint");

        // And so must a removal -- indexing is INSERT OR REPLACE with no delete,
        // so a dropped file otherwise leaves orphaned units with no signal.
        std::fs::remove_file(src.join("b.rs")).unwrap();
        let after_remove = source_fingerprint(&dir).expect("fingerprint");
        assert_ne!(after_add, after_remove, "removing a source file must move the fingerprint");

        // Changing the root set is a staleness event in its own right.
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":"other"}]}"#,
        ).unwrap();
        assert_ne!(
            after_remove,
            source_fingerprint(&dir).expect("fingerprint"),
            "editing the manifest must move the fingerprint",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_output_is_pruned() {
        let dir = std::env::temp_dir().join(format!("cortex_fp_prune_{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":null}]}"#,
        ).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();
        let before = source_fingerprint(&dir).expect("fingerprint");

        // Generated code under target/ must not churn the key on every build.
        std::fs::create_dir_all(src.join("target").join("debug")).unwrap();
        std::fs::write(src.join("target").join("debug").join("gen.rs"), "fn gen() {}").unwrap();
        assert_eq!(
            before,
            source_fingerprint(&dir).expect("fingerprint"),
            "build output must be pruned, or every cargo build invalidates the cache",
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod version_cost_tests {
    use super::*;
    /// The acceptance test for the whole mechanism: a source edit that nobody
    /// ingested must still move the cache key. Before this, index_version saw
    /// only `code_units.indexed_at` -- so editing code and skipping reindex left
    /// every cached answer valid, and cortex reported no staleness at all.
    #[test]
    fn an_uningested_source_edit_moves_the_index_version() {
        let db = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join(".cortex").join("memory.db");
        if !db.exists() { return; }
        let conn = Connection::open(&db).unwrap();

        let dir = std::env::temp_dir().join(format!("cortex_iv_{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":null}]}"#,
        ).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();

        let before = compute_index_version(&conn, Some(&dir)).unwrap();
        std::fs::write(src.join("b.rs"), "fn b() {}").unwrap();
        let after = compute_index_version(&conn, Some(&dir)).unwrap();

        assert_ne!(before, after, "an uningested source edit must invalidate the cache");

        // And the DB-only version must NOT see it -- that is the gap being closed.
        let db_only_before = compute_index_version(&conn, None).unwrap();
        std::fs::write(src.join("c.rs"), "fn c() {}").unwrap();
        assert_eq!(
            db_only_before,
            compute_index_version(&conn, None).unwrap(),
            "without the source fingerprint the version is blind to source edits",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn what_a_request_now_pays() {
        let db = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join(".cortex").join("memory.db");
        if !db.exists() { return; }
        let conn = Connection::open(&db).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().to_path_buf();
        let _ = current_index_version(&conn, Some(&root));
        let runs = 500;
        let start = std::time::Instant::now();
        for _ in 0..runs { let _ = current_index_version(&conn, Some(&root)); }
        eprintln!("current_index_version (per request): {:?}", start.elapsed() / runs);
    }

    #[test]
    fn how_expensive_is_the_db_half() {
        let db = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join(".cortex").join("memory.db");
        if !db.exists() { eprintln!("skipped: no memory.db"); return; }
        let conn = Connection::open(&db).unwrap();
        let _ = compute_index_version(&conn, None);
        let runs = 10;
        let start = std::time::Instant::now();
        for _ in 0..runs { let _ = compute_index_version(&conn, None); }
        eprintln!("compute_index_version (DB only): {:?} per call", start.elapsed() / runs);
    }
}

#[cfg(test)]
mod stale_root_tests {
    use super::*;

    /// End-to-end: a root whose source moved after ingest must be named, and one
    /// that did not must stay silent. This is the signal that tells an agent its
    /// knowledge is behind the code -- the thing index_version alone could never
    /// see, because it only ever measured when cortex was last told.
    #[test]
    fn a_root_that_moved_since_ingest_is_named_and_a_quiet_one_is_not() {
        let dir = std::env::temp_dir().join(format!("cortex_stale_{}", std::process::id()));
        let a = dir.join("crate_a").join("src");
        let b = dir.join("crate_b").join("src");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[
                {"source":"crate_a/src","name":"a","scope":null},
                {"source":"crate_b/src","name":"b","scope":null}]}"#,
        ).unwrap();
        std::fs::write(a.join("x.rs"), "fn x() {}").unwrap();
        std::fs::write(b.join("y.rs"), "fn y() {}").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();

        // Stamp both, as `cortex index` would.
        for (rel, path) in [("crate_a/src", &a), ("crate_b/src", &b)] {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("source_fp:{rel}"), root_fingerprint(path)],
            ).unwrap();
        }
        assert!(stale_roots(&conn, &dir).is_empty(), "nothing changed yet");

        // Touch only crate_a.
        std::fs::write(a.join("z.rs"), "fn z() {}").unwrap();
        let stale = stale_roots(&conn, &dir);
        assert_eq!(stale, vec!["crate_a/src".to_string()],
            "only the root that moved should be named, got {stale:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unstamped root predates the mechanism. It must stay silent rather than
    /// report stale, or every existing store starts shouting on first run.
    #[test]
    fn an_unstamped_root_is_silent() {
        let dir = std::env::temp_dir().join(format!("cortex_unstamped_{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":null}]}"#,
        ).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        assert!(stale_roots(&conn, &dir).is_empty(), "no stamp means no claim");

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod staleness_notice_tests {
    use super::*;

    struct Fix { dir: std::path::PathBuf, conn: Connection }

    fn fixture(tag: &str) -> Fix {
        let dir = std::env::temp_dir().join(format!("cortex_notice_{}_{}", tag, std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join(".cortex")).unwrap();
        std::fs::write(
            dir.join(".cortex").join("index-sources.json"),
            r#"{"targets":[{"source":"src","name":"t","scope":null}]}"#,
        ).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('source_fp:src', ?1)",
            rusqlite::params![root_fingerprint(&src)],
        ).unwrap();
        Fix { dir, conn }
    }

    /// The cadence IS the design. Per call is noise that gets tuned out; once per
    /// session is missed because the edit happens mid-session. Once per change is
    /// the only one that is both timely and quiet.
    #[test]
    fn fires_once_per_change_not_once_per_call() {
        let f = fixture("cadence");
        assert!(staleness_notice(&f.conn, &f.dir).is_none(), "clean index must be silent");

        std::fs::write(f.dir.join("src").join("b.rs"), "fn b() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // clear the debounce

        let first = staleness_notice(&f.conn, &f.dir);
        assert!(first.is_some(), "the first lookup after an edit must say so");
        assert!(first.as_ref().unwrap().contains("src"), "it must name the root");

        for i in 0..5 {
            assert!(
                staleness_notice(&f.conn, &f.dir).is_none(),
                "repeat call {i} re-warned; a warning on every call is noise",
            );
        }
        std::fs::remove_dir_all(&f.dir).ok();
    }

    /// Recovering must reset it, or the next drift goes unreported.
    #[test]
    fn reindexing_clears_it_and_the_next_drift_reports_again() {
        let f = fixture("recover");
        let src = f.dir.join("src");

        std::fs::write(src.join("b.rs"), "fn b() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(staleness_notice(&f.conn, &f.dir).is_some(), "drift 1 reported");

        // Reindex: re-stamp to the current state.
        f.conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'source_fp:src'",
            rusqlite::params![root_fingerprint(&src)],
        ).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(staleness_notice(&f.conn, &f.dir).is_none(), "reindex must silence it");

        // A second, independent drift must be reported afresh.
        std::fs::write(src.join("c.rs"), "fn c() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(
            staleness_notice(&f.conn, &f.dir).is_some(),
            "a later drift must report again, not be swallowed by the earlier one",
        );
        std::fs::remove_dir_all(&f.dir).ok();
    }
}
