//! Did each mechanism actually fire?
//!
//! This project's recurring failure is not a broken feature — it is a feature
//! that was never running and looked fine. A `confidence` field that existed
//! only in a doc comment. A response cache with zero entries. A skill rejection
//! that did not stick. A column whose absence silently dropped every write. A
//! source fingerprint that was inert until its sampling site moved inside the
//! request loop. Each of those was shipped, plausible, and doing nothing.
//!
//! None of them could be caught by a test, because each was individually
//! correct. What was missing is the question "has this ever actually run?" —
//! which is cheap, and which nothing was asking.
//!
//! This report cannot itself fail silently: its entire output is a list of
//! things that did not happen.

use anyhow::Result;

use crate::memory::Store;

pub struct Mechanism {
    pub label: &'static str,
    pub table: &'static str,
    pub ts_col: &'static str,
    /// Flag when nothing has fired within this many days. Chosen per mechanism:
    /// some are expected every session, some are meant to be rare.
    pub expect_days: f64,
    /// What it means when this one goes quiet.
    pub when_idle: &'static str,
}

/// Everything with a heartbeat worth watching, and how often to expect one.
pub const MECHANISMS: &[Mechanism] = &[
    Mechanism {
        label: "compact_output (token saving)",
        table: "compression_savings",
        ts_col: "saved_at",
        expect_days: 2.0,
        when_idle: "the Bash hook is not installed or not reaching the server",
    },
    Mechanism {
        label: "test_signal (outcome scoring)",
        table: "test_outcomes",
        ts_col: "observed_at",
        expect_days: 2.0,
        when_idle: "builds are running but their verdicts are not being read",
    },
    Mechanism {
        label: "edit_guard (push retrieval)",
        table: "edit_guard_fires",
        ts_col: "fired_at",
        // Designed to be rare — silence for a day is correct, a month is not.
        expect_days: 30.0,
        when_idle: "no edit has matched a trap; check the hook is installed",
    },
    // The HOOK, not its output. `challenges` stays empty whenever nobody
    // disagrees, which is most of the time and is correct — so watching that
    // table cannot distinguish a working mechanism from an uninstalled one.
    Mechanism {
        label: "note_challenge hook (is it running)",
        table: "hook_heartbeat",
        ts_col: "last_fired",
        expect_days: 2.0,
        when_idle: "the UserPromptSubmit hook is not installed or not reaching the server",
    },
    Mechanism {
        label: "user corrections captured",
        table: "challenges",
        ts_col: "raised_at",
        // Disagreements are genuinely rare. This going quiet is a fact about the
        // conversations, not a fault — which is exactly why the hook above is
        // watched separately.
        expect_days: 60.0,
        when_idle: "no claim has been disputed; not a fault if the hook above is live",
    },
    Mechanism {
        label: "retrieval telemetry",
        table: "session_retrieval_log",
        ts_col: "retrieved_at",
        expect_days: 2.0,
        when_idle: "knowledge is not being consulted at all",
    },
    Mechanism {
        label: "knowledge markers committed",
        table: "knowledge_markers",
        ts_col: "extracted_at",
        expect_days: 14.0,
        when_idle: "sessions are ending without capturing what they learned",
    },
    Mechanism {
        label: "closeout outcomes applied",
        table: "outcome_applied_log",
        ts_col: "applied_at",
        expect_days: 14.0,
        when_idle: "expected once test_signal scores sessions instead",
    },
    Mechanism {
        label: "skill candidates mined",
        table: "skill_candidates",
        ts_col: "last_seen_at",
        expect_days: 21.0,
        when_idle: "the consolidation pipeline is not clustering sessions",
    },
    Mechanism {
        label: "proposals raised",
        table: "proposals",
        ts_col: "created_at",
        expect_days: 30.0,
        when_idle: "the self-learning loop is producing nothing to review",
    },
    Mechanism {
        label: "query gaps recorded",
        table: "query_gap_log",
        ts_col: "last_seen_at",
        expect_days: 21.0,
        when_idle: "either coverage is perfect or gap logging is broken",
    },
];

#[derive(Debug, PartialEq)]
pub enum Status {
    /// Fired within its expected window.
    Live,
    /// Has fired, but not lately.
    Idle,
    /// Has never fired at all.
    NeverFired,
    /// The table or column is missing, so the question cannot be answered —
    /// which is itself a finding, not a reason to stay quiet.
    CannotCheck(String),
}

pub struct Reading {
    pub label: &'static str,
    pub rows: i64,
    pub age_days: Option<f64>,
    pub status: Status,
    pub when_idle: &'static str,
}

/// Read every mechanism's heartbeat.
pub fn read_all(store: &Store) -> Vec<Reading> {
    MECHANISMS.iter().map(|m| read_one(store, m)).collect()
}

fn read_one(store: &Store, m: &Mechanism) -> Reading {
    // Timestamps in this schema are a mix of unix epochs and RFC 3339 strings,
    // so ask SQLite to normalise rather than parsing both shapes here.
    // CAST is not optional: strftime returns TEXT, so without it the read of a
    // column storing RFC 3339 fails and the mechanism reports "cannot check".
    // Which it did, on this module's first run, for three of eleven rows —
    // correctly refusing to claim health it could not establish.
    let sql = format!(
        "SELECT COUNT(*),
                MAX(CASE WHEN typeof({c}) IN ('integer','real') THEN CAST({c} AS INTEGER)
                         ELSE CAST(strftime('%s', {c}) AS INTEGER) END)
         FROM {t}",
        c = m.ts_col,
        t = m.table
    );
    let row: rusqlite::Result<(i64, Option<i64>)> =
        store.conn().query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?)));

    match row {
        Err(e) => Reading {
            label: m.label,
            rows: 0,
            age_days: None,
            status: Status::CannotCheck(e.to_string()),
            when_idle: m.when_idle,
        },
        Ok((rows, last)) => {
            let now = chrono::Utc::now().timestamp();
            let age_days = last.map(|t| (now - t) as f64 / 86_400.0);
            let status = if rows == 0 || last.is_none() {
                Status::NeverFired
            } else if age_days.unwrap_or(f64::MAX) > m.expect_days {
                Status::Idle
            } else {
                Status::Live
            };
            Reading { label: m.label, rows, age_days, status, when_idle: m.when_idle }
        }
    }
}

/// Only the mechanisms worth mentioning: silence about what is working.
///
/// Returns an empty string when everything is live, so this can be appended to
/// a report every session without becoming noise.
pub fn render_problems(readings: &[Reading]) -> String {
    let bad: Vec<&Reading> =
        readings.iter().filter(|r| !matches!(r.status, Status::Live)).collect();
    if bad.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nMECHANISMS NOT FIRING\n");
    for r in bad {
        let when = match (&r.status, r.age_days) {
            (Status::NeverFired, _) => "never".to_string(),
            (Status::CannotCheck(e), _) => format!("cannot check: {e}"),
            (_, Some(d)) => format!("{d:.0}d ago"),
            _ => "unknown".to_string(),
        };
        out.push_str(&format!("  {:34} {:>16}   {}\n", r.label, when, r.when_idle));
    }
    out
}

/// The full table, for the CLI.
pub fn render_full(readings: &[Reading]) -> String {
    let mut out = String::from("MECHANISM                          ROWS      LAST FIRED\n");
    for r in readings {
        let when = match (&r.status, r.age_days) {
            (Status::NeverFired, _) => "NEVER".to_string(),
            (Status::CannotCheck(_), _) => "cannot check".to_string(),
            (_, Some(d)) if d < 1.0 => format!("{:.1}h ago", d * 24.0),
            (_, Some(d)) => format!("{d:.0}d ago"),
            _ => "-".to_string(),
        };
        let flag = match r.status {
            Status::Live => " ",
            Status::Idle => "!",
            Status::NeverFired => "x",
            Status::CannotCheck(_) => "?",
        };
        out.push_str(&format!("{flag} {:32} {:<9} {}\n", r.label, r.rows, when));
    }
    out.push_str("\n  ! idle beyond its expected window   x never fired   ? cannot check\n");
    out
}

pub fn run_cli(store: &Store) -> Result<()> {
    let readings = read_all(store);
    print!("{}", render_full(&readings));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_report_says_nothing() {
        let readings = vec![Reading {
            label: "x",
            rows: 5,
            age_days: Some(0.1),
            status: Status::Live,
            when_idle: "",
        }];
        assert!(render_problems(&readings).is_empty(), "silence about what works");
    }

    #[test]
    fn a_mechanism_that_never_fired_is_named() {
        let readings = vec![Reading {
            label: "test_signal",
            rows: 0,
            age_days: None,
            status: Status::NeverFired,
            when_idle: "verdicts are not being read",
        }];
        let out = render_problems(&readings);
        assert!(out.contains("test_signal"), "{out}");
        assert!(out.contains("never"), "{out}");
        assert!(out.contains("verdicts are not being read"), "must say what it means: {out}");
    }

    #[test]
    fn a_missing_table_is_reported_not_swallowed() {
        // The failure this whole module exists to prevent: a check that cannot
        // run must say so, not return a clean bill of health.
        let readings = vec![Reading {
            label: "gone",
            rows: 0,
            age_days: None,
            status: Status::CannotCheck("no such table".into()),
            when_idle: "",
        }];
        assert!(render_problems(&readings).contains("cannot check"));
    }

    #[test]
    fn every_mechanism_declares_what_its_silence_means() {
        for m in MECHANISMS {
            assert!(!m.when_idle.is_empty(), "{} has no diagnosis", m.label);
            assert!(m.expect_days > 0.0, "{} has no expected cadence", m.label);
        }
    }
}
