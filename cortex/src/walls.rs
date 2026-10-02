//! Walls: limits, recorded with whose limit they are.
//!
//! The failure this exists for was measured in this workspace's own history
//! (`docs/frontier-plan-2026-09-29.md`). Agents accepted, and then wrote down,
//! limits whose real source was movable: a library default (wgpu's 256 texture
//! layers, reported as a hardware limit), a version (multiview with MSAA), our
//! own API, the desktop frame rate of research code, or an authority ("every
//! shipped Quest title turns them off"). Every one that was later tested did
//! not hold as stated, and the check that moved it was cheap. A limit written
//! into memory is read by the next session as a closed verdict, so the store
//! itself was where limits hardened.
//!
//! A wall here is a claim with a provenance, evidence, and, while it is open,
//! the cheapest test that would decide it. Two rules are enforced rather than
//! documented, because documented rules ran at 2-5% compliance and required
//! parameters at 100%:
//!
//! * A wall cannot be recorded without saying whose limit it is and on what
//!   evidence. Evidence that is only inferred, only an authority, or only the
//!   performance of someone else's implementation leaves it `open`, and an open
//!   wall must name its cheapest decisive test.
//! * A verdict changes only with a new fact: a measurement, or a dated source.
//!   Challenged models flip about 46% of their answers, right or wrong
//!   (FlipFlop), so a status change without one is refused in either direction.

use anyhow::{anyhow, bail, Result};
use chrono::{NaiveDate, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::memory::Store;

pub const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS walls (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        claim         TEXT    NOT NULL,
        topic         TEXT    NOT NULL DEFAULT '',
        provenance    TEXT    NOT NULL,
        status        TEXT    NOT NULL DEFAULT 'open',
        evidence      TEXT    NOT NULL DEFAULT '[]',
        untested      TEXT    NOT NULL DEFAULT '',
        cheapest_test TEXT    NOT NULL DEFAULT '',
        revisit_when  TEXT    NOT NULL DEFAULT '',
        revisit_after TEXT,
        challenge_id  INTEGER,
        links         TEXT    NOT NULL DEFAULT '',
        created_at    TEXT    NOT NULL,
        updated_at    TEXT    NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_walls_claim ON walls(lower(claim));
";

/// Whose limit is it: `(key, label, movable)`. A limit from a movable class is
/// a work item with a cost, not a wall.
pub const PROVENANCES: &[(&str, &str, bool)] = &[
    ("physics", "physics or information bound", false),
    ("hardware", "hardware capability", false),
    ("platform", "platform or OS policy", false),
    ("library-default", "library default or configuration", true),
    ("library-version", "library version", true),
    ("our-design", "our own API or design", true),
    ("implementation", "existing implementations", true),
    ("authority", "authority or consensus", true),
    ("budget", "budget", true),
];

/// Evidence kinds. Only `measured`, `vendor-doc` and `paper` are facts that can
/// settle or change a verdict; the two sources count only with a date.
pub const EVIDENCE_KINDS: &[&str] =
    &["measured", "vendor-doc", "paper", "implementation", "authority", "inferred"];

pub const STATUSES: &[&str] = &["open", "holds", "moved", "retired"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub date: String,
}

#[derive(Debug, Clone, Default)]
pub struct Wall {
    pub id: i64,
    pub claim: String,
    pub topic: Vec<String>,
    pub provenance: String,
    pub status: String,
    pub evidence: Vec<Evidence>,
    pub untested: String,
    pub cheapest_test: String,
    pub revisit_when: String,
    pub revisit_after: Option<String>,
    pub challenge_id: Option<i64>,
    pub links: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A wall as a caller asks to record it, before validation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewWall {
    pub claim: String,
    pub provenance: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub topic: Vec<String>,
    #[serde(default)]
    pub untested: String,
    #[serde(default)]
    pub cheapest_test: String,
    #[serde(default)]
    pub revisit_when: String,
    #[serde(default)]
    pub revisit_after: Option<String>,
    #[serde(default)]
    pub challenge_id: Option<i64>,
    #[serde(default)]
    pub links: String,
}

/// Changes to an existing wall. `None` leaves a field as it is; new evidence is
/// appended, never replaces.
#[derive(Debug, Clone, Default)]
pub struct WallUpdate {
    pub status: Option<String>,
    pub provenance: Option<String>,
    pub evidence: Vec<Evidence>,
    pub topic: Option<Vec<String>>,
    pub untested: Option<String>,
    pub cheapest_test: Option<String>,
    pub revisit_when: Option<String>,
    pub revisit_after: Option<String>,
    pub links: Option<String>,
}

// ── Vocabulary ────────────────────────────────────────────────────────────────

fn slug(s: &str) -> String {
    s.trim().to_lowercase().replace(['_', ' '], "-")
}

/// The provenance class for a caller's spelling, or `None`.
pub fn provenance_key(s: &str) -> Option<&'static str> {
    let k = slug(s);
    let alias = match k.as_str() {
        "physical" | "information" | "information-bound" | "physics-bound" => "physics",
        "hardware-capability" | "device" | "silicon" | "gpu" => "hardware",
        "os" | "os-policy" | "platform-policy" | "policy" | "driver" | "spec" | "format" => "platform",
        "default" | "config" | "configuration" | "library-config" => "library-default",
        "version" | "library" => "library-version",
        "design" | "api" | "our-api" | "our-code" | "ownership" => "our-design",
        "implementations" | "existing-implementation" | "existing-implementations"
        | "research-code" => "implementation",
        "consensus" | "expert" | "experts" | "research-says" => "authority",
        "cost" | "frame-budget" => "budget",
        other => other,
    };
    PROVENANCES.iter().find(|(key, _, _)| *key == alias).map(|(key, _, _)| *key)
}

pub fn provenance_label(key: &str) -> &'static str {
    PROVENANCES.iter().find(|(k, _, _)| *k == key).map(|(_, l, _)| *l).unwrap_or("unclassified")
}

pub fn is_movable(key: &str) -> bool {
    PROVENANCES.iter().any(|(k, _, m)| *k == key && *m)
}

fn provenance_help(given: &str) -> String {
    let classes: Vec<String> =
        PROVENANCES.iter().map(|(k, l, _)| format!("{k} ({l})")).collect();
    format!(
        "`provenance` must say whose limit this is (got {given:?}). One of: {}. \
         The last six are movable: a work item with a cost, not a wall.",
        classes.join(", ")
    )
}

fn evidence_kind(s: &str) -> Option<&'static str> {
    let k = slug(s);
    let alias = match k.as_str() {
        "measured-on-device" | "measured-on-target" | "measurement" | "measure" | "test"
        | "tested" | "benchmark" => "measured",
        "doc" | "docs" | "vendor" | "vendor-docs" | "documentation" | "spec" | "release"
        | "release-notes" => "vendor-doc",
        "research" | "article" | "study" => "paper",
        "existing-implementation" | "implementations" | "code" => "implementation",
        "consensus" | "expert" | "experts" => "authority",
        "inference" | "reasoned" | "reasoning" | "estimate" => "inferred",
        other => other,
    };
    EVIDENCE_KINDS.iter().find(|k| **k == alias).copied()
}

/// Can this evidence settle or change a verdict?
pub fn is_fact(e: &Evidence) -> bool {
    match e.kind.as_str() {
        "measured" => true,
        "vendor-doc" | "paper" => !e.date.is_empty(),
        _ => false,
    }
}

fn status_key(s: &str) -> Result<&'static str> {
    let k = slug(s);
    let k = match k.as_str() {
        "held" | "hold" | "confirmed" => "holds",
        "move" | "broken" | "fell" => "moved",
        "retire" => "retired",
        other => other,
    };
    STATUSES
        .iter()
        .find(|s| **s == k)
        .copied()
        .ok_or_else(|| anyhow!("unknown status {s:?}: use open, holds, moved or retired"))
}

/// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
fn valid_date(d: &str) -> bool {
    let d = d.trim();
    match d.len() {
        4 => d.parse::<u16>().is_ok(),
        7 => NaiveDate::parse_from_str(&format!("{d}-01"), "%Y-%m-%d").is_ok(),
        10 => NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok(),
        _ => false,
    }
}

fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

fn normalize_evidence(items: Vec<Evidence>) -> Result<Vec<Evidence>> {
    let mut out = Vec::with_capacity(items.len());
    for e in items {
        let kind = evidence_kind(&e.kind).ok_or_else(|| {
            anyhow!(
                "evidence kind {:?} is not one of: {}",
                e.kind,
                EVIDENCE_KINDS.join(", ")
            )
        })?;
        let text = e.text.trim().to_string();
        if text.chars().count() < 8 {
            bail!("evidence text is too short to be checked later: {:?}", e.text);
        }
        let mut date = e.date.trim().to_string();
        if !date.is_empty() && !valid_date(&date) {
            bail!("evidence date {date:?} must be YYYY, YYYY-MM or YYYY-MM-DD");
        }
        if matches!(kind, "vendor-doc" | "paper") && date.is_empty() {
            bail!(
                "date the {kind} ({text:?}): YYYY, YYYY-MM or YYYY-MM-DD. An undated source \
                 cannot settle a limit, and stale sources are how version walls survive"
            );
        }
        if kind == "measured" && date.is_empty() {
            date = today();
        }
        out.push(Evidence { kind: kind.to_string(), text, source: e.source.trim().to_string(), date });
    }
    Ok(out)
}

fn clean_topic(topic: &[String]) -> Vec<String> {
    let mut v: Vec<String> = topic
        .iter()
        .flat_map(|t| t.split(','))
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ── Storage ───────────────────────────────────────────────────────────────────

const COLUMNS: &str = "id, claim, topic, provenance, status, evidence, untested, cheapest_test, \
                       revisit_when, revisit_after, challenge_id, links, created_at, updated_at";

fn from_row(r: &Row) -> rusqlite::Result<Wall> {
    let topic: String = r.get(2)?;
    let evidence: String = r.get(5)?;
    Ok(Wall {
        id: r.get(0)?,
        claim: r.get(1)?,
        topic: topic.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
        provenance: r.get(3)?,
        status: r.get(4)?,
        // A row that fails to parse keeps its claim; losing one wall's evidence
        // must never take the listing down with it.
        evidence: serde_json::from_str(&evidence).unwrap_or_default(),
        untested: r.get(6)?,
        cheapest_test: r.get(7)?,
        revisit_when: r.get(8)?,
        revisit_after: r.get(9)?,
        challenge_id: r.get(10)?,
        links: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

pub fn get(store: &Store, id: i64) -> Result<Option<Wall>> {
    Ok(store
        .conn()
        .query_row(&format!("SELECT {COLUMNS} FROM walls WHERE id = ?1"), params![id], from_row)
        .optional()?)
}

pub fn all(store: &Store) -> Result<Vec<Wall>> {
    let mut st = store.conn().prepare(&format!("SELECT {COLUMNS} FROM walls ORDER BY id"))?;
    let rows = st.query_map([], from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn find_by_claim(store: &Store, claim: &str) -> Result<Option<i64>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT id FROM walls WHERE lower(claim) = lower(?1)",
            params![claim.trim()],
            |r| r.get(0),
        )
        .optional()?)
}

/// Record a new wall. Returns it with any notes the caller should see (e.g.
/// that it was stored as `open` rather than the status asked for).
pub fn record(store: &Store, new: NewWall) -> Result<(Wall, Vec<String>)> {
    let claim = new.claim.split_whitespace().collect::<Vec<_>>().join(" ");
    if claim.chars().count() < 12 {
        bail!(
            "`claim` must state the limit as a measurable claim (what, on what, at what \
             budget); got {claim:?}"
        );
    }
    let provenance =
        provenance_key(&new.provenance).ok_or_else(|| anyhow!(provenance_help(&new.provenance)))?;
    let evidence = normalize_evidence(new.evidence)?;
    if evidence.is_empty() {
        bail!(
            "a wall needs at least one evidence item ({{kind, text, source?, date?}}; kinds: {}). \
             A limit with no evidence is a guess, and guesses are what this ledger exists to catch",
            EVIDENCE_KINDS.join(", ")
        );
    }
    let mut notes = Vec::new();
    let asked = match new.status.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) => status_key(s)?,
        None => "open",
    };
    let status = if matches!(asked, "holds" | "moved") && !evidence.iter().any(is_fact) {
        notes.push(format!(
            "stored as `open`, not `{asked}`: its evidence is only {}. A measurement, or a \
             dated vendor doc or paper, settles it.",
            kinds_of(&evidence)
        ));
        "open"
    } else {
        asked
    };
    let cheapest_test = new.cheapest_test.trim().to_string();
    if status == "open" && cheapest_test.is_empty() {
        bail!(
            "an open wall needs `cheapest_test`: the cheapest check that would decide it, with \
             a time estimate. Naming it is the point of recording the wall"
        );
    }
    if let Some(d) = new.revisit_after.as_deref().filter(|d| !d.trim().is_empty()) {
        if NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").is_err() {
            bail!("`revisit_after` must be YYYY-MM-DD; got {d:?}");
        }
    }
    if let Some(id) = find_by_claim(store, &claim)? {
        bail!("already recorded as wall #{id}; change it with update_wall(id={id}, ...)");
    }
    if is_movable(provenance) && status == "holds" {
        notes.push(format!(
            "`{provenance}` is a movable class: this limit holds only until someone pays to move \
             it. Name that cost in `untested` or `cheapest_test`."
        ));
    }
    let now = Utc::now().to_rfc3339();
    store.conn().execute(
        "INSERT INTO walls (claim, topic, provenance, status, evidence, untested, cheapest_test,
                            revisit_when, revisit_after, challenge_id, links, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            claim,
            clean_topic(&new.topic).join(","),
            provenance,
            status,
            serde_json::to_string(&evidence)?,
            new.untested.trim(),
            cheapest_test,
            new.revisit_when.trim(),
            new.revisit_after.as_deref().map(str::trim).filter(|d| !d.is_empty()),
            new.challenge_id,
            new.links.trim(),
            now,
        ],
    )?;
    let id = store.conn().last_insert_rowid();
    if let Some(c) = new.challenge_id {
        link_challenge(store, id, c)?;
    }
    let wall = get(store, id)?.ok_or_else(|| anyhow!("wall #{id} vanished after insert"))?;
    Ok((wall, notes))
}

/// Change a wall. A new status or provenance needs new evidence, and a verdict
/// (holds/moved) needs a new FACT: a measurement or a dated source.
pub fn update(store: &Store, id: i64, upd: WallUpdate) -> Result<(Wall, Vec<String>)> {
    let current = get(store, id)?.ok_or_else(|| anyhow!("no wall #{id}"))?;
    let new_evidence = normalize_evidence(upd.evidence)?;
    let mut notes = Vec::new();

    let status = match upd.status.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) => status_key(s)?,
        None => STATUSES.iter().find(|s| **s == current.status).copied().unwrap_or("open"),
    };
    if status != current.status {
        match status {
            "holds" | "moved" if !new_evidence.iter().any(is_fact) => bail!(
                "wall #{id} is `{}`; changing it to `{status}` needs a new fact: a `measured` \
                 item, or a `vendor-doc` or `paper` with its date. Reconsidering is not a fact - \
                 challenged models flip about half their answers either way",
                current.status
            ),
            _ if new_evidence.is_empty() => bail!(
                "changing wall #{id} from `{}` to `{status}` needs a new evidence item saying why",
                current.status
            ),
            _ => {}
        }
    }
    let provenance = match upd.provenance.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => {
            let key = provenance_key(p).ok_or_else(|| anyhow!(provenance_help(p)))?;
            if key != current.provenance && new_evidence.is_empty() {
                bail!("reclassifying wall #{id} as `{key}` needs a new evidence item saying why");
            }
            key.to_string()
        }
        None => current.provenance.clone(),
    };
    let cheapest_test = upd.cheapest_test.unwrap_or(current.cheapest_test).trim().to_string();
    if status == "open" && cheapest_test.is_empty() {
        bail!("an open wall needs `cheapest_test`: the cheapest check that would decide it");
    }
    let revisit_after = match upd.revisit_after {
        Some(d) if d.trim().is_empty() => None,
        Some(d) => {
            if NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").is_err() {
                bail!("`revisit_after` must be YYYY-MM-DD; got {d:?}");
            }
            Some(d.trim().to_string())
        }
        None => current.revisit_after.clone(),
    };
    let mut evidence = current.evidence.clone();
    evidence.extend(new_evidence);
    let topic = match upd.topic {
        Some(t) => clean_topic(&t),
        None => current.topic.clone(),
    };
    if status == "moved" && current.status != "moved" {
        notes.push(
            "a limit that moved is worth recording as a pattern or anti-pattern too, so the \
             way it moved is findable by name"
                .to_string(),
        );
    }
    store.conn().execute(
        "UPDATE walls SET status = ?2, provenance = ?3, evidence = ?4, topic = ?5,
                          untested = ?6, cheapest_test = ?7, revisit_when = ?8,
                          revisit_after = ?9, links = ?10, updated_at = ?11
         WHERE id = ?1",
        params![
            id,
            status,
            provenance,
            serde_json::to_string(&evidence)?,
            topic.join(","),
            upd.untested.unwrap_or(current.untested).trim(),
            cheapest_test,
            upd.revisit_when.unwrap_or(current.revisit_when).trim(),
            revisit_after,
            upd.links.unwrap_or(current.links).trim(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    let wall = get(store, id)?.ok_or_else(|| anyhow!("wall #{id} vanished after update"))?;
    Ok((wall, notes))
}

/// Link a settled challenge to the wall it was about, both ways.
pub fn link_challenge(store: &Store, wall_id: i64, challenge_id: i64) -> Result<()> {
    store.conn().execute(
        "UPDATE walls SET challenge_id = COALESCE(challenge_id, ?2) WHERE id = ?1",
        params![wall_id, challenge_id],
    )?;
    store.conn().execute(
        "UPDATE challenges SET wall_id = ?1 WHERE id = ?2",
        params![wall_id, challenge_id],
    )?;
    Ok(())
}

/// Walls whose `revisit_after` has come, oldest first.
pub fn due_for_revisit(store: &Store, today: &str) -> Result<Vec<Wall>> {
    let mut st = store.conn().prepare(&format!(
        "SELECT {COLUMNS} FROM walls
         WHERE revisit_after IS NOT NULL AND revisit_after <= ?1 AND status != 'retired'
         ORDER BY revisit_after, id"
    ))?;
    let rows = st.query_map(params![today], from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (moved, held, open) across the ledger.
pub fn base_rate(store: &Store) -> Result<(i64, i64, i64)> {
    Ok(store.conn().query_row(
        "SELECT COALESCE(SUM(status = 'moved'), 0), COALESCE(SUM(status = 'holds'), 0),
                COALESCE(SUM(status = 'open'), 0)
         FROM walls",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// This project's own record, in one line: the calibration an agent needs
/// before accepting a limit, drawn from local evidence rather than exhortation.
/// Empty until at least one limit has been tested.
pub fn base_rate_line(store: &Store) -> String {
    let Ok((moved, held, open)) = base_rate(store) else { return String::new() };
    if moved + held == 0 {
        return String::new();
    }
    format!(
        "Here, {moved} of {} tested limits moved when checked ({held} held); {open} open.",
        moved + held
    )
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn kinds_of(evidence: &[Evidence]) -> String {
    let mut kinds: Vec<&str> = evidence.iter().map(|e| e.kind.as_str()).collect();
    kinds.sort();
    kinds.dedup();
    kinds.join(" and ")
}

/// The newest fact, by date, as `kind date: text`.
fn latest_fact(w: &Wall) -> Option<String> {
    w.evidence
        .iter()
        .filter(|e| is_fact(e))
        .max_by(|a, b| a.date.cmp(&b.date))
        .map(|e| format!("{} {}: {}", e.kind, e.date, clip(&e.text, 110)))
}

/// One line per wall: status, provenance, the claim, and its open edge. This is
/// the form retrieval serves, so a documented limit arrives as a claim with a
/// price rather than a stop sign.
pub fn render_line(w: &Wall) -> String {
    let mut s = format!("#{} LIMIT [{} · {}] {}", w.id, w.status, w.provenance, clip(&w.claim, 140));
    let mut edge: Vec<String> = Vec::new();
    match w.status.as_str() {
        "holds" | "moved" => {
            if let Some(f) = latest_fact(w) {
                edge.push(format!("{}: {f}", if w.status == "moved" { "moved" } else { "held" }));
            }
        }
        _ => {}
    }
    if w.status != "moved" && w.status != "retired" {
        if !w.untested.is_empty() {
            edge.push(format!("untested: {}", clip(&w.untested, 120)));
        }
        if !w.cheapest_test.is_empty() {
            edge.push(format!("cheapest test: {}", clip(&w.cheapest_test, 120)));
        }
    }
    if !edge.is_empty() {
        s.push_str(" — ");
        s.push_str(&edge.join("; "));
    }
    if let Some(d) = w.revisit_after.as_deref() {
        if d <= today().as_str() && w.status != "retired" {
            s.push_str(&format!(" [revisit due since {d}]"));
        }
    }
    s
}

/// Everything about one wall.
pub fn render_full(w: &Wall) -> String {
    let movable = if is_movable(&w.provenance) {
        " — movable: a work item with a cost, not a wall"
    } else {
        ""
    };
    let mut s = format!(
        "wall #{} [{}]\n  claim:      {}\n  provenance: {} ({}){movable}\n",
        w.id,
        w.status,
        w.claim,
        w.provenance,
        provenance_label(&w.provenance)
    );
    if !w.topic.is_empty() {
        s.push_str(&format!("  topic:      {}\n", w.topic.join(", ")));
    }
    s.push_str("  evidence:\n");
    for e in &w.evidence {
        let src = if e.source.is_empty() { String::new() } else { format!(" [{}]", e.source) };
        let fact = if is_fact(e) { "" } else { " (not a fact)" };
        s.push_str(&format!("    - {} {}: {}{src}{fact}\n", e.kind, e.date, e.text));
    }
    if !w.untested.is_empty() {
        s.push_str(&format!("  untested:   {}\n", w.untested));
    }
    if !w.cheapest_test.is_empty() {
        s.push_str(&format!("  cheapest test: {}\n", w.cheapest_test));
    }
    if !w.revisit_when.is_empty() || w.revisit_after.is_some() {
        s.push_str(&format!(
            "  revisit:    {}{}\n",
            w.revisit_when,
            w.revisit_after.as_deref().map(|d| format!(" (after {d})")).unwrap_or_default()
        ));
    }
    if let Some(c) = w.challenge_id {
        s.push_str(&format!("  challenge:  #{c}\n"));
    }
    if !w.links.is_empty() {
        s.push_str(&format!("  links:      {}\n", w.links));
    }
    s.push_str(&format!("  recorded:   {}  updated: {}\n", w.created_at, w.updated_at));
    s
}

/// The text hint matching is scored against.
pub fn haystack(w: &Wall) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        w.claim,
        w.topic.join(" "),
        w.provenance,
        provenance_label(&w.provenance),
        w.untested,
        w.cheapest_test,
        w.revisit_when
    )
    .to_lowercase()
}

// ── Cheap tests ───────────────────────────────────────────────────────────────

/// A test this short is worth running before arguing about the limit.
pub const CHEAP_TEST_MINUTES: u32 = 30;

/// Minutes a `cheapest_test` says it takes ("~20 min", "(~1 h)", "half a
/// day"), or `None` when it names no duration.
pub fn estimate_minutes(text: &str) -> Option<u32> {
    let t = text.to_lowercase();
    for (phrase, minutes) in [("half a day", 240), ("half an hour", 30), ("an hour", 60), ("a day", 480)] {
        if t.contains(phrase) {
            return Some(minutes);
        }
    }
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        // A version string ("30.0.1") does not parse, and is not a duration.
        let Ok(n) = chars[start..i].iter().collect::<String>().parse::<f32>() else { continue };
        let mut j = i;
        while j < chars.len() && (chars[j] == ' ' || chars[j] == '-') {
            j += 1;
        }
        let unit: String = chars[j..].iter().take_while(|c| c.is_alphabetic()).collect();
        let minutes = match unit.as_str() {
            "m" | "min" | "mins" | "minute" | "minutes" => n,
            "h" | "hr" | "hrs" | "hour" | "hours" => n * 60.0,
            "day" | "days" => n * 480.0,
            _ => continue,
        };
        return Some(minutes.round() as u32);
    }
    None
}

/// An open wall whose cheapest test is short enough to just run.
pub fn is_cheap_shot(w: &Wall) -> bool {
    w.status == "open"
        && estimate_minutes(&w.cheapest_test).is_some_and(|m| m <= CHEAP_TEST_MINUTES)
}

// ── Dependencies ──────────────────────────────────────────────────────────────

/// Keys of a Cargo.toml that are not package names.
const CARGO_KEYS: &[&str] = &[
    "version", "edition", "name", "path", "features", "default-features", "optional",
    "package", "git", "branch", "rev", "tag", "workspace", "authors", "description",
    "license", "repository", "readme", "homepage", "documentation", "keywords",
    "categories", "resolver", "members", "exclude", "include", "build", "publish",
    "rust-version", "links", "default-run", "crate-type", "required-features", "harness",
    "test", "bench", "doc", "proc-macro", "opt-level", "debug", "lto", "codegen-units",
    "panic", "incremental", "overflow-checks", "strip", "inherits", "registry",
];

fn is_package_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@' | '/' | '.'))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '@')
}

/// Packages whose version an edit to a manifest touches: Cargo.toml,
/// Cargo.lock and package.json. Any other file yields nothing.
pub fn changed_dependencies(file_path: &str, added: &str) -> Vec<String> {
    let file = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path).to_lowercase();
    let mut names: Vec<String> = Vec::new();
    for line in added.lines().map(str::trim) {
        let name = match file.as_str() {
            "cargo.toml" => {
                if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    // [dependencies.wgpu], [target.'cfg(..)'.dependencies.wgpu]
                    section
                        .contains("dependencies.")
                        .then(|| section.rsplit('.').next().unwrap_or("").to_string())
                } else {
                    line.split_once('=').and_then(|(k, v)| {
                        let (k, v) = (k.trim().trim_matches('"'), v.trim());
                        let dep = (v.starts_with('"') || v.starts_with('{'))
                            && !CARGO_KEYS.contains(&k)
                            && !k.contains('.');
                        dep.then(|| k.to_string())
                    })
                }
            }
            "cargo.lock" => line
                .strip_prefix("name = \"")
                .map(|n| n.trim_end_matches('"').to_string()),
            "package.json" => line.split_once(':').and_then(|(k, v)| {
                let (k, v) = (k.trim().trim_matches('"'), v.trim().trim_matches(['"', ',']));
                let versioned = v.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '^' | '~'));
                (versioned && !matches!(k, "version" | "node" | "npm")).then(|| k.to_string())
            }),
            _ => None,
        };
        if let Some(n) = name.filter(|n| is_package_name(n)) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// Packages cargo reports changing in a command's output, e.g.
/// `Updating wgpu v30.0.1 -> v31.0.0`, `Adding`, `Downgrading`, `Removing`.
pub fn cargo_reported_changes(output: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() >= 3
            && matches!(t[0], "Updating" | "Upgrading" | "Downgrading" | "Adding" | "Removing")
            && t[2].strip_prefix('v').is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            && is_package_name(t[1])
            && !names.iter().any(|n| n == t[1])
        {
            names.push(t[1].to_string());
        }
    }
    names
}

/// Live walls bound to one of these packages: named in `revisit_when`, or a
/// library-version / library-default wall naming it in its claim or topic.
/// Returns (package, wall).
pub fn bound_to_packages(store: &Store, names: &[String]) -> Result<Vec<(String, Wall)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mentions = |text: &str, name: &str| {
        text.to_lowercase()
            .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '@' | '/')))
            .any(|w| w == name)
    };
    let mut out = Vec::new();
    for w in all(store)?.into_iter().filter(|w| matches!(w.status.as_str(), "open" | "holds")) {
        let version_bound = matches!(w.provenance.as_str(), "library-version" | "library-default");
        let hit = names.iter().find(|n| {
            let n = n.to_lowercase();
            mentions(&w.revisit_when, &n)
                || (version_bound && (mentions(&w.claim, &n) || w.topic.iter().any(|t| *t == n)))
        });
        if let Some(n) = hit {
            out.push((n.clone(), w));
        }
    }
    Ok(out)
}

// ── Marker evidence ───────────────────────────────────────────────────────────

/// Parse one evidence line from a `[CORTEX-WALL]` marker:
/// `kind: text @ source @ date`, `kind: text @ date`, or `kind: text (date)`.
pub fn parse_evidence_line(line: &str) -> Option<Evidence> {
    let line = line.trim().trim_start_matches(['-', '*']).trim();
    let (kind, rest) = line.split_once(':')?;
    let kind = evidence_kind(kind)?.to_string();
    let parts: Vec<&str> = rest.split(" @ ").map(str::trim).collect();
    let mut text = parts[0].to_string();
    let (mut source, mut date) = (String::new(), String::new());
    match parts.len() {
        1 => {}
        2 if valid_date(parts[1]) => date = parts[1].to_string(),
        2 => source = parts[1].to_string(),
        _ => {
            source = parts[1].to_string();
            date = parts[2].to_string();
        }
    }
    if date.is_empty() {
        if let Some(open) = text.rfind('(') {
            let inner = text[open + 1..].trim_end_matches(')').trim();
            if text.trim_end().ends_with(')') && valid_date(inner) {
                date = inner.to_string();
                text = text[..open].trim().to_string();
            }
        }
    }
    Some(Evidence { kind, text, source, date })
}

/// How a WALL marker is logged in `knowledge_markers`.
pub fn marker_body(w: &NewWall) -> String {
    let evidence: Vec<String> = w
        .evidence
        .iter()
        .map(|e| format!("{}: {} @ {} @ {}", e.kind, e.text, e.source, e.date))
        .collect();
    format!(
        "claim: {}\nprovenance: {}\nstatus: {}\nevidence: {}\nuntested: {}\ncheapest_test: {}",
        w.claim,
        w.provenance,
        w.status.as_deref().unwrap_or("open"),
        evidence.join("; "),
        w.untested,
        w.cheapest_test
    )
}

// ── Import ────────────────────────────────────────────────────────────────────

/// Import walls from a JSON array of `NewWall`s, skipping claims already on
/// record. Returns (recorded, skipped, errors).
pub fn import(store: &Store, json: &str) -> Result<(usize, usize, Vec<String>)> {
    let items: Vec<NewWall> = serde_json::from_str(json)?;
    let (mut recorded, mut skipped, mut errors) = (0, 0, Vec::new());
    for item in items {
        let claim = item.claim.clone();
        if find_by_claim(store, &claim)?.is_some() {
            skipped += 1;
            continue;
        }
        match record(store, item) {
            Ok(_) => recorded += 1,
            Err(e) => errors.push(format!("{}: {e}", clip(&claim, 60))),
        }
    }
    Ok((recorded, skipped, errors))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> crate::test_support::TempStore {
        crate::test_support::TempStore::new("walls").unwrap()
    }

    fn ev(kind: &str, text: &str, date: &str) -> Evidence {
        Evidence { kind: kind.into(), text: text.into(), source: String::new(), date: date.into() }
    }

    fn ray_query() -> NewWall {
        NewWall {
            claim: "Hardware ray queries are unavailable on Quest 3".into(),
            provenance: "hardware".into(),
            evidence: vec![ev("inferred", "Adreno 740 was assumed to lack RT units", "")],
            topic: vec!["quest, raytracing".into()],
            untested: "whether Quest's driver exposes VK_KHR_ray_query".into(),
            cheapest_test: "log VK_KHR_ray_query at startup (~15 min)".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_wall_needs_a_provenance_and_evidence() {
        let s = store();
        let mut w = ray_query();
        w.provenance = "it just is".into();
        let err = record(&s, w).unwrap_err().to_string();
        assert!(err.contains("whose limit"), "{err}");

        let mut w = ray_query();
        w.evidence.clear();
        assert!(record(&s, w).unwrap_err().to_string().contains("evidence"));

        let mut w = ray_query();
        w.evidence = vec![ev("gut feeling", "it seems unlikely to work", "")];
        assert!(record(&s, w).unwrap_err().to_string().contains("not one of"));
    }

    #[test]
    fn an_open_wall_must_name_its_cheapest_test() {
        let s = store();
        let mut w = ray_query();
        w.cheapest_test.clear();
        assert!(record(&s, w).unwrap_err().to_string().contains("cheapest_test"));
    }

    #[test]
    fn a_verdict_without_a_fact_is_stored_open() {
        let s = store();
        let mut w = ray_query();
        w.status = Some("holds".into());
        let (wall, notes) = record(&s, w).unwrap();
        assert_eq!(wall.status, "open");
        assert!(notes[0].contains("stored as `open`"), "{notes:?}");
        assert_eq!(wall.topic, vec!["quest", "raytracing"]);
    }

    #[test]
    fn a_source_must_be_dated_and_a_measurement_is_dated_today() {
        let s = store();
        let mut w = ray_query();
        w.evidence = vec![ev("paper", "Mesa Turnip exposes accelerated ray queries on a740+", "")];
        assert!(record(&s, w).unwrap_err().to_string().contains("date the paper"));

        let mut w = ray_query();
        w.evidence = vec![ev("measured", "VK_KHR_ray_query absent from the device's list", "")];
        w.status = Some("holds".into());
        let (wall, _) = record(&s, w).unwrap();
        assert_eq!(wall.status, "holds");
        assert_eq!(wall.evidence[0].date, today());
    }

    #[test]
    fn a_verdict_changes_only_with_a_new_fact_in_either_direction() {
        let s = store();
        let (wall, _) = record(&s, ray_query()).unwrap();
        let reconsidered = WallUpdate {
            status: Some("moved".into()),
            evidence: vec![ev("inferred", "on reflection the driver probably exposes it", "")],
            ..Default::default()
        };
        let err = update(&s, wall.id, reconsidered).unwrap_err().to_string();
        assert!(err.contains("needs a new fact"), "{err}");

        let measured = WallUpdate {
            status: Some("moved".into()),
            evidence: vec![ev("measured", "VK_KHR_ray_query listed by the Quest 3 driver", "2026-09-30")],
            ..Default::default()
        };
        let (moved, notes) = update(&s, wall.id, measured).unwrap();
        assert_eq!(moved.status, "moved");
        assert_eq!(moved.evidence.len(), 2, "evidence is appended, never replaced");
        assert!(!notes.is_empty());

        // Back the other way needs a fact too.
        let back = WallUpdate { status: Some("holds".into()), ..Default::default() };
        assert!(update(&s, wall.id, back).unwrap_err().to_string().contains("needs a new fact"));
    }

    #[test]
    fn the_line_shows_the_open_edge_and_the_base_rate_counts_tested_limits() {
        let s = store();
        let (open, _) = record(&s, ray_query()).unwrap();
        let line = render_line(&open);
        assert!(line.starts_with(&format!("#{} LIMIT [open · hardware]", open.id)), "{line}");
        assert!(line.contains("untested: whether Quest's driver"), "{line}");
        assert!(line.contains("cheapest test: log VK_KHR_ray_query"), "{line}");
        assert_eq!(base_rate_line(&s), "", "nothing tested yet, so no rate to state");

        record(
            &s,
            NewWall {
                claim: "Probe texture arrays are capped at 256 layers by the hardware".into(),
                provenance: "library-default".into(),
                status: Some("moved".into()),
                evidence: vec![ev("measured", "Adreno 740 reports maxImageArrayLayers = 2048", "2026-09-24")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(base_rate_line(&s), "Here, 1 of 1 tested limits moved when checked (0 held); 1 open.");
    }

    #[test]
    fn duplicate_claims_point_at_the_existing_wall() {
        let s = store();
        let (w, _) = record(&s, ray_query()).unwrap();
        let mut again = ray_query();
        again.claim = again.claim.to_uppercase();
        let err = record(&s, again).unwrap_err().to_string();
        assert!(err.contains(&format!("wall #{}", w.id)), "{err}");
    }

    #[test]
    fn marker_evidence_lines_parse_in_every_documented_form() {
        let e = parse_evidence_line("- measured: FastRPC refused three ways @ quest_app logcat @ 2026-09-17").unwrap();
        assert_eq!((e.kind.as_str(), e.source.as_str(), e.date.as_str()), ("measured", "quest_app logcat", "2026-09-17"));
        let e = parse_evidence_line("paper: Mesa Turnip ray query on a740+ (2025)").unwrap();
        assert_eq!((e.kind.as_str(), e.text.as_str(), e.date.as_str()), ("paper", "Mesa Turnip ray query on a740+", "2025"));
        let e = parse_evidence_line("vendor-doc: glTF 2.0 needs a channel @ 2021").unwrap();
        assert_eq!(e.date, "2021");
        assert!(parse_evidence_line("not evidence at all").is_none());
    }

    #[test]
    fn a_test_duration_is_read_from_its_description() {
        assert_eq!(estimate_minutes("one deploy (~20 min)"), Some(20));
        assert_eq!(estimate_minutes("bench one view (~1 h)"), Some(60));
        assert_eq!(estimate_minutes("one bench.py A/B (~2 h)"), Some(120));
        assert_eq!(estimate_minutes("prototype it (~half a day)"), Some(240));
        assert_eq!(estimate_minutes("move to wgpu 30.0.1 and rebuild, 15 minutes"), Some(15));
        assert_eq!(estimate_minutes("try it with two hands"), None);
        assert_eq!(estimate_minutes("query the device"), None);
        let mut w = Wall { status: "open".into(), cheapest_test: "log it at startup (~20 min)".into(), ..Default::default() };
        assert!(is_cheap_shot(&w));
        w.cheapest_test = "one bench.py A/B (~2 h)".into();
        assert!(!is_cheap_shot(&w));
    }

    #[test]
    fn manifest_edits_name_the_packages_they_change() {
        let toml = "wgpu = \"31.0.0\"\nopenxr = { version = \"0.19\", features = [\"loaded\"] }\nversion = \"0.2.0\"\nedition = \"2021\"";
        assert_eq!(changed_dependencies("vr_workspace/space_soup/Cargo.toml", toml), vec!["wgpu", "openxr"]);
        assert_eq!(changed_dependencies("Cargo.toml", "[dependencies.naga]\nversion = \"30\""), vec!["naga"]);
        assert_eq!(changed_dependencies("Cargo.lock", "[[package]]\nname = \"wgpu-hal\"\nversion = \"31.0.0\""), vec!["wgpu-hal"]);
        assert_eq!(changed_dependencies("web/package.json", "\"@babylonjs/core\": \"^8.1.0\",\n\"version\": \"1.0.0\""), vec!["@babylonjs/core"]);
        assert!(changed_dependencies("src/main.rs", "wgpu = \"31\"").is_empty(), "only manifests count");
    }

    #[test]
    fn cargo_output_names_what_it_updated() {
        let out = "    Updating crates.io index\n     Locking 2 packages to latest compatible versions\n    Updating wgpu v30.0.1 -> v31.0.0\n      Adding naga v31.0.0\n";
        assert_eq!(cargo_reported_changes(out), vec!["wgpu", "naga"]);
        assert!(cargo_reported_changes("error[E0308]: mismatched types").is_empty());
    }

    #[test]
    fn a_version_bound_wall_is_found_by_the_package_that_changed() {
        let s = store();
        let (bound, _) = record(
            &s,
            NewWall {
                claim: "wgpu cannot multisample a layered texture".into(),
                provenance: "library-version".into(),
                status: Some("holds".into()),
                evidence: vec![ev("measured", "wgpu 25 rejects array layers with MSAA", "2026-09-18")],
                ..Default::default()
            },
        )
        .unwrap();
        let (_hw, _) = record(&s, ray_query()).unwrap();
        let (named, _) = record(
            &s,
            NewWall {
                claim: "The OpenXR loader lacks recommended layer resolution".into(),
                provenance: "platform".into(),
                evidence: vec![ev("inferred", "not in openxr-sys 0.10 bindings", "")],
                cheapest_test: "grep the new bindings (~5 min)".into(),
                revisit_when: "an openxr crate release".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let hits = bound_to_packages(&s, &["wgpu".into(), "openxr".into()]).unwrap();
        let ids: Vec<i64> = hits.iter().map(|(_, w)| w.id).collect();
        assert_eq!(ids, vec![bound.id, named.id], "the hardware wall mentions neither");
        assert!(bound_to_packages(&s, &["serde".into()]).unwrap().is_empty());
    }

    #[test]
    fn due_revisits_are_listed_and_import_skips_known_claims() {
        let s = store();
        let mut w = ray_query();
        w.revisit_after = Some("2026-01-01".into());
        record(&s, w).unwrap();
        assert_eq!(due_for_revisit(&s, "2026-09-30").unwrap().len(), 1);
        assert_eq!(due_for_revisit(&s, "2025-12-31").unwrap().len(), 0);

        let json = r#"[{"claim":"Hardware ray queries are unavailable on Quest 3","provenance":"hardware",
                        "evidence":[{"kind":"inferred","text":"assumed from July's answer"}],
                        "cheapest_test":"query the extension"},
                       {"claim":"Multiview with MSAA needs wgpu 28 or later","provenance":"library-version",
                        "status":"moved","evidence":[{"kind":"measured","text":"builds and runs on wgpu 30.0.1","date":"2026-09-18"}]}]"#;
        let (recorded, skipped, errors) = import(&s, json).unwrap();
        assert_eq!((recorded, skipped), (1, 1), "{errors:?}");
    }
}
