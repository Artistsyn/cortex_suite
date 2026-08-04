/// Phase 0C: VS Code Copilot session store reader.
///
/// Read-only companion to the VS Code internal SQLite DB that stores
/// every Copilot chat turn. We never write to it.
///
/// Path discovery: scans %APPDATA%\Code\User\workspaceStorage\*\GitHub.copilot-chat\
///                 chat-session-resources\state.db and returns the most recently
///                 modified file.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

// ── Path discovery ────────────────────────────────────────────────────────────

/// Find the most recently modified VS Code Copilot session store DB.
/// Limits scan to the 50 most recently modified workspace folders to avoid
/// long pauses on machines with many workspaces.
pub fn find_session_store() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let ws = PathBuf::from(appdata)
        .join("Code")
        .join("User")
        .join("workspaceStorage");

    if !ws.exists() {
        return None;
    }

    // Collect ALL candidates first (only stat the parent dirs, not the DBs yet).
    let mut folder_mtimes: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ws) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    folder_mtimes.push((entry.path(), mtime));
                }
            }
        }
    }
    // Sort by mtime descending so newest workspaces are checked first.
    folder_mtimes.sort_by(|a, b| b.1.cmp(&a.1));

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for (folder, _) in folder_mtimes.into_iter().take(50) {
        let db = folder
            .join("GitHub.copilot-chat")
            .join("chat-session-resources")
            .join("state.db");
        if db.exists() {
            if let Ok(meta) = std::fs::metadata(&db) {
                if let Ok(mtime) = meta.modified() {
                    candidates.push((db, mtime));
                }
            }
        }
    }

    candidates.into_iter().max_by_key(|(_, m)| *m).map(|(p, _)| p)
}

/// Open the VS Code session store read-only.
pub fn open_readonly(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open session store: {}", path.display()))
}

// ── Turn reading ──────────────────────────────────────────────────────────────

/// Return the N most recent assistant_response texts from the session store.
/// Used by `flush_knowledge_markers` to scan for CORTEX-* tags.
pub fn recent_assistant_responses(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    // The turns table in VS Code session store has:
    // id, session_id, turn_index, user_message, assistant_response, timestamp
    let mut stmt = conn.prepare(
        "SELECT assistant_response FROM turns
         WHERE assistant_response IS NOT NULL AND assistant_response != ''
         ORDER BY timestamp DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Return recent turn pairs (user, assistant) for session snapshot analysis.
pub fn recent_turns(conn: &Connection, limit: usize) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(user_message, ''), COALESCE(assistant_response, '')
         FROM turns
         ORDER BY timestamp DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
