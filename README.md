# Cortex Suite

Gives your AI coding assistant a memory and a map of your code.

- **Remembers** what worked and what broke, so mistakes aren't repeated.
- **Looks up your real code** instead of guessing names and types.
- **Stays current**: change a file and it already knows.

Works with Claude Code, VS Code Copilot and Codex, in 10 languages. Everything
stays on your computer.

## Install

### Easiest: let your AI install it

Don't have an AI coding assistant yet?
[Install Claude Code](https://docs.claude.com/en/docs/claude-code/setup).

Then copy the prompt for your assistant and paste it in. It installs Cortex
Suite for **all** your projects.

<details>
<summary><b>Claude Code</b></summary>

```text
Install Cortex Suite for all of my projects.

1. If `cargo` isn't installed, install Rust from https://rustup.rs first.
2. Clone https://github.com/Artistsyn/cortex_suite into ~/cortex_suite
   (if it's already there, run `git pull` in it instead).
3. From inside ~/cortex_suite, run ./scripts/install-global.sh
   If I have more than one Claude Code account (look in my shell profile for
   aliases that set CLAUDE_CONFIG_DIR), add `--claude-config <folder>` for each
   folder that isn't already a ~/.claude-* folder.
4. Show me the cortex and quartz-ctx lines from `claude mcp list`, then tell me
   to restart Claude Code.
```
</details>

<details>
<summary><b>GitHub Copilot (VS Code, agent mode)</b></summary>

```text
Install Cortex Suite so you can use it in all of my projects. Run each step in
the terminal and check it worked before moving on.

1. Check the tools it needs: `git --version`, `cargo --version`, and a C
   compiler (on a Mac: `xcode-select -p`; on Linux: `cc --version`).
   - If cargo is missing, install Rust from https://rustup.rs, then open a new
     terminal so cargo is on the PATH.
   - If the C compiler is missing: on a Mac run `xcode-select --install`; on
     Linux install build-essential.
2. Clone https://github.com/Artistsyn/cortex_suite into ~/cortex_suite
   (if it's already there, run `git pull` in it instead).
3. From inside ~/cortex_suite, run ./scripts/install-global.sh
   It builds two MCP servers (cortex and quartz-ctx) and adds them to my VS Code
   user MCP config (mcp.json in VS Code's User folder), so they work in every
   workspace. The build takes a few minutes.
4. Open that user mcp.json (Command Palette: "MCP: Open User Configuration")
   and confirm it has "cortex" and "quartz-ctx" entries.
5. Tell me to reload the window, then run "MCP: List Servers" and start both,
   and to turn their tools on in the agent's tool picker.
6. If this workspace is a git repo, also run
   ~/cortex_suite/cortex/target/debug/cortex hooks-init --vscode
   so cortex can warn you about known mistakes as you work.

I'm on Windows? The install script needs macOS or Linux. Instead, run
.\scripts\setup.ps1 -Workspace <project folder> from ~/cortex_suite for each
project.
```
</details>

<details>
<summary><b>ChatGPT</b></summary>

```text
I want to install Cortex Suite (https://github.com/Artistsyn/cortex_suite).
It's a pair of local MCP servers, "cortex" (memory) and "quartz-ctx" (code
map), that give AI coding assistants like Claude Code, VS Code Copilot and the
Codex CLI a memory and a map of my code. You can't run commands on my computer,
so walk me through it: give me ONE step at a time, tell me exactly what to type
and what I should see, and wait for me to paste the result before the next step.
If something fails, help me fix it before moving on.

First ask me which operating system I use (macOS, Linux or Windows) and which
assistants I use (Claude Code, VS Code Copilot, Codex CLI).

The steps, for macOS and Linux:

1. Open a terminal.
2. Check git: `git --version`. If it's missing: on a Mac, `xcode-select
   --install`; on Linux, install git with the package manager.
3. Check a C compiler: on a Mac `xcode-select -p` (if it errors, run
   `xcode-select --install` and wait for it to finish); on Linux
   `cc --version` (if missing, `sudo apt install build-essential` or the
   distro's equivalent).
4. Check Rust: `cargo --version`. If it's missing, run
   `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`, accept the
   defaults, then close the terminal and open a new one, and check again.
5. Download it: `git clone https://github.com/Artistsyn/cortex_suite ~/cortex_suite`
   (if the folder already exists: `cd ~/cortex_suite && git pull`).
6. Install it for every project: `cd ~/cortex_suite && ./scripts/install-global.sh`
   The build takes a few minutes. It then registers both servers with every
   assistant it finds: each Claude Code account, VS Code's user MCP config, and
   ~/.codex/config.toml. Its output says which ones it set up. If I use more
   than one Claude Code account through CLAUDE_CONFIG_DIR, add
   `--claude-config <that folder>` for each.
7. Check it worked, for each assistant I use:
   - Claude Code: `claude mcp list` should show cortex and quartz-ctx as
     Connected. Then restart Claude Code.
   - VS Code Copilot: reload the window, run "MCP: List Servers" from the
     Command Palette, start both, and turn their tools on in agent mode.
   - Codex CLI: start a new codex session; the servers load from config.toml.
For Windows, the install script doesn't run. Instead, after steps 2-5 (Git for
Windows, Visual Studio Build Tools with the C++ workload, Rust from
https://rustup.rs, and the clone), run in PowerShell from the cortex_suite
folder, once per project:
`.\scripts\setup.ps1 -Workspace C:\path\to\project`

Common problems: "cargo: command not found" means the terminal needs
reopening after installing Rust. A build error about "cc" or "linker" means the
C compiler from step 3 is missing. A warning about mcp.json means VS Code's
config has comments in it; then add the two servers by hand, with commands
~/.cortex-suite/bin/cortex-mcp and ~/.cortex-suite/bin/quartz-ctx-mcp.
```
</details>

### Or by hand

You need [Rust](https://rustup.rs).

```bash
git clone https://github.com/Artistsyn/cortex_suite ~/cortex_suite
~/cortex_suite/scripts/install-global.sh
```

Then restart your editor. Or, to set up just one project:
`~/cortex_suite/scripts/setup.sh ~/code/my-project`
(on Windows: `.\scripts\setup.ps1 -Workspace C:\code\my-project`).

## Is it working?

In any project you've worked in:

```bash
~/cortex_suite/cortex/target/debug/cortex scoreboard
```

## Update

`git pull` in `cortex_suite`, then run the install again.

---

Details: [TECHNICAL.md](TECHNICAL.md) · Help: [SETUP_HANDOFF.md](SETUP_HANDOFF.md)
