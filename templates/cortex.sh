#!/usr/bin/env bash
# cortex launcher for macOS and Linux.
#
# The companion to cortex.ps1. Same commands, same pinned --db, so a workspace
# behaves identically whichever platform its developer is on.
#
# Nothing here is specific to one machine or one workspace: the project name is
# derived from the directory you install into, and every path is relative to the
# repo root. Copy it to <workspace>/.cortex/cortex.sh and chmod +x.

set -uo pipefail

# ── Locate the workspace ─────────────────────────────────────────────────────
# The script lives in <repo>/.cortex/, so the repo root is its parent. Resolved
# through symlinks so a link into the repo does not change what gets indexed.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
cd "$ROOT" || exit 1

# Derived, never hardcoded: the workspace name is whatever the directory is
# called. Baking in one project's name is what made the old tagger produce
# garbage on every machine but its author's.
NAME="${CORTEX_NAME:-$(basename "$ROOT")}"

DB="${CORTEX_DB:-.cortex/memory.db}"
INDEX_CONFIG=".cortex/index-sources.json"
REPO="."
CARGO="cortex/Cargo.toml"
QCTX_OUT=".cortex/apigraph"

# Windows shells (Git Bash / MSYS) still want the .exe suffix.
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
    *)                    EXE=""     ;;
esac
BINARY="cortex/target/debug/cortex${EXE}"
QCTX_BINARY="quartz-ctx/target/release/quartz-ctx${EXE}"

say() { printf '[cortex] %s\n' "$*"; }
die() { say "$*"; exit 1; }

# The manifest's targets, as source<TAB>name<TAB>scope.
#
# Read by the cortex binary, not by python3. Shelling out to python for a JSON
# parse would make python a dependency of a suite that advertises none -- and it
# only fails on a machine that lacks it, which is never the author's. The binary
# is already required, so it costs nothing to ask it.
manifest_targets() {
    if [ -x "$BINARY" ]; then
        "$BINARY" manifest --repo "$ROOT" 2>/dev/null
    else
        cargo run --quiet --manifest-path "$CARGO" -- manifest --repo "$ROOT" 2>/dev/null
    fi
}

primary_source() {
    manifest_targets | head -1 | cut -f1
}

run_bin() {
    # Prefer the built binary; fall back to cargo run so a fresh clone works
    # before anyone has built anything.
    if [ -x "$BINARY" ]; then
        "$BINARY" --db "$DB" "$@"
    else
        cargo run --quiet --manifest-path "$CARGO" -- --db "$DB" "$@"
    fi
}

CMD="${1:-help}"
shift || true

case "$CMD" in

serve)
    SRC="$(primary_source)"
    [ -n "$SRC" ] || SRC="src"
    say "starting MCP server (name=$NAME, source=$SRC)"
    run_bin serve --source "$SRC" --repo "$REPO" --name "$NAME" "$@"
    ;;

deploy)
    # Rebuild without stopping the MCP server.
    #
    # On Unix this is already safe -- replacing a running executable leaves the
    # running process on its old inode -- so unlike the PowerShell version there
    # is nothing to park. Kept as its own command so the workflow is identical
    # on both platforms and nobody has to remember which one needs the dance.
    say "building..."
    cargo build --manifest-path "$CARGO" || die "build failed"
    [ -x "$BINARY" ] || die "build reported success but $BINARY is missing"
    say "deployed: $BINARY"
    say "the running server keeps its old image until it restarts."
    ;;

reindex)
    [ -f "$INDEX_CONFIG" ] || die "no $INDEX_CONFIG - nothing to index"

    # Process substitution rather than a temp file: no cleanup path to get
    # wrong, and no collision between two runs in the same second.
    while IFS="$(printf '	')" read -r SRC TNAME SCOPE; do
        [ -n "${SRC:-}" ] || continue
        if [ ! -d "$SRC" ]; then
            say "WARN: skipping missing source path $SRC"
            continue
        fi

        # quartz-ctx is the extractor: its api-graph carries full signatures that
        # cortex's own pass does not. Optional -- a missing binary degrades to
        # cortex extraction rather than failing the reindex.
        GRAPH=""
        if [ -x "$QCTX_BINARY" ]; then
            CTXDIR="${SCOPE:-primary}"
            if "$QCTX_BINARY" generate --source "$SRC" --name "${TNAME:-$NAME}"                  --out "$QCTX_OUT" --include-private >/dev/null 2>&1; then
                CANDIDATE="$QCTX_OUT/docs/$CTXDIR/api-graph.json"
                [ -f "$CANDIDATE" ] && GRAPH="$CANDIDATE"
            fi
        fi

        # Built as separate positional args rather than an array. macOS ships
        # bash 3.2, where `set -u` plus an EMPTY array expansion ("${arr[@]}")
        # aborts with "unbound variable" -- the same class of mistake as reaching
        # for GNU-only tools: it works on the author's shell and nowhere else.
        if [ -n "$SCOPE" ] && [ -n "$GRAPH" ]; then
            say "re-indexing $SRC (scope: $SCOPE, with api-graph)"
            run_bin index --source "$SRC" --name "${TNAME:-$NAME}" --scope "$SCOPE" --api-graph "$GRAPH"
        elif [ -n "$SCOPE" ]; then
            say "re-indexing $SRC (scope: $SCOPE)"
            run_bin index --source "$SRC" --name "${TNAME:-$NAME}" --scope "$SCOPE"
        elif [ -n "$GRAPH" ]; then
            say "re-indexing $SRC (unscoped, with api-graph)"
            run_bin index --source "$SRC" --name "${TNAME:-$NAME}" --api-graph "$GRAPH"
        else
            say "re-indexing $SRC (unscoped)"
            run_bin index --source "$SRC" --name "${TNAME:-$NAME}"
        fi
    done < <(manifest_targets)

    say "done. indexed configured sources into $DB"
    ;;

check-mcp)
    # Validate the two MCP configs before a user discovers the problem as a
    # server that silently never starts.
    #
    # Documenting "use relative paths" in the templates was not enough -- the
    # reference workspace still shipped an absolute command in one file and a
    # relative one in the other, which behaves perfectly for the author and dies
    # for everyone who copies it. A check fires; a comment does not.
    problems=0
    for f in ".mcp.json" ".vscode/mcp.json"; do
        if [ ! -f "$f" ]; then
            say "MISSING: $f"
            problems=$((problems + 1))
            continue
        fi
        # An absolute path: a leading / or a drive letter.
        if grep -qE '"command"[[:space:]]*:[[:space:]]*"(/|[A-Za-z]:)' "$f"; then
            say "ABSOLUTE PATH in $f - resolve commands against the workspace root instead:"
            grep -nE '"command"[[:space:]]*:[[:space:]]*"(/|[A-Za-z]:)' "$f" | sed 's/^/    /'
            problems=$((problems + 1))
        fi
    done

    # Drift between the two: the same tool answering differently per editor is
    # the failure this catches, and it produces no error on its own.
    if [ -f ".mcp.json" ] && [ -f ".vscode/mcp.json" ]; then
        a=$(grep -oE '"command"[[:space:]]*:[[:space:]]*"[^"]+"' .mcp.json | sed 's/.*: *//' | tr -d '"' | sort)
        b=$(grep -oE '"command"[[:space:]]*:[[:space:]]*"[^"]+"' .vscode/mcp.json | sed 's/.*: *//' | tr -d '"' | sort)
        # Compare ignoring a trailing .exe, which legitimately differs per platform.
        a_n=$(printf '%s
' "$a" | sed 's/\.exe$//')
        b_n=$(printf '%s
' "$b" | sed 's/\.exe$//')
        if [ "$a_n" != "$b_n" ]; then
            say "DRIFT: the two configs name different commands."
            say "  .mcp.json:        $(printf '%s ' $a_n)"
            say "  .vscode/mcp.json: $(printf '%s ' $b_n)"
            problems=$((problems + 1))
        fi
    fi

    if [ "$problems" -eq 0 ]; then
        say "mcp configs: relative paths, and both hosts agree"
    else
        say "mcp configs: $problems problem(s)"
        exit 1
    fi
    ;;

doctor|selfcheck|status|recall|health-report|graph-diff|meta|prune|review|crystallize|adr|\
consolidate|correction|cluster-sessions|detect-skills|propose-gaps|propose-survival|\
consolidate-pipeline|consolidate-if-stale|review-proposals|skill-status|skill-approve|\
skill-reject|scoreboard|pattern|anti-pattern|prefs|annotate|context|graph|index|watch)
    run_bin "$CMD" "$@"
    ;;

--)
    # Passthrough: everything after -- goes to the binary verbatim.
    run_bin "$@"
    ;;

help|*)
    cat <<EOF
cortex launcher (bash) - workspace: $NAME
  db: $DB

  serve         start the MCP server
  deploy        cargo build, then report the artifact
  reindex       regenerate api-graphs and re-index every source in the manifest
  check-mcp     validate both MCP configs (relative paths, no drift)
  status        store summary            doctor        pipeline health
  recall <t>    look something up        health-report self-learning state
  skill-status  drafts awaiting review   skill-approve <name>
  -- <args>     pass anything straight through to the binary

  Overrides: CORTEX_NAME, CORTEX_DB
EOF
    ;;
esac
