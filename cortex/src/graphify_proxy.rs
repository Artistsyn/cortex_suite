//! `cortex graphify-serve`: graphify's MCP server, kept current.
//!
//! `graphify-rs serve` reads graph.json once, at startup, and never again -
//! verified by swapping a 3-node graph for a 6-node one under a running server,
//! which went on reporting 3. So a rebuild (closeout's, or anyone's) changed
//! nothing for any session already running, and graphify answered architecture
//! questions from whatever the tree looked like when the session began.
//!
//! This sits between the host and graphify-rs as a line-for-line stdio proxy.
//! Before forwarding a tool call it asks whether the source is newer than the
//! graph; if so it rebuilds incrementally (JSON only, `--update`: ~1.7 s on
//! FlowMake) and restarts the child, replaying the host's handshake, so the
//! answer comes from the current tree. It also restarts the child when another
//! process rebuilt the graph. Nothing runs between calls: no watcher, no timer,
//! no idle CPU.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde_json::Value;

/// A lock younger than this is someone else's rebuild in progress.
const LOCK_FRESH: Duration = Duration::from_secs(120);
/// How long to wait for another process's rebuild before answering anyway.
const LOCK_WAIT: Duration = Duration::from_secs(30);

struct Graphify {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// graph.json's mtime when this child loaded it.
    loaded: Option<SystemTime>,
}

fn graph_mtime(graph: &Path) -> Option<SystemTime> {
    std::fs::metadata(graph).and_then(|m| m.modified()).ok()
}

impl Graphify {
    fn spawn(bin: &str, graph: &Path, repo: &Path) -> Result<Self> {
        let loaded = graph_mtime(graph);
        let mut child = Command::new(bin)
            .args(["serve", "--graph"])
            .arg(graph)
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("could not start `{bin} serve` - is graphify-rs installed?"))?;
        let stdin = child.stdin.take().context("no child stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("no child stdout")?);
        Ok(Self { child, stdin, stdout, loaded })
    }

    fn send(&mut self, line: &str) -> Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Read lines until the response carrying `id`. Anything else the child
    /// emits (a notification) is passed through to `out` untouched.
    fn response_for(&mut self, id: &Value, out: &mut impl Write) -> Result<String> {
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                anyhow::bail!("graphify-rs exited");
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let matches = serde_json::from_str::<Value>(trimmed)
                .ok()
                .is_some_and(|v| v.get("id") == Some(id));
            if matches {
                return Ok(trimmed.to_string());
            }
            writeln!(out, "{trimmed}")?;
            out.flush()?;
        }
    }
}

impl Drop for Graphify {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Take the rebuild lock, or wait out someone else's. Returns true when WE hold
/// it and should rebuild; false when another process just did.
fn acquire_lock(lock: &Path) -> bool {
    let start = Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(lock) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return true;
            }
            Err(_) => {
                let age = std::fs::metadata(lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .unwrap_or(Duration::MAX);
                if age > LOCK_FRESH {
                    // Left behind by a process that died mid-rebuild.
                    let _ = std::fs::remove_file(lock);
                    continue;
                }
                if start.elapsed() > LOCK_WAIT {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(200));
                if !lock.exists() {
                    return false; // they finished; their graph is current
                }
            }
        }
    }
}

/// Bring graph.json up to date if the source moved past it. Returns a note to
/// attach to the answer when the graph could NOT be made current.
fn ensure_fresh(repo: &Path, graph: &Path) -> Option<String> {
    let reason = crate::closeout::graph_is_stale(repo, graph)?;
    let lock = graph.with_file_name(".rebuild.lock");
    if !acquire_lock(&lock) {
        return crate::closeout::graph_is_stale(repo, graph)
            .map(|r| format!("[graph snapshot may be stale: {r}; another rebuild is in progress]"));
    }
    let t = Instant::now();
    let result = crate::closeout::rebuild_graph(repo);
    let _ = std::fs::remove_file(&lock);
    match result {
        Ok(()) => {
            eprintln!("graphify-serve: rebuilt ({reason}) in {:.1}s", t.elapsed().as_secs_f64());
            None
        }
        Err(e) => Some(format!("[graph snapshot is stale ({reason}); rebuild failed: {e}]")),
    }
}

fn attach_note(response: &str, note: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(response) else { return response.to_string() };
    if let Some(text) = v["result"]["content"][0]["text"].as_str() {
        v["result"]["content"][0]["text"] = Value::String(format!("{text}\n\n{note}"));
    } else if let Some(msg) = v["error"]["message"].as_str() {
        v["error"]["message"] = Value::String(format!("{msg} {note}"));
    }
    serde_json::to_string(&v).unwrap_or_else(|_| response.to_string())
}

pub fn serve(repo: &Path, graph: &Path, bin: &str) -> Result<()> {
    let graph: PathBuf = if graph.is_absolute() { graph.to_path_buf() } else { repo.join(graph) };
    let mut child = Graphify::spawn(bin, &graph, repo)?;
    let mut init_request: Option<String> = None;
    let mut init_notice: Option<String> = None;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(trimmed).unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");
        let id = req.get("id").cloned();

        let mut note: Option<String> = None;
        if method == "initialize" {
            init_request = Some(trimmed.to_string());
        } else if method == "notifications/initialized" {
            init_notice = Some(trimmed.to_string());
        } else if method == "tools/call" {
            note = ensure_fresh(repo, &graph);
            // Restart when the file moved under the child - our rebuild or
            // another session's.
            if graph_mtime(&graph) != child.loaded {
                match Graphify::spawn(bin, &graph, repo) {
                    Ok(mut fresh) => {
                        let mut sink = std::io::sink();
                        if let Some(init) = &init_request {
                            let init_id = serde_json::from_str::<Value>(init)
                                .ok()
                                .and_then(|v| v.get("id").cloned())
                                .unwrap_or(Value::Null);
                            fresh.send(init)?;
                            fresh.response_for(&init_id, &mut sink)?;
                        }
                        if let Some(n) = &init_notice {
                            fresh.send(n)?;
                        }
                        eprintln!("graphify-serve: reloaded graph.json");
                        child = fresh;
                    }
                    Err(e) => {
                        note = Some(format!("[graph reload failed, answering from the previous snapshot: {e}]"));
                    }
                }
            }
        }

        child.send(trimmed)?;
        let Some(id) = id else { continue }; // a notification: no response
        let mut response = child.response_for(&id, &mut out)?;
        if let Some(n) = &note {
            response = attach_note(&response, n);
        }
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    Ok(())
}
