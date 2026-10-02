//! One stdio MCP connection, kept on the current build.
//!
//! Claude Code keeps a session's MCP servers for as long as the session runs,
//! which for long work is days. A rebuilt server binary used to reach none of
//! them: the running process kept the old code, and the client kept the tool
//! list it fetched at connect. From Claude Code 2.1.284's client code, and a
//! live session where noted:
//!
//! - A server that declares `tools.listChanged` may send
//!   `notifications/tools/list_changed`. The client then fetches the list again
//!   and adds what was added, changed or removed to the conversation as a tool
//!   delta, leaving the cached prompt as it was. Without the declaration it does
//!   not listen.
//! - The client listens for it only on a connection it subscribed: one opened
//!   at session start or reconnected with /mcp in a terminal, or one its MCP
//!   discovery cache adopted (when that is on).
//! - When a stdio server's process ends, the client starts it again on the
//!   session's next call to it and keeps the tool list from before. A
//!   connection whose first request is a tool call, with no `tools/list` before
//!   it, is therefore holding an old list, and is told to fetch it again. A
//!   client that did not subscribe that connection ignores this (observed
//!   2026-10-01: the session kept 13 tools while the server listed 14). Only a
//!   fresh connection refreshes it: /mcp reconnect in a terminal, or a new
//!   process. The desktop app offers neither for a local server.
//!
//! While waiting for a request the server compares its binary on disk with the
//! one it started from, every couple of seconds. Once a rebuild has replaced it,
//! the new build is started on the side with this process's arguments and must
//! answer `initialize` and `tools/list`. Then this process becomes the new build
//! on the same pipes (exec), the client's connection carries on, and the new
//! build announces its tools. Only Unix has exec; elsewhere the server says on
//! stderr that a rebuild is waiting and keeps serving.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Set for the process that takes over a running connection.
pub const RESUMED_ENV: &str = "CORTEX_SUITE_MCP_RESUMED";

/// Prefix of the variables that hand values on to the next build.
const CARRY_PREFIX: &str = "CORTEX_SUITE_MCP_CARRY_";

/// The notification that makes a client fetch the tool list again.
pub const LIST_CHANGED: &str = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

/// A rebuild is taken only once its file has been still this long, so a binary
/// still being written is never started.
const SETTLE: Duration = Duration::from_secs(2);

/// How often a server waiting for a request looks at its binary.
const IDLE_CHECK: Duration = Duration::from_secs(2);

/// How long a rebuilt binary has to answer `initialize` and `tools/list`.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

/// What identifies one build of the binary at a path.
#[derive(Debug, Clone, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        m.ino()
    };
    #[cfg(not(unix))]
    let inode = 0;
    Some(Stamp { len: m.len(), modified: m.modified().ok(), inode })
}

/// The executable this process started from, as it was then. The path is
/// taken at start: on Linux `current_exe` names a replaced file "(deleted)".
struct Binary {
    path: PathBuf,
    started: Stamp,
    /// The last build looked at, so a failed one is not tried on every wait.
    tried: Option<Stamp>,
}

impl Binary {
    fn current() -> Option<Binary> {
        let exe = std::env::current_exe().ok()?;
        let path = std::path::absolute(&exe).unwrap_or(exe);
        let started = stamp(&path)?;
        Some(Binary { path, started, tried: None })
    }

    /// A different build, settled and not yet tried, now sits at the path.
    fn replaced(&self) -> Option<Stamp> {
        let now = stamp(&self.path)?;
        if now == self.started || self.tried.as_ref() == Some(&now) {
            return None;
        }
        let age = now.modified.and_then(|m| SystemTime::now().duration_since(m).ok());
        age.is_some_and(|a| a >= SETTLE).then_some(now)
    }
}

/// Start the build at `path` with this process's arguments and hold the MCP
/// handshake with it: it must answer `initialize`, then list at least one tool.
/// Its stdin and stdout are pipes of its own, never the client's.
fn probe(path: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(path)
        .args(std::env::args_os().skip(1))
        .env_remove(RESUMED_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("does not start ({e})"))?;
    let (Some(mut input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("started without pipes".into());
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let handshake = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"cortex_suite-probe","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    );
    let verdict = (|| {
        input
            .write_all(handshake.as_bytes())
            .and_then(|_| input.flush())
            .map_err(|e| format!("closed its input ({e})"))?;
        let deadline = Instant::now() + PROBE_LIMIT;
        let mut initialized = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx.recv_timeout(left).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => format!(
                    "did not answer initialize and tools/list within {} s",
                    PROBE_LIMIT.as_secs()
                ),
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    "ended without answering initialize and tools/list".to_string()
                }
            })?;
            let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            match reply.get("id").and_then(|id| id.as_i64()) {
                Some(1) if reply.get("result").is_some() => initialized = true,
                Some(1) => return Err(format!("refused initialize: {}", reply["error"])),
                Some(2) => {
                    let tools = reply["result"]["tools"].as_array().map_or(0, |t| t.len());
                    return if initialized && tools > 0 {
                        Ok(())
                    } else {
                        Err(format!("listed {tools} tools"))
                    };
                }
                // A notification, which a server may send at any time.
                _ => {}
            }
        }
    })();

    // It ends at the end of its input; it is stopped if it does not, promptly.
    drop(input);
    let quit = Instant::now() + Duration::from_secs(5);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < quit {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    verdict
}

/// One client connection on stdin and stdout.
pub struct Session {
    reader: BufReader<io::StdinLock<'static>>,
    name: &'static str,
    binary: Option<Binary>,
    resumed: bool,
    /// The client asked for the tool list on this connection.
    listed: bool,
    /// A list_changed is owed, sent before the next read.
    owed: bool,
    /// One has been owed already on this connection.
    told: bool,
    carry: Vec<(String, String)>,
}

impl Session {
    /// The connection on this process's stdin. A process that took over a
    /// running connection owes the client a list_changed at once: its tools
    /// may differ from the build the client listed.
    pub fn open(name: &'static str) -> Session {
        let resumed = std::env::var_os(RESUMED_ENV).is_some();
        if resumed {
            eprintln!("{name}: continuing this session on the rebuilt binary");
        }
        Session {
            // Larger than stdin's own buffer, so every read goes straight to the
            // pipe and `buffer()` holds all that was read and not yet used.
            reader: BufReader::with_capacity(1 << 16, io::stdin().lock()),
            name,
            binary: Binary::current(),
            resumed,
            listed: false,
            owed: resumed,
            told: resumed,
            carry: Vec::new(),
        }
    }

    /// A value the process this one took over from handed on with `carry`.
    pub fn carried(&self, key: &str) -> Option<String> {
        self.resumed.then(|| std::env::var(format!("{CARRY_PREFIX}{key}")).ok()).flatten()
    }

    /// Hand `value` on to the build that takes this connection over, if one
    /// does: state a client would otherwise see reset mid-session.
    pub fn carry(&mut self, key: &str, value: String) {
        self.carry.retain(|(k, _)| k != key);
        self.carry.push((key.to_string(), value));
    }

    /// Note a request's method. A tool call with no tools/list before it on
    /// this connection means the client kept a list from an earlier process.
    pub fn saw(&mut self, method: &str) {
        observe(method, &mut self.listed, &mut self.owed, &mut self.told);
    }

    /// The next request line, or None at the end of input. Before reading it
    /// sends an owed list_changed. While nothing has been read ahead, which is
    /// while waiting for the client, a rebuilt binary takes the connection over.
    pub fn next_line(&mut self, out: &mut impl Write) -> io::Result<Option<String>> {
        if self.owed {
            writeln!(out, "{LIST_CHANGED}")?;
            out.flush()?;
            self.owed = false;
        }
        while self.reader.buffer().is_empty() {
            self.hand_over_if_rebuilt(out)?;
            if input_ready(IDLE_CHECK) {
                break;
            }
        }
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn hand_over_if_rebuilt(&mut self, out: &mut impl Write) -> io::Result<()> {
        let Some(bin) = self.binary.as_mut() else { return Ok(()) };
        let Some(now) = bin.replaced() else { return Ok(()) };
        bin.tried = Some(now);
        if !cfg!(unix) {
            eprintln!(
                "{}: the binary was rebuilt; reconnect this server from the host to use it",
                self.name
            );
            return Ok(());
        }
        if let Err(why) = probe(&bin.path) {
            eprintln!("{}: the rebuilt binary {why}; this build keeps serving", self.name);
            return Ok(());
        }
        out.flush()?;
        let err = hand_over(&bin.path, self.name, &self.carry);
        eprintln!("{}: could not start the rebuilt binary ({err}); this build keeps serving", self.name);
        Ok(())
    }
}

/// The bookkeeping behind `Session::saw`, apart so it can be tested.
fn observe(method: &str, listed: &mut bool, owed: &mut bool, told: &mut bool) {
    match method {
        "tools/list" => *listed = true,
        "tools/call" if !*listed && !*told => {
            *owed = true;
            *told = true;
        }
        _ => {}
    }
}

/// Whether stdin has something to read (or has closed) within `wait`.
#[cfg(unix)]
fn input_ready(wait: Duration) -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let ms = wait.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd, alive for the duration of the call.
    let n = unsafe { libc::poll(&mut fd, 1, ms) };
    // Timed out or interrupted: not yet. Readable, closed or failed: the read
    // that follows reports which.
    n > 0 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
}

/// No wait without poll: the read blocks, and a rebuild is looked for between
/// requests only.
#[cfg(not(unix))]
fn input_ready(_wait: Duration) -> bool {
    true
}

/// Become the build at `path` on the same pipes, so the client's connection
/// and anything it has already queued carry over. Returns only on failure.
#[cfg(unix)]
fn hand_over(path: &Path, name: &str, carry: &[(String, String)]) -> io::Error {
    use std::os::unix::process::CommandExt;
    eprintln!("{name}: binary rebuilt; handing this session to the new build");
    let mut next = std::process::Command::new(path);
    next.args(std::env::args_os().skip(1)).env(RESUMED_ENV, "1");
    for (key, value) in carry {
        next.env(format!("{CARRY_PREFIX}{key}"), value);
    }
    next.exec()
}

#[cfg(not(unix))]
fn hand_over(_path: &Path, _name: &str, _carry: &[(String, String)]) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "no exec on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_call_before_any_listing_owes_a_notice_and_only_once() {
        let (mut listed, mut owed, mut told) = (false, false, false);
        observe("initialize", &mut listed, &mut owed, &mut told);
        observe("tools/call", &mut listed, &mut owed, &mut told);
        assert!(owed && told, "a reconnect kept the old list");
        owed = false;
        observe("tools/call", &mut listed, &mut owed, &mut told);
        assert!(!owed, "told once per connection");

        let (mut listed, mut owed, mut told) = (false, false, false);
        observe("tools/list", &mut listed, &mut owed, &mut told);
        observe("tools/call", &mut listed, &mut owed, &mut told);
        assert!(!owed, "a client that listed tools has the current list");
    }

    #[test]
    fn a_replacement_counts_once_it_has_settled_and_only_until_tried() {
        let dir = std::env::temp_dir().join(format!("qx_stamp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bin");
        std::fs::write(&path, b"one").unwrap();
        let mut b = Binary { path: path.clone(), started: stamp(&path).unwrap(), tried: None };
        assert!(b.replaced().is_none());

        // Written now: still settling.
        std::fs::write(dir.join("next"), b"two!").unwrap();
        std::fs::rename(dir.join("next"), &path).unwrap();
        assert!(b.replaced().is_none(), "a file this fresh may still be written");

        let old = SystemTime::now() - Duration::from_secs(30);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        b.tried = Some(b.replaced().expect("a settled replacement"));
        assert!(b.replaced().is_none(), "a build already tried is not tried on every wait");
        std::fs::remove_dir_all(&dir).ok();
    }
}
