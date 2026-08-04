/// File system watcher — observes changes and queues them for Syn's review.
/// Never auto-crystallizes. Never writes to memory without explicit approval.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::crystallizer::record_observation;
use crate::memory::Store;

pub fn watch(source_dir: &Path, db_path: &Path) -> Result<()> {
    let db_path = db_path.to_path_buf();
    let source_dir = source_dir.to_path_buf();

    eprintln!("cortex watch: observing {}", source_dir.display());
    eprintln!("  Changes queued for review — run `cortex review` to inspect.");
    eprintln!("  Press Ctrl+C to stop.\n");

    let recent: Arc<Mutex<Vec<(PathBuf, std::time::Instant)>>> = Arc::new(Mutex::new(vec![]));
    let recent_clone = recent.clone();

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(tx, Config::default().with_poll_interval(Duration::from_secs(1)))?;
    watcher.watch(&source_dir, RecursiveMode::Recursive)?;

    for event in rx {
        match event {
            Ok(ev) => {
                if !matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    continue;
                }

                for path in &ev.paths {
                    if path.extension().map_or(true, |e| e != "rs") {
                        continue;
                    }

                    // Debounce: skip if same file was seen within last 2 seconds
                    let now = std::time::Instant::now();
                    let mut seen = recent_clone.lock().unwrap();
                    seen.retain(|(_, t)| t.elapsed() < Duration::from_secs(2));

                    if seen.iter().any(|(p, _)| p == path) {
                        continue;
                    }
                    seen.push((path.clone(), now));
                    drop(seen);

                    let rel = path.strip_prefix(&source_dir).unwrap_or(path);
                    let summary = format!("Modified: {}", rel.display());
                    let diff_hint = read_changed_lines(path).unwrap_or_default();

                    eprintln!("  [change] {}", rel.display());

                    let store = match Store::open(&db_path) {
                        Ok(s) => s,
                        Err(e) => { eprintln!("  warn: could not open store: {e}"); continue; }
                    };

                    let _ = record_observation(&store, &rel.to_string_lossy(), &summary, &diff_hint);
                }
            }
            Err(e) => eprintln!("  watch error: {e}"),
        }
    }

    Ok(())
}

/// Read the last N non-empty lines of a file as a diff hint.
/// No actual diffing — just gives context for manual review.
fn read_changed_lines(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail: Vec<&str> = lines.iter().rev().take(20).rev().cloned().collect();
    Ok(tail.join("\n"))
}
