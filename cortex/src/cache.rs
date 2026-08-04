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
pub fn compute_index_version(conn: &Connection) -> Result<String> {
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
