//! `cortex reload-servers`: put running MCP servers on the binary now on disk.
//!
//! A server built with `quartz_ctx::mcp_session` moves itself onto a rebuilt
//! binary while it waits for a request. One built before that cannot, and keeps
//! the code it started with for the life of its session, which for long work is
//! days. This stops those that are idle. Claude Code starts a stopped stdio
//! server again on the session's next call to it, from the binary on disk.
//!
//! That moves the code, not the session's tool list. The new build tells the
//! session to fetch the list again, but Claude Code 2.1.284 listens for that
//! only on a connection it opened at session start or reconnected with /mcp in
//! a terminal (or adopted through its MCP discovery cache, when that is on); a
//! server it started again by itself after a call is not one of those.
//! Observed 2026-10-01: the restarted quartz-ctx served 14 tools and sent the
//! notice, and the session kept the 13 it had. A fresh connection refreshes the
//! list and subscribes it, after which rebuilds reach the session with their
//! tools. The desktop app has no way to give a local server one short of a
//! restart: its /mcp reconnects only remote servers, and the typed
//! `/mcp reconnect` answers "aren't available in this session".
//!
//! Only cortex and quartz-ctx servers (`serve`, `graphify-serve`) whose parent
//! is a Claude Code process are stopped. Other hosts are reported and left
//! alone: whether VS Code starts a stopped server again is not known.

use anyhow::Result;

/// Executables whose servers this may stop.
#[cfg(unix)]
const OURS: &[&str] = &["cortex", "quartz-ctx"];

pub fn run(dry_run: bool) -> Result<()> {
    imp::run(dry_run)
}

#[cfg(not(unix))]
mod imp {
    pub fn run(_dry_run: bool) -> anyhow::Result<()> {
        println!(
            "reload-servers needs a Unix process table. Reconnect the servers from the host \
             instead: /mcp in Claude Code, \"MCP: Restart Server\" in VS Code."
        );
        Ok(())
    }
}

#[cfg(unix)]
mod imp {
    use super::OURS;
    use anyhow::{Context, Result};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct Proc {
        pid: u32,
        ppid: u32,
        stat: String,
        cpu: String,
        command: String,
    }

    /// The process table: pid, parent, state, CPU time and command line.
    fn processes(pids: Option<&[u32]>) -> Result<Vec<Proc>> {
        let mut ps = Command::new("ps");
        match pids {
            Some(p) => {
                let list: Vec<String> = p.iter().map(u32::to_string).collect();
                ps.args(["-o", "pid=,ppid=,stat=,time=,command=", "-p", &list.join(",")]);
            }
            None => {
                ps.args(["-axo", "pid=,ppid=,stat=,time=,command="]);
            }
        }
        let out = ps.output().context("running ps")?;
        Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_row).collect())
    }

    fn parse_row(line: &str) -> Option<Proc> {
        let mut rest = line.trim_start();
        let mut field = || {
            let (f, r) = rest.split_once(char::is_whitespace)?;
            rest = r.trim_start();
            Some(f.to_string())
        };
        let (pid, ppid, stat, cpu) = (field()?, field()?, field()?, field()?);
        Some(Proc { pid: pid.parse().ok()?, ppid: ppid.parse().ok()?, stat, cpu, command: rest.to_string() })
    }

    /// The executable a process runs and the inode it was started from, which
    /// a rebuild does not change.
    #[cfg(target_os = "linux")]
    fn running_image(pid: u32) -> Option<(PathBuf, u64)> {
        use std::os::unix::fs::MetadataExt;
        let link = PathBuf::from(format!("/proc/{pid}/exe"));
        let inode = std::fs::metadata(&link).ok()?.ino();
        let target = std::fs::read_link(&link).ok()?;
        let target = target.to_string_lossy();
        Some((PathBuf::from(target.strip_suffix(" (deleted)").unwrap_or(&target)), inode))
    }

    #[cfg(not(target_os = "linux"))]
    fn running_image(pid: u32) -> Option<(PathBuf, u64)> {
        let args = ["-a", "-p", &pid.to_string(), "-d", "txt", "-Fin"];
        let out = Command::new("lsof")
            .args(args)
            .output()
            .or_else(|_| Command::new("/usr/sbin/lsof").args(args).output())
            .ok()?;
        // One i<inode> line, then one n<path> line, per mapped file; the
        // executable is the mapped file named like one of ours.
        let mut inode = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(i) = line.strip_prefix('i') {
                inode = i.parse().ok();
            } else if let Some(n) = line.strip_prefix('n') {
                let path = PathBuf::from(n);
                if is_ours(&path) {
                    return Some((path, inode?));
                }
            }
        }
        None
    }

    fn is_ours(path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()).is_some_and(|n| OURS.contains(&n))
    }

    fn inode_now(path: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.ino())
    }

    /// The server's subcommand, when the process is one of our MCP servers.
    fn server_kind(command: &str) -> Option<&'static str> {
        let words: Vec<&str> = command.split_whitespace().collect();
        let kind = if words.contains(&"graphify-serve") {
            "graphify-serve"
        } else if words.contains(&"serve") {
            "serve"
        } else {
            return None;
        };
        // A path may hold spaces, so the executable is found by name.
        let named = OURS.iter().any(|ours| {
            words.iter().any(|w| Path::new(w).file_name().and_then(|n| n.to_str()) == Some(ours))
        });
        named.then_some(kind)
    }

    /// Claude Code, as the desktop app, the native CLI, the npm package or the
    /// IDE extensions run it.
    fn is_claude_code(command: &str) -> bool {
        let program = command.split_whitespace().next().unwrap_or("");
        command.contains("claude-code")
            || matches!(Path::new(program).file_name().and_then(|n| n.to_str()), Some("claude"))
    }

    fn host_label(command: &str) -> String {
        if is_claude_code(command) {
            "Claude Code".into()
        } else if command.contains("Code Helper") || command.contains("Visual Studio Code") {
            "VS Code".into()
        } else {
            let program = command.split_whitespace().next().unwrap_or("?");
            Path::new(program).file_name().map_or(program.into(), |n| n.to_string_lossy().into_owned())
        }
    }

    struct Server {
        pid: u32,
        parent: u32,
        name: String,
        host: String,
        claude: bool,
        stale: bool,
        quiet: bool,
    }

    pub fn run(dry_run: bool) -> Result<()> {
        let table = processes(None)?;
        let commands: HashMap<u32, &str> = table.iter().map(|p| (p.pid, p.command.as_str())).collect();
        let mut servers = Vec::new();
        for p in &table {
            let Some(kind) = server_kind(&p.command) else { continue };
            let Some((exe, running)) = running_image(p.pid) else { continue };
            let name = exe.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
            let parent = commands.get(&p.ppid).copied().unwrap_or("");
            servers.push(Server {
                pid: p.pid,
                parent: p.ppid,
                name: format!("{name} {kind}"),
                host: host_label(parent),
                claude: is_claude_code(parent),
                // A binary that is gone cannot be started again: leave it running.
                stale: inode_now(&exe).is_some_and(|now| now != running),
                quiet: p.stat.starts_with(['S', 'I']),
            });
        }

        // Idle means asleep and no CPU used across half a second, so a call in
        // progress is not cut off.
        let watched: Vec<u32> = servers.iter().filter(|s| s.stale && s.claude && s.quiet).map(|s| s.pid).collect();
        if !watched.is_empty() {
            let before: HashMap<u32, String> =
                table.iter().filter(|p| watched.contains(&p.pid)).map(|p| (p.pid, p.cpu.clone())).collect();
            std::thread::sleep(Duration::from_millis(500));
            let after = processes(Some(&watched))?;
            for s in servers.iter_mut().filter(|s| watched.contains(&s.pid)) {
                s.quiet = after.iter().any(|a| {
                    a.pid == s.pid && a.stat.starts_with(['S', 'I']) && before.get(&s.pid) == Some(&a.cpu)
                });
            }
        }

        let current = servers.iter().filter(|s| !s.stale).count();
        let stale: Vec<&Server> = servers.iter().filter(|s| s.stale).collect();
        if stale.is_empty() {
            println!("reload-servers: every cortex_suite MCP server runs the binary on disk ({current} running).");
            return Ok(());
        }

        let mut stopped = Vec::new();
        for s in &stale {
            if !(s.claude && s.quiet) || dry_run {
                continue;
            }
            let ok = Command::new("kill").args(["-TERM", &s.pid.to_string()]).status().is_ok_and(|st| st.success());
            if ok {
                stopped.push(s.pid);
            }
        }
        // Report a stop only once the process is gone.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut alive: Vec<u32> = stopped.clone();
        while !alive.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let left = processes(Some(&alive))?;
            alive.retain(|pid| left.iter().any(|p| p.pid == *pid));
        }

        let stop = if dry_run { "would stop" } else { "stopped" };
        println!(
            "reload-servers: {} of {} cortex_suite MCP servers run a replaced binary{}",
            stale.len(),
            servers.len(),
            if dry_run { " (dry run: nothing stopped)" } else { "" }
        );
        for s in &stale {
            let (label, why) = if !s.claude {
                ("left", ": not a Claude Code session; restart it from that host")
            } else if !s.quiet {
                ("left", ": busy; run this again when it is idle")
            } else if dry_run || (stopped.contains(&s.pid) && !alive.contains(&s.pid)) {
                (stop, "")
            } else if alive.contains(&s.pid) {
                ("signalled", ": still running after 3 s")
            } else {
                ("left", ": could not be signalled")
            };
            println!("  {label:<10} {:<22} pid {:<7} {} {}{why}", s.name, s.pid, s.host, s.parent);
        }
        if current > 0 {
            println!("{current} already on the current binary.");
        }
        if !dry_run && !stopped.is_empty() {
            println!(
                "Each stopped server starts again on its session's next call to it, on the current \
                 binary. The session keeps the tool list it had until it gets a fresh connection: \
                 /mcp reconnect in a terminal session, a restart of the desktop app there. After \
                 that, rebuilds reach it with their tools, because servers built from now on move \
                 themselves onto a rebuild while idle."
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rows_keep_spaces_in_the_command() {
            let p = parse_row("  39454     1 S     12:01.50 /Users/u/Library/Application Support/Claude/claude-code/2.1.284/claude.app/Contents/MacOS/claude --verbose").unwrap();
            assert_eq!((p.pid, p.ppid, p.stat.as_str(), p.cpu.as_str()), (39454, 1, "S", "12:01.50"));
            assert!(p.command.starts_with("/Users/u/Library/Application Support/"));
            assert!(is_claude_code(&p.command));
        }

        #[test]
        fn only_our_servers_are_candidates() {
            assert_eq!(server_kind("cortex_suite/cortex/target/debug/cortex --db .cortex/memory.db serve --repo ."), Some("serve"));
            assert_eq!(server_kind("cortex_suite/cortex/target/debug/cortex graphify-serve --repo ."), Some("graphify-serve"));
            assert_eq!(server_kind("/x/quartz-ctx serve --sources-from s.json"), Some("serve"));
            assert_eq!(server_kind("/x/quartz-ctx nav hook"), None, "hooks are not servers");
            assert_eq!(server_kind("graphify-rs serve --graph g.json"), None, "not ours");
            assert_eq!(server_kind("cortex reload-servers"), None);
        }

        #[test]
        fn hosts_are_told_apart() {
            assert!(is_claude_code("/Users/u/.local/bin/claude --resume"));
            assert!(is_claude_code("node /usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js"));
            assert!(!is_claude_code("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin) --type=utility"));
            assert_eq!(host_label("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)"), "VS Code");
        }
    }
}
