//! A running MCP server moves onto a rebuilt binary without dropping its
//! client, and a client holding a tool list from an earlier process is told to
//! fetch it again (src/mcp_session.rs).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

struct Server {
    child: Child,
    input: ChildStdin,
    lines: Receiver<String>,
    log: PathBuf,
}

impl Server {
    fn start(bin: &Path, dir: &Path) -> Server {
        let log = dir.join("stderr.log");
        let mut child = Command::new(bin)
            .arg("serve")
            .arg("--source")
            .arg(dir.join("src"))
            .args(["--name", "T"])
            .env_remove("CORTEX_SUITE_MCP_RESUMED")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Server { child, input, lines, log }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    /// The next line the server writes, or None if it writes nothing in `wait`.
    fn next(&self, wait: Duration) -> Option<Value> {
        match self.lines.recv_timeout(wait) {
            Ok(line) => Some(serde_json::from_str(&line).unwrap()),
            Err(RecvTimeoutError::Timeout) => None,
            Err(e) => panic!("the server's output ended ({e}); stderr:\n{}", self.stderr()),
        }
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = self.next(Duration::from_secs(60)).expect("an answer");
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    fn handshake(&mut self) -> Value {
        let init = self.request(
            1,
            "initialize",
            json!({ "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": { "name": "hot_swap", "version": "1" } }),
        );
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        init
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A source root to serve and a copy of the built binary to serve it with.
fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("qx_hot_swap_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "/// Says hello.\npub fn hello() {}\n").unwrap();
    let bin = dir.join("bin/quartz-ctx");
    install(&bin, std::fs::read(env!("CARGO_BIN_EXE_quartz-ctx")).unwrap());
    (dir, bin)
}

/// Replace `bin` with a new file, as cargo does, last written a while ago so
/// it counts as settled.
fn install(bin: &Path, content: Vec<u8>) {
    use std::os::unix::fs::PermissionsExt;
    let next = bin.with_extension("next");
    std::fs::write(&next, content).unwrap();
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).unwrap();
    let written = SystemTime::now() - Duration::from_secs(10);
    std::fs::File::options().write(true).open(&next).unwrap().set_modified(written).unwrap();
    std::fs::rename(&next, bin).unwrap();
}

fn tool_count(reply: &Value) -> usize {
    reply["result"]["tools"].as_array().map_or(0, |t| t.len())
}

#[test]
fn a_rebuilt_binary_takes_over_the_connection_while_idle() {
    let (dir, bin) = fixture("takeover");
    let mut s = Server::start(&bin, &dir);
    let init = s.handshake();
    assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], true, "{init}");
    let tools = tool_count(&s.request(2, "tools/list", json!({})));
    assert!(tools > 0);

    // The rebuild. The idle server finds it, has it answer a handshake, then
    // becomes it; the new build's first words are the notice.
    install(&bin, std::fs::read(env!("CARGO_BIN_EXE_quartz-ctx")).unwrap());
    let note = s.next(Duration::from_secs(30)).expect("a notice from the new build");
    assert_eq!(note["method"], "notifications/tools/list_changed", "{note}");
    assert!(s.child.try_wait().unwrap().is_none(), "the same process carries on");
    let log = s.stderr();
    assert!(log.contains("handing this session to the new build"), "{log}");
    assert!(log.contains("continuing this session on the rebuilt binary"), "{log}");

    // The connection carries on with no second initialize.
    assert_eq!(tool_count(&s.request(3, "tools/list", json!({}))), tools);
    let call = s.request(4, "tools/call", json!({ "name": "get_source", "arguments": { "name": "hello" } }));
    let text = call["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("pub fn hello"), "{call}");
    assert!(s.next(Duration::from_secs(1)).is_none(), "nothing more is owed");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_call_on_a_connection_that_never_listed_tools_is_followed_by_the_notice() {
    let (dir, bin) = fixture("relist");
    let mut s = Server::start(&bin, &dir);
    s.handshake();
    // A client that started the server again keeps its old list and calls at once.
    let call = s.request(2, "tools/call", json!({ "name": "get_source", "arguments": { "name": "hello" } }));
    assert!(call["result"].is_object(), "{call}");
    let note = s.next(Duration::from_secs(10)).expect("told to fetch the list again");
    assert_eq!(note["method"], "notifications/tools/list_changed", "{note}");
    s.request(3, "tools/list", json!({}));
    s.request(4, "tools/call", json!({ "name": "get_source", "arguments": { "name": "hello" } }));
    assert!(s.next(Duration::from_secs(1)).is_none(), "told once per connection");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_connection_that_listed_tools_is_not_told() {
    let (dir, bin) = fixture("listed");
    let mut s = Server::start(&bin, &dir);
    s.handshake();
    s.request(2, "tools/list", json!({}));
    s.request(3, "tools/call", json!({ "name": "get_source", "arguments": { "name": "hello" } }));
    assert!(s.next(Duration::from_secs(1)).is_none(), "its list is current");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rebuild_that_does_not_serve_is_not_taken() {
    let (dir, bin) = fixture("broken");
    let mut s = Server::start(&bin, &dir);
    s.handshake();
    let tools = tool_count(&s.request(2, "tools/list", json!({})));

    // A "build" that starts and exits without answering.
    install(&bin, b"#!/bin/sh\nexit 0\n".to_vec());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !s.stderr().contains("keeps serving") {
        assert!(Instant::now() < deadline, "the broken build was never tried:\n{}", s.stderr());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(s.next(Duration::from_millis(200)).is_none(), "no notice for a build not taken");
    assert_eq!(tool_count(&s.request(3, "tools/list", json!({}))), tools, "the old build still serves");
    assert!(!s.stderr().contains("handing this session"), "{}", s.stderr());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}
