//! `cortex graphify-serve` moves onto a rebuilt cortex binary without dropping
//! its client: the new build's child is given the client's handshake, and the
//! old child is stopped and reaped first (src/graphify_proxy.rs). A child that
//! dies is started again the same way.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

/// Stands in for graphify-rs: answers the handshake, lists one tool, and says
/// on every call which process answered and whether it saw `initialize`.
const FAKE_GRAPHIFY: &str = r##"#!/bin/sh
init=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      init=1
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fake-graphify","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"graph_stats","description":"d","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pid=%s initialized=%s"}]}}\n' "$id" "$$" "$init" ;;
  esac
done
"##;

/// A workspace with a current graph, removed when the test ends. Everything
/// sits in dot-directories, which the proxy's staleness walk skips, so no call
/// starts a real graphify-rs rebuild.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("cx_graphify_swap_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bin")).unwrap();
        std::fs::create_dir_all(dir.join(".graphify-output")).unwrap();
        std::fs::write(dir.join(".graphify-output/graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
        install(&dir.join(".bin/graphify"), FAKE_GRAPHIFY.as_bytes().to_vec());
        Workspace(dir)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

struct Proxy {
    child: Child,
    input: ChildStdin,
    lines: Receiver<String>,
    log: PathBuf,
}

impl Proxy {
    fn start(cortex: &Path, ws: &Workspace) -> Proxy {
        let log = ws.0.join("stderr.log");
        let mut child = Command::new(cortex)
            .args(["graphify-serve", "--repo"])
            .arg(&ws.0)
            .args(["--graph", ".graphify-output/graph.json", "--graphify"])
            .arg(ws.0.join(".bin/graphify"))
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
        Proxy { child, input, lines, log }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    /// The next line the proxy writes, or None if it writes nothing in `wait`.
    fn next(&self, wait: Duration) -> Option<Value> {
        match self.lines.recv_timeout(wait) {
            Ok(line) => Some(serde_json::from_str(&line).unwrap()),
            Err(RecvTimeoutError::Timeout) => None,
            Err(e) => panic!("the proxy's output ended ({e}); stderr:\n{}", self.stderr()),
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
                    "clientInfo": { "name": "graphify_hot_swap", "version": "1" } }),
        );
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        init
    }

    /// Which graphify child answered a call, and whether it had been initialized.
    fn call(&mut self, id: i64) -> (String, String) {
        let reply = self.request(id, "tools/call", json!({ "name": "graph_stats", "arguments": {} }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
        let field = |key: &str| {
            text.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {reply}"))
                .to_string()
        };
        (field("pid="), field("initialized="))
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ps` state of a process: "" once it is gone from the process table, a
/// state starting with Z while it has exited but nobody has reaped it.
fn state(pid: &str) -> String {
    let out = Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_rebuilt_cortex_takes_over_the_proxy_and_gives_its_child_the_handshake() {
    let ws = Workspace::new("takeover");
    let cortex = ws.0.join(".bin/cortex");
    install(&cortex, std::fs::read(env!("CARGO_BIN_EXE_cortex")).unwrap());
    let mut p = Proxy::start(&cortex, &ws);
    let init = p.handshake();
    assert_eq!(init["result"]["serverInfo"]["name"], "fake-graphify", "{init}");
    let (old, initialized) = p.call(2);
    assert_eq!(initialized, "1");

    // The rebuild. The idle proxy has the new build answer a handshake, lets
    // its child go, and becomes the new build; whose first words are the notice.
    install(&cortex, std::fs::read(env!("CARGO_BIN_EXE_cortex")).unwrap());
    let note = p.next(Duration::from_secs(60)).expect("a notice from the new build");
    assert_eq!(note["method"], "notifications/tools/list_changed", "{note}");
    assert!(p.child.try_wait().unwrap().is_none(), "the same process carries on");
    let log = p.stderr();
    assert!(log.contains("handing this session to the new build"), "{log}");
    assert!(log.contains("continuing this session on the rebuilt binary"), "{log}");

    // The old child is gone, not left a zombie, and the new build's child was
    // given the client's handshake: the client never sends a second one.
    assert_eq!(state(&old), "", "the old child is still in the process table:\n{log}");
    let (new, initialized) = p.call(3);
    assert_ne!(new, old, "a child of the new build answers");
    assert_eq!(initialized, "1", "the new child was not given the client's initialize");
    let tools = p.request(4, "tools/list", json!({}));
    assert_eq!(tools["result"]["tools"][0]["name"], "graph_stats", "{tools}");
    assert!(p.next(Duration::from_secs(1)).is_none(), "nothing more is owed");
}

#[test]
fn a_child_that_died_is_started_again_and_given_the_handshake() {
    let ws = Workspace::new("respawn");
    let mut p = Proxy::start(Path::new(env!("CARGO_BIN_EXE_cortex")), &ws);
    p.handshake();
    let (old, _) = p.call(2);

    // Killed between calls; the proxy finds out at the next request.
    Command::new("kill").arg(&old).status().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !state(&old).starts_with('Z') {
        assert!(Instant::now() < deadline, "the child did not exit: {}", state(&old));
        std::thread::sleep(Duration::from_millis(50));
    }
    let (new, initialized) = p.call(3);
    assert_ne!(new, old);
    assert_eq!(initialized, "1", "the restarted child was not given the client's initialize");
    assert!(p.stderr().contains("started graphify-rs again"), "{}", p.stderr());
    assert_eq!(state(&old), "", "the dead child was not reaped");
}
