#!/usr/bin/env bash
# Build and wire the cortex + quartz-ctx MCP suite into a workspace (macOS/Linux).
#
#   ./setup.sh <workspace-path> [--force] [--skip-build]
#
# Safe to re-run: never overwrites an existing config unless --force.
set -euo pipefail

SUITE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE=""
FORCE=0
SKIP_BUILD=0

for arg in "$@"; do
  case "$arg" in
    --force)      FORCE=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    -h|--help)    sed -n '2,8p' "$0"; exit 0 ;;
    *)            WORKSPACE="$arg" ;;
  esac
done

say()  { printf '[setup] %s\n' "$1"; }
warn() { printf '[setup] WARN: %s\n' "$1" >&2; }
die()  { printf '[setup] ERROR: %s\n' "$1" >&2; exit 1; }

[ -n "$WORKSPACE" ] || die "usage: ./setup.sh <workspace-path> [--force] [--skip-build]"
[ -d "$WORKSPACE" ] || die "workspace not found: $WORKSPACE"
WORKSPACE="$(cd "$WORKSPACE" && pwd)"

say "workspace: $WORKSPACE"
say "suite:     $SUITE_ROOT"

# A workspace that already has a manifest or a store is being updated, not set
# up. Setup leaves its launchers and skills as they are, so it ends by naming
# the ones that differ from this suite's copies and handing the rest of the
# update to SETUP_HANDOFF.md section 0, where an agent can finish it.
UPDATING=0
if [ -e "$WORKSPACE/.cortex/index-sources.json" ] || [ -e "$WORKSPACE/.cortex/memory.db" ]; then
  UPDATING=1
fi
KEPT=""

# Only the build needs a toolchain. --skip-build exists precisely for a machine
# that has the binaries already (a shared checkout, a second workspace) and it
# used to die here anyway, on a requirement it was not about to use.
if [ "$SKIP_BUILD" -eq 0 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs and reopen the shell (or pass --skip-build if the binaries are already built)."
    say "rust: $(rustc --version)"
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
  # Running servers are left running. On macOS and Linux a build replaces a
  # binary in use without disturbing the copy that is running, and a cortex or
  # quartz-ctx server waiting for a request moves onto the new build by itself,
  # on the same connection, and tells its host the tool list changed. Setup
  # used to stop every server here first, which threw that away: the host
  # started them again by itself, and a session keeps the tool list it had
  # for a server started that way. A server too old to move itself is listed
  # by `reload-servers --dry-run` (SETUP_HANDOFF.md section 0).
  say "building cortex (debug)..."
  ( cd "$SUITE_ROOT/cortex" && cargo build ) || die "cortex build failed"
  say "building quartz-ctx (release)..."
  ( cd "$SUITE_ROOT/quartz-ctx" && cargo build --release ) || die "quartz-ctx build failed"
fi

CORTEX_EXE="$SUITE_ROOT/cortex/target/debug/cortex"
QCTX_EXE="$SUITE_ROOT/quartz-ctx/target/release/quartz-ctx"
[ -x "$CORTEX_EXE" ] || die "expected binary missing: $CORTEX_EXE"
[ -x "$QCTX_EXE" ]   || die "expected binary missing: $QCTX_EXE"

# Ask the artifact its version: a failed build leaves the previous binary behind.
say "cortex:     $("$CORTEX_EXE" --version 2>&1 | head -1)"
say "quartz-ctx: $("$QCTX_EXE" --version 2>&1 | head -1)"

# Path of $1 relative to $2, in POSIX shell only.
#
# This used to shell out to python3, which contradicted the promise that the
# suite needs nothing but Rust — and failed outright on a machine without it,
# silently falling back to an absolute path. Both forms work in an MCP config,
# so prefer the readable one when the suite sits under the workspace and use
# absolute otherwise.
relpath() {
  target="$1"
  base="${2%/}/"
  case "$target" in
    "$base"*) printf '%s' "${target#"$base"}" ;;
    *)        printf '%s' "$target" ;;
  esac
}
CORTEX_REL="$(relpath "$CORTEX_EXE" "$WORKSPACE")"
QCTX_REL="$(relpath "$QCTX_EXE" "$WORKSPACE")"
NAME="$(basename "$WORKSPACE")"

mkdir -p "$WORKSPACE/.cortex" "$WORKSPACE/.vscode" "$WORKSPACE/.github"

write_if_absent() {
  local path="$1" label="$2" content="$3"
  if [ -e "$path" ] && [ "$FORCE" -eq 0 ]; then
    warn "$label exists, leaving it alone (re-run with --force to overwrite)"
    return
  fi
  printf '%s\n' "$content" > "$path"
  say "wrote $label"
}

# Where the suite lives, recorded for the launcher.
#
# The launcher can probe for the suite at the workspace root or in a
# cortex_suite/ subdirectory, but not for the layout this script most often
# produces: a suite cloned somewhere of its own, pointed at a workspace
# elsewhere. Nothing in the workspace recorded that, so `.cortex/cortex.sh
# reindex` read its manifest through a binary it could not find, indexed
# nothing, and reported success.
#
# Always rewritten, never skipped for --force: it describes THIS machine's
# layout, so a stale copy from a moved or re-cloned suite is worse than none.
printf '# Written by setup.sh — where the cortex suite lives on this machine.\n# Delete this file if the suite moves and re-run setup.\nCORTEX_SUITE="%s"\n' \
    "$SUITE_ROOT" > "$WORKSPACE/.cortex/suite.env"
say "wrote .cortex/suite.env (suite at $SUITE_ROOT)"

if [ ! -e "$WORKSPACE/.cortex/index-sources.json" ] || [ "$FORCE" -eq 1 ]; then
  cp "$SUITE_ROOT/templates/index-sources.json" "$WORKSPACE/.cortex/index-sources.json"
  say "wrote .cortex/index-sources.json  <-- EDIT THIS: list your crates"
else
  warn "index-sources.json exists, leaving it alone"
fi

write_if_absent "$WORKSPACE/.mcp.json" ".mcp.json (Claude Code)" "$(cat <<EOF
{
  "mcpServers": {
    "cortex": {
      "command": "$CORTEX_REL",
      "args": ["--db", ".cortex/memory.db", "serve", "--repo", ".", "--name", "$NAME"]
    },
    "quartz-ctx": {
      "command": "$QCTX_REL",
      "args": ["serve", "--sources-from", ".cortex/index-sources.json", "--name", "$NAME"]
    }
  }
}
EOF
)"

write_if_absent "$WORKSPACE/.vscode/mcp.json" ".vscode/mcp.json (VS Code)" "$(cat <<EOF
{
  "servers": {
    "cortex": {
      "type": "stdio",
      "command": "$CORTEX_REL",
      "args": ["--db", ".cortex/memory.db", "serve", "--repo", ".", "--name", "$NAME"],
      "description": "Project memory: patterns, anti-patterns, decisions, code index."
    },
    "quartz-ctx": {
      "type": "stdio",
      "command": "$QCTX_REL",
      "args": ["serve", "--sources-from", ".cortex/index-sources.json", "--name", "$NAME"],
      "description": "API ground truth parsed live from source. Start coding tasks with get_api_context(hint)."
    }
  },
  "inputs": []
}
EOF
)"

# Both launchers, regardless of platform: a mixed team shares one workspace,
# and the Windows developer needs cortex.ps1 in the same checkout the macOS
# developer gets cortex.sh from.
# The frontier skill (auditing a limit before accepting it) goes to the path
# each host reads: .claude/skills for Claude Code, .github/prompts for Copilot.
for pair in "templates/cortex.sh:.cortex/cortex.sh" "templates/cortex.ps1:.cortex/cortex.ps1" "templates/skills/frontier/SKILL.md:.claude/skills/frontier/SKILL.md" "templates/skills/frontier.prompt.md:.github/prompts/frontier.prompt.md"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  if [ -e "$WORKSPACE/$dst" ] && [ "$FORCE" -eq 0 ]; then
    warn "$dst exists, leaving it alone"
    cmp -s "$SUITE_ROOT/$src" "$WORKSPACE/$dst" || KEPT="$KEPT $dst"
  else
    mkdir -p "$(dirname "$WORKSPACE/$dst")"
    cp "$SUITE_ROOT/$src" "$WORKSPACE/$dst"
    case "$dst" in *.sh) chmod +x "$WORKSPACE/$dst" ;; esac
    say "wrote $dst"
  fi
done

# The agent instructions. Only the cortex_suite section of each file is ours: a
# missing file is written from the template, an existing one gains the section
# or has it brought up to date, and nothing else in it changes. This used to
# copy the templates only where no file existed - so anyone with instructions
# of their own got none of ours - and to overwrite the whole file under
# --force. Updating later is the same command: ./.cortex/cortex.sh instructions
INSTR_FORCE=""
[ "$FORCE" -eq 1 ] && INSTR_FORCE="--force"
"$CORTEX_EXE" instructions --root "$WORKSPACE" --name "$NAME" $INSTR_FORCE \
  | while IFS= read -r line; do say "$line"; done \
  || die "cortex could not update the instruction files (built before 'instructions' existed? re-run without --skip-build)"

if [ "$UPDATING" -eq 1 ]; then
  say ""
  say "UPDATE: this workspace already had cortex_suite; setup brought the cortex_suite"
  say "section of its instruction files up to date (reported above)."
  if [ -n "$KEPT" ]; then
    say "These differ from the copies this suite ships, and setup left them as they were:"
    for f in $KEPT; do say "  $f"; done
  fi
  say ""
  say "Finish the update with the checklist in section 0 of"
  say "  $SUITE_ROOT/SETUP_HANDOFF.md"
  say "It covers what setup does not: servers too old to move onto the new build,"
  say "launchers and skills older than this suite's, VS Code's hooks, and older"
  say "cortex guidance kept in instruction files, .cortex/prefs.toml and the store,"
  say "then checks the result. If an agent is running this setup, it should work"
  say "through that section now."
  say ""
  say "Setup complete."
  exit 0
fi

say ""
say "NEXT: edit .cortex/index-sources.json to list your projects, then run:"
say "  ./.cortex/cortex.sh reindex        # or .\.cortex\cortex.ps1 reindex on Windows"
say "  ./.cortex/cortex.sh check-mcp      # confirms both MCP configs agree"
say ""
say "  For a Rust library, point a target at its 'src'. For anything with more"
say "  than one language, point at the APPLICATION directory instead — a web app"
say "  is ONE root covering both its backend and its frontend. Rooting at"
say "  'app/frontend/src' indexes the callers and not the routes they call, and"
say "  every call then reports as 'no matching route'."
say ""
say "  Check it with:  $QCTX_REL boundaries --source ."
say "  A long 'calls with no matching route' list usually means a missing root."
say ""
say "Or index a single crate directly:"
say "  $CORTEX_REL --db .cortex/memory.db index --source <crate>/src --name <Name>"
say ""
say "Then restart your editor so it picks up the MCP config."
say "Verify with:  $CORTEX_REL --db .cortex/memory.db doctor"
say ""
say "Setup complete."
