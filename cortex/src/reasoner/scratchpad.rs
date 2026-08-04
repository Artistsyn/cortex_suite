//! Scratchpad — persistent latent state for reasoning tasks.
//! Stores hypotheses, critiques, and confidence scores across sessions.

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use sha2::{Sha256, Digest};
use rusqlite::Connection;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hypothesis {
    pub index: u8,
    pub content: String,
    pub score: f32,           // 0.0–1.0
    pub critiques: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratchpad {
    pub id: String,           // SHA-256 of task text
    pub task: String,
    pub loop_index: u8,
    pub hypotheses: Vec<Hypothesis>,
    pub critique_log: Vec<String>,
    pub confidence: f32,      // 0.0–1.0
    pub halted_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Scratchpad {
    /// Create a new scratchpad scoped to an optional session key.
    /// When `session_key` is provided, it is incorporated into the hash
    /// so that different sessions with the same task get distinct scratchpads.
    pub fn new(task: &str, session_key: Option<&str>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(task.as_bytes());
        if let Some(sk) = session_key {
            hasher.update(b"::session::");
            hasher.update(sk.as_bytes());
        }
        let id = format!("{:x}", hasher.finalize());
        
        let now = Utc::now();
        Self {
            id,
            task: task.to_string(),
            loop_index: 0,
            hypotheses: vec![],
            critique_log: vec![],
            confidence: 0.0,
            halted_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn add_hypothesis(&mut self, index: u8, content: &str) -> crate::Result<()> {
        self.loop_index = index;
        self.hypotheses.push(Hypothesis {
            index,
            content: content.to_string(),
            score: 0.0,
            critiques: vec![],
            created_at: Utc::now(),
        });
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn add_critique(&mut self, critique: &str) -> crate::Result<()> {
        self.critique_log.push(critique.to_string());
        if let Some(hyp) = self.hypotheses.last_mut() {
            hyp.critiques.push(critique.to_string());
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn set_confidence(&mut self, confidence: f32) {
        self.confidence = confidence.max(0.0).min(1.0);
        if let Some(hyp) = self.hypotheses.last_mut() {
            hyp.score = self.confidence;
        }
        self.updated_at = Utc::now();
    }

    pub fn set_halted(&mut self, reason: &str) {
        self.halted_reason = Some(reason.to_string());
        self.updated_at = Utc::now();
    }

    /// Check stability: if last 2 hypotheses have < 5% content diff, they're stable.
    pub fn is_stable(&self) -> bool {
        if self.hypotheses.len() < 2 {
            return false;
        }
        let last = &self.hypotheses[self.hypotheses.len() - 1].content;
        let prev = &self.hypotheses[self.hypotheses.len() - 2].content;
        
        // Simple token-level diff ratio
        let max_len = last.len().max(prev.len());
        if max_len == 0 {
            return true;
        }
        
        let diff = (last.len() as i32 - prev.len() as i32).abs() as f32;
        (diff / max_len as f32) < 0.05
    }
}

/// Initialize scratchpad persistence: ensure table exists.
pub fn init_store(conn: &Connection) -> crate::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS scratchpads (
            id          TEXT PRIMARY KEY,
            task        TEXT NOT NULL,
            state_json  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

/// Load scratchpad from database by task hash (optionally scoped to a session).
pub fn load_from_db(conn: &Connection, task: &str, session_key: Option<&str>) -> crate::Result<Option<Scratchpad>> {
    let mut hasher = Sha256::new();
    hasher.update(task.as_bytes());
    if let Some(sk) = session_key {
        hasher.update(b"::session::");
        hasher.update(sk.as_bytes());
    }
    let id = format!("{:x}", hasher.finalize());
    
    let mut stmt = conn.prepare(
        "SELECT state_json FROM scratchpads WHERE id = ?1"
    )?;
    
    let result = stmt.query_row([&id], |row| {
        let json: String = row.get(0)?;
        serde_json::from_str(&json)
            .map_err(|_e| rusqlite::Error::InvalidQuery)
    });
    
    match result {
        Ok(sp) => Ok(Some(sp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Save scratchpad to database.
pub fn save_to_db(conn: &Connection, scratchpad: &Scratchpad) -> crate::Result<()> {
    let json = serde_json::to_string(scratchpad)?;
    conn.execute(
        "INSERT OR REPLACE INTO scratchpads (id, task, state_json, updated_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            &scratchpad.id,
            &scratchpad.task,
            json,
            scratchpad.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}
