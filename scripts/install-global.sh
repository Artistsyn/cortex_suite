#!/usr/bin/env bash
# Install cortex + quartz-ctx for EVERY project (macOS/Linux), instead of one
# workspace at a time like setup.sh.
#
#   ./scripts/install-global.sh [--claude-config DIR]... [--no-claude] [--no-vscode] [--no-codex] [--skip-build]
#
# What it does:
#   1. builds both servers (unless --skip-build);
#   2. writes two launchers to ~/.cortex-suite/bin that serve whichever folder
#      the assistant was started in: its own memory under
#      ~/.cortex-suite/projects/ (or its .cortex/memory.db, if set up with
#      setup.sh), code found from .cortex/index-sources.json, a Rust project's
#      crates, or the folder itself. The home folder (or /) is never indexed;
#   3. registers them, for all projects, with:
#        - Claude Code: the default account, every ~/.claude-* folder that holds
#          an account, and each --claude-config DIR;
#        - VS Code (Copilot agent mode): the user mcp.json;
#        - Codex CLI: ~/.codex/config.toml;
#      skipping any that isn't installed;
#   4. checks each Claude Code account connects.
#
# Safe to re-run: it replaces its own entries and leaves everything else alone.
set -euo pipefail

SUITE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.cortex-suite/bin"
SKIP_BUILD=0
DO_CLAUDE=1
DO_VSCODE=1
DO_CODEX=1
EXTRA_CONFIGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --claude-config) EXTRA_CONFIGS+=("$2"); shift ;;
    --no-claude)     DO_CLAUDE=0 ;;
    --no-vscode)     DO_VSCODE=0 ;;
    --no-codex)      DO_CODEX=0 ;;
    --skip-build)    SKIP_BUILD=1 ;;
    -h|--help)       sed -n '2,24p' "$0"; exit 0 ;;
    *)               printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

say()  { printf '[install] %s\n' "$1"; }
warn() { printf '[install] WARN: %s\n' "$1" >&2; }
die()  { printf '[install] ERROR: %s\n' "$1" >&2; exit 1; }

# 1. Build.
if [ "$SKIP_BUILD" -eq 0 ]; then
  command -v cargo >/dev/null 2>&1 || die "cargo not found: install Rust from https://rustup.rs and open a new terminal"
  say "building cortex..."
  ( cd "$SUITE_ROOT/cortex" && cargo build ) || die "cortex build failed"
  say "building quartz-ctx..."
  ( cd "$SUITE_ROOT/quartz-ctx" && cargo build --release ) || die "quartz-ctx build failed"
fi
CORTEX_EXE="$SUITE_ROOT/cortex/target/debug/cortex"
QCTX_EXE="$SUITE_ROOT/quartz-ctx/target/release/quartz-ctx"
[ -x "$CORTEX_EXE" ] || die "missing $CORTEX_EXE (run without --skip-build)"
[ -x "$QCTX_EXE" ]   || die "missing $QCTX_EXE (run without --skip-build)"

# 2. Launchers.
mkdir -p "$BIN_DIR" "$HOME/.cortex-suite/empty"
cat > "$BIN_DIR/cortex-mcp" <<EOF
#!/bin/sh
# cortex for whichever project the assistant started in (written by
# cortex_suite/scripts/install-global.sh).
CX="$CORTEX_EXE"
NAME="\$(basename "\$PWD")"
case "\$PWD" in
  "\$HOME"|/) exec "\$CX" --db "\$HOME/.cortex-suite/home.db" serve --repo "\$HOME/.cortex-suite/empty" --name home ;;
esac
if [ -f .cortex/memory.db ] || [ -f .cortex/index-sources.json ]; then
  # A project set up with setup.sh (or used before) keeps its own memory.
  DB=.cortex/memory.db
else
  # Memory lives outside the project, one folder per project path, so no
  # database lands in a repository to be committed by accident.
  KEY="\$(printf '%s' "\$PWD" | tr -c 'A-Za-z0-9._-' '_')"
  mkdir -p "\$HOME/.cortex-suite/projects/\$KEY"
  DB="\$HOME/.cortex-suite/projects/\$KEY/memory.db"
  # cortex still keeps a small marker in .cortex/: keep that folder out of git.
  if [ ! -e .cortex ]; then
    mkdir .cortex && printf '*\\n' > .cortex/.gitignore
  fi
fi
# cortex installs its Claude Code hooks in .claude/settings.local.json, a file
# for this machine only: keep it out of git through the repository's own
# exclude list, which changes nothing anyone else sees.
if PREFIX="\$(git rev-parse --show-prefix 2>/dev/null)"; then
  EXCLUDE="\$(git rev-parse --git-path info/exclude)"
  PATTERN="/\${PREFIX}.claude/settings.local.json"
  if ! grep -qxF "\$PATTERN" "\$EXCLUDE" 2>/dev/null; then
    mkdir -p "\$(dirname "\$EXCLUDE")"
    # A last line without its newline would swallow the pattern.
    [ -s "\$EXCLUDE" ] && [ -n "\$(tail -c 1 "\$EXCLUDE")" ] && printf '\\n' >> "\$EXCLUDE"
    printf '%s\\n' "\$PATTERN" >> "\$EXCLUDE"
  fi
fi
exec "\$CX" --db "\$DB" serve --repo . --name "\$NAME"
EOF
cat > "$BIN_DIR/quartz-ctx-mcp" <<EOF
#!/bin/sh
# quartz-ctx for whichever project the assistant started in (written by
# cortex_suite/scripts/install-global.sh). Uses .cortex/index-sources.json if
# the project has one, else a Rust project's crates, else the folder itself.
QX="$QCTX_EXE"
NAME="\$(basename "\$PWD")"
case "\$PWD" in
  "\$HOME"|/) exec "\$QX" serve --source "\$HOME/.cortex-suite/empty" --name home ;;
esac
if [ -f .cortex/index-sources.json ]; then
  exec "\$QX" serve --sources-from .cortex/index-sources.json --name "\$NAME"
fi
if find . -maxdepth 3 -name Cargo.toml -not -path '*/target/*' 2>/dev/null | grep -q .; then
  exec "\$QX" serve --discover . --include-private --name "\$NAME"
fi
exec "\$QX" serve --source . --include-private --name "\$NAME"
EOF
chmod +x "$BIN_DIR/cortex-mcp" "$BIN_DIR/quartz-ctx-mcp"
say "wrote launchers in $BIN_DIR"

# 3a. Claude Code: every account.
CLAUDE_BIN="$(command -v claude || true)"
if [ "$DO_CLAUDE" -eq 1 ] && [ -n "$CLAUDE_BIN" ]; then
  configs=("")   # "" = the default account (~/.claude.json)
  for d in "$HOME"/.claude-*; do
    [ -f "$d/.claude.json" ] && configs+=("$d")
  done
  for d in "${EXTRA_CONFIGS[@]+"${EXTRA_CONFIGS[@]}"}"; do configs+=("$d"); done
  for cfg in "${configs[@]}"; do
    label="${cfg:-default account}"
    for pair in "cortex:$BIN_DIR/cortex-mcp" "quartz-ctx:$BIN_DIR/quartz-ctx-mcp"; do
      name="${pair%%:*}"; cmd="${pair#*:}"
      if [ -n "$cfg" ]; then
        CLAUDE_CONFIG_DIR="$cfg" "$CLAUDE_BIN" mcp remove --scope user "$name" >/dev/null 2>&1 || true
        CLAUDE_CONFIG_DIR="$cfg" "$CLAUDE_BIN" mcp add --scope user "$name" -- "$cmd" >/dev/null
      else
        "$CLAUDE_BIN" mcp remove --scope user "$name" >/dev/null 2>&1 || true
        "$CLAUDE_BIN" mcp add --scope user "$name" -- "$cmd" >/dev/null
      fi
    done
    say "Claude Code ($label): registered cortex and quartz-ctx for all projects"
  done
elif [ "$DO_CLAUDE" -eq 1 ]; then
  say "Claude Code not found, skipped (https://docs.claude.com/en/docs/claude-code/setup)"
fi

# 3b. VS Code (Copilot agent mode): the user-level mcp.json.
case "$(uname -s)" in
  Darwin) VSCODE_USER="$HOME/Library/Application Support/Code/User" ;;
  *)      VSCODE_USER="${XDG_CONFIG_HOME:-$HOME/.config}/Code/User" ;;
esac
if [ "$DO_VSCODE" -eq 1 ] && [ -d "$VSCODE_USER" ]; then
  if command -v python3 >/dev/null 2>&1; then
    # A config it can't read is reported and skipped; the install carries on.
    python3 - "$VSCODE_USER/mcp.json" "$BIN_DIR" <<'PY' || warn "VS Code: skipped (see above)"
import json, os, sys
path, bin_dir = sys.argv[1], sys.argv[2]
data = {}
if os.path.exists(path):
    try:
        data = json.load(open(path))
    except Exception:
        print(f"[install] WARN: {path} isn't plain JSON (comments?); add cortex and quartz-ctx to it by hand, with commands {bin_dir}/cortex-mcp and {bin_dir}/quartz-ctx-mcp", file=sys.stderr)
        sys.exit(1)
servers = data.setdefault("servers", {})
servers["cortex"] = {"type": "stdio", "command": f"{bin_dir}/cortex-mcp", "args": []}
servers["quartz-ctx"] = {"type": "stdio", "command": f"{bin_dir}/quartz-ctx-mcp", "args": []}
json.dump(data, open(path, "w"), indent=2)
print(f"[install] VS Code: registered cortex and quartz-ctx in {path}")
PY
  else
    warn "python3 not found: add cortex and quartz-ctx to $VSCODE_USER/mcp.json by hand"
  fi
fi

# 3c. Codex CLI.
CODEX_CFG="$HOME/.codex/config.toml"
if [ "$DO_CODEX" -eq 1 ] && [ -d "$HOME/.codex" ]; then
  touch "$CODEX_CFG"
  if grep -q '^# cortex_suite begin' "$CODEX_CFG"; then
    # Replace our block.
    awk '/^# cortex_suite begin/{skip=1} !skip{print} /^# cortex_suite end/{skip=0}' "$CODEX_CFG" > "$CODEX_CFG.tmp" && mv "$CODEX_CFG.tmp" "$CODEX_CFG"
  fi
fi
# Servers named cortex or quartz-ctx that the user added themselves: a second
# table with the same name would break the file, so leave Codex alone.
if [ "$DO_CODEX" -eq 1 ] && [ -d "$HOME/.codex" ] && grep -Eq '^[[:space:]]*\[mcp_servers\.("?cortex"?|"?quartz-ctx"?)\]' "$CODEX_CFG"; then
  warn "Codex: $CODEX_CFG already has a cortex or quartz-ctx server; left it as it is"
elif [ "$DO_CODEX" -eq 1 ] && [ -d "$HOME/.codex" ]; then
  cat >> "$CODEX_CFG" <<EOF
# cortex_suite begin (written by install-global.sh)
[mcp_servers.cortex]
command = "$BIN_DIR/cortex-mcp"

[mcp_servers."quartz-ctx"]
command = "$BIN_DIR/quartz-ctx-mcp"
# cortex_suite end
EOF
  say "Codex: registered cortex and quartz-ctx in $CODEX_CFG"
fi

# 4. Check.
if [ "$DO_CLAUDE" -eq 1 ] && [ -n "$CLAUDE_BIN" ]; then
  say "checking Claude Code (from $SUITE_ROOT):"
  ( cd "$SUITE_ROOT" && "$CLAUDE_BIN" mcp list 2>/dev/null | grep -E '^(cortex|quartz-ctx):' | sed 's/^/[install]   /' ) || true
fi

say ""
say "Done. Restart your editor or assistant to load the servers."
