//! Fixtures shared by every test module in this crate.
//!
//! TWO TIERS, AND THEY ANSWER DIFFERENT QUESTIONS
//!
//! `TempStore` is for logic: hermetic, disposable, and identical on every
//! machine. Almost every test wants this.
//!
//! `live_store_readonly` is for the store people actually have. A hermetic
//! fixture contains what its author imagined, and the real one contains three
//! hundred entries with long bodies, unicode, embedded quotes and brackets --
//! which is exactly the material that produced the marker-escaping bugs. Losing
//! all contact with it means nobody notices when the real store develops a shape
//! the code cannot read. It is opened READ-ONLY, so a test run can never migrate
//! or mutate somebody's knowledge base as a side effect.
//!
//! WHY CLEANUP IS A DROP AND NOT A LINE AT THE END OF THE TEST
//!
//! Every module here already tried to clean up, with a `remove_file` on the last
//! line of each test. That works right up until a test fails -- an assertion
//! unwinds, the line never runs, and the database stays. Which is the wrong way
//! round: a failing run is when you are least likely to be tidying up by hand,
//! and it is also when the most runs happen. This machine had accumulated 304
//! leftover test databases totalling 110MB that way.
//!
//! `Drop` runs during unwinding, so the directory goes whether the test passed,
//! failed, or panicked in a helper.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use crate::memory::Store;

/// A store in a directory of its own, removed when the test ends.
///
/// Deref'd to `Store`, so it is a drop-in for a bare `Store` at the call sites.
pub struct TempStore {
    store: Store,
    dir: PathBuf,
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

impl TempStore {
    /// A fresh, EMPTY store.
    ///
    /// Empty is the part that takes doing. `Store::open` runs `first_run_init`
    /// when it creates the file, which seeds four anti-patterns, a pattern and
    /// fourteen annotations, writes a prefs.toml beside the database, and prints
    /// a banner -- so a test that counted rows would be silently off by the size
    /// of the seed, and every test would print the banner.
    ///
    /// Creating the file first makes it not-new, so only the schema migration
    /// runs. That is a quiet dependency on how `Store::open` decides newness, so
    /// the emptiness is ASSERTED below rather than assumed: if that ever changes,
    /// this fails loudly here instead of skewing counts somewhere else.
    pub fn new(tag: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "cortex_t_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;

        let db = dir.join("memory.db");
        // A zero-length file is a valid empty SQLite database, so this only
        // suppresses the first-run seeding -- the schema still gets built.
        std::fs::File::create(&db)?;

        let store = Store::open(&db)?;
        let seeded: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM anti_patterns", [], |r| r.get(0))?;
        assert_eq!(
            seeded, 0,
            "a TempStore came up seeded -- `Store::open` no longer treats an \
             existing empty file as not-new, so every row count in the suite is \
             now off by the size of the first-run seed",
        );

        Ok(Self { store, dir })
    }

    /// The directory, for the rare test that needs to put a file beside the db.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl std::ops::Deref for TempStore {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.store
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        // Best effort: a test that already failed must not be turned into a
        // second, more confusing failure by its own cleanup.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A directory that removes itself, for fixtures that are not a store.
///
/// Several test modules build a `.cortex/` tree, a skills directory or a graph
/// file rather than a database. They leaked for the same reason -- cleanup on
/// the last line of the test, skipped by any unwind.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "cortex_d_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A path inside it. The file need not exist yet.
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The real store, opened READ-ONLY, or `None` when there is not one.
///
/// Found by walking UP from this crate rather than by counting directories.
/// The previous helper hardcoded one level and the store is two above, so it
/// never resolved and every test guarded by it returned early -- passing for
/// months without executing. A search cannot be off by one.
///
/// Read-only is not a nicety. `Store::open` runs migrations, so a test suite
/// that opened the real store the ordinary way would alter somebody's knowledge
/// base as a side effect of `cargo test`.
pub fn live_store_readonly() -> Option<Connection> {
    let mut dir: Option<&Path> = Some(Path::new(env!("CARGO_MANIFEST_DIR")));
    while let Some(d) = dir {
        let db = d.join(".cortex").join("memory.db");
        if db.exists() {
            return Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_store_starts_empty_and_is_writable() {
        let s = TempStore::new("selftest").expect("a temp store should open");
        let n: i64 = s
            .conn()
            .query_row("SELECT count(*) FROM patterns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the schema is there but nothing is seeded");
        s.conn()
            .execute(
                "INSERT INTO patterns (name, intent, body, uses, tags, approved_at)
                 VALUES ('n','i','b','[]','[]','2026-01-01')",
                [],
            )
            .expect("and it is a real, writable database");
    }

    #[test]
    fn two_temp_stores_do_not_share_a_directory() {
        let a = TempStore::new("iso").unwrap();
        let b = TempStore::new("iso").unwrap();
        assert_ne!(a.dir(), b.dir(), "same tag must still get its own directory");
    }

    #[test]
    fn the_directory_is_gone_after_the_store_is_dropped() {
        let path = {
            let s = TempStore::new("cleanup").unwrap();
            s.dir().to_path_buf()
        };
        assert!(!path.exists(), "Drop did not remove {}", path.display());
    }

    #[test]
    fn cleanup_survives_a_panic() {
        // The whole reason this is a Drop. Manual cleanup on the last line of a
        // test is skipped by the unwind, which is why 304 databases had piled up.
        let path = std::sync::Mutex::new(PathBuf::new());
        let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let s = TempStore::new("panicky").unwrap();
            *path.lock().unwrap() = s.dir().to_path_buf();
            panic!("as a test would");
        }));
        assert!(taken.is_err(), "the panic should have propagated");
        let p = path.lock().unwrap().clone();
        assert!(!p.exists(), "a panicking test left {} behind", p.display());
    }
}

/// Invariants the real store must hold, checked read-only.
///
/// This is the audit that found the marker-escaping damage, kept as a test. Each
/// check below names a shape that actually occurred in this store, so a
/// regression in the marker parser is caught by the data rather than by somebody
/// noticing a description reads oddly six months later.
///
/// Skips cleanly when there is no store — a fresh checkout has none, and a test
/// that failed there would be noise rather than signal.
#[cfg(test)]
mod store_health {
    use super::*;

    /// Every offending row, as `id: excerpt`, for a failure message worth acting on.
    fn offenders(conn: &Connection, sql: &str) -> Vec<String> {
        let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
        let rows = stmt.query_map([], |r| {
            Ok(format!("{}: {}", r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        });
        rows.map(|r| r.filter_map(|x| x.ok()).take(8).collect()).unwrap_or_default()
    }

    macro_rules! store {
        () => {
            match live_store_readonly() {
                Some(c) => c,
                None => {
                    eprintln!("skipping: no store on this machine");
                    return;
                }
            }
        };
    }

    #[test]
    fn no_anti_pattern_has_a_placeholder_remedy() {
        // The `wrong:`/`correct:` split is done per line, so a body whose
        // newlines were escaped in transit lands wholly in `wrong` and the
        // remedy becomes a placeholder. Three entries were stranded that way:
        // the advice was present in the row and unreachable by anyone reading it.
        let c = store!();
        let bad = offenders(
            &c,
            "SELECT id, substr(description,1,60) FROM anti_patterns
             WHERE correct IN ('see body above', '') OR correct IS NULL",
        );
        assert!(bad.is_empty(), "anti-patterns with no usable remedy:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn no_entry_kept_its_escaping() {
        // A value whose quotes arrived escaped used to be stored as the first
        // whitespace-delimited token -- one description reached the store as the
        // two characters `\"A`, with its tags emptied.
        let c = store!();
        let mut bad = offenders(
            &c,
            r#"SELECT id, substr(description,1,60) FROM anti_patterns WHERE description LIKE '%\"%'"#,
        );
        bad.extend(offenders(
            &c,
            r#"SELECT id, name FROM patterns WHERE name LIKE '%\"%' OR intent LIKE '%\"%'"#,
        ));
        assert!(bad.is_empty(), "entries holding escaped quotes:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn no_remedy_is_buried_in_the_wrong_half() {
        // The precise damage signature, and deliberately narrower than "the
        // field mentions `\n`". An anti-pattern here documents a newline escape
        // becoming a real newline in a heredoc, so its `wrong` half says `\n` on
        // purpose and is perfectly intact -- a looser check flagged it, which
        // would have taught whoever ran this to ignore the test.
        //
        // What is actually broken is a remedy still sitting inside `wrong`
        // behind an unconverted escape, because the split is done per line.
        let c = store!();
        let mut bad = offenders(
            &c,
            r"SELECT id, substr(description,1,60) FROM anti_patterns
              WHERE wrong LIKE '%\ncorrect:%'",
        );
        bad.extend(offenders(
            &c,
            r"SELECT id, name FROM patterns
              WHERE body LIKE '%\n%' AND body NOT LIKE '%' || char(10) || '%'",
        ));
        assert!(bad.is_empty(), "bodies flattened onto one line:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn no_description_was_truncated_at_a_bracket() {
        // The header parser used to stop at the first `]`, so a value containing
        // one was cut there and every attribute after it was lost. An entry
        // about the regex `[a-z_]+` was stored as "... using [a-z_".
        let c = store!();
        let bad = offenders(
            &c,
            "SELECT id, description FROM anti_patterns
             WHERE description LIKE '%[%' AND description NOT LIKE '%]%'",
        );
        assert!(bad.is_empty(), "descriptions truncated at a bracket:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn every_entry_says_something() {
        let c = store!();
        let mut bad = offenders(
            &c,
            "SELECT id, coalesce(description,'(null)') FROM anti_patterns
             WHERE trim(coalesce(description,'')) = '' OR trim(coalesce(wrong,'')) = ''",
        );
        bad.extend(offenders(
            &c,
            "SELECT id, coalesce(name,'(null)') FROM patterns
             WHERE trim(coalesce(name,'')) = '' OR trim(coalesce(body,'')) = ''",
        ));
        assert!(bad.is_empty(), "entries with an empty required field:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn a_recurring_failure_signature_carries_an_identity() {
        // A signature keyed on the error code alone merged unrelated failures
        // into one phantom trap. New-scheme keys always carry a discriminator.
        let c = store!();
        let bad = offenders(
            &c,
            "SELECT rowid, signature FROM recurring_errors
             WHERE signature LIKE 'rust:%' AND signature NOT LIKE 'rust:%:%'",
        );
        assert!(bad.is_empty(), "failure signatures with no identity:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn the_store_is_actually_being_read() {
        // Guards the guard. Every test above passes vacuously against an empty
        // or unreadable database, and this file's whole predecessor passed for
        // months by never finding the store at all.
        let c = store!();
        let n: i64 = c
            .query_row("SELECT count(*) FROM anti_patterns", [], |r| r.get(0))
            .unwrap_or(0);
        assert!(n > 0, "opened a store with no anti-patterns in it — reading the wrong file?");
    }
}
