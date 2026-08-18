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
QCTX_OUT=".cortex/apigraph"

# ── Locate the suite source ──────────────────────────────────────────────────
# The two crates sit either directly at the workspace root -- the layout the
# reference workspace grew up with -- or inside a cortex_suite checkout, which
# is what the Quickstart's `git clone <this-repo> cortex_suite` actually gives
# you. Hardcoding the first meant every command that shells out to cargo failed
# with "manifest path `cortex/Cargo.toml` does not exist" for anyone who
# followed the README, while setup.sh had already written a working .mcp.json
# pointing at cortex_suite/ -- so the MCP servers ran and the launcher did not.
#
# There is a THIRD layout, and it is the one the Quickstart actually describes:
# the suite cloned somewhere of its own and pointed at a workspace elsewhere
# (`./setup.sh ~/code/my-project`). Neither probe below can find it -- nothing
# in the workspace records where it went -- so every cargo-shelling command
# failed, and worse, `reindex` read its target list through the binary it could
# not find, looped zero times, and printed "done. indexed configured sources".
# A no-op that reports success is the failure this project keeps meeting.
#
# So setup writes the path it used into .cortex/suite.env, and that is read
# here. CORTEX_SUITE in the environment still wins, for any layout nobody has
# thought of yet.
if [ -z "${CORTEX_SUITE:-}" ] && [ -f ".cortex/suite.env" ]; then
    # shellcheck disable=SC1091
    . ".cortex/suite.env"
fi
if [ -n "${CORTEX_SUITE:-}" ]; then
    SUITE="${CORTEX_SUITE%/}/"
elif [ -f "cortex/Cargo.toml" ]; then
    SUITE=""
elif [ -f "cortex_suite/cortex/Cargo.toml" ]; then
    SUITE="cortex_suite/"
else
    SUITE=""   # fall through; the missing-manifest error below is the clear one
fi
CARGO="${SUITE}cortex/Cargo.toml"

# Windows shells (Git Bash / MSYS) still want the .exe suffix.
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
    *)                    EXE=""     ;;
esac
BINARY="${SUITE}cortex/target/debug/cortex${EXE}"
QCTX_BINARY="${SUITE}quartz-ctx/target/release/quartz-ctx${EXE}"

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

# The first root that actually exists, not simply the first listed.
#
# `cortex serve` takes ONE --source, so handing it a path that is not on this
# machine indexes nothing. (quartz-ctx used to make this fatal as well, exiting
# before the MCP handshake when its first root was missing; it now skips missing
# roots and starts either way, so manifest order no longer decides whether a
# server comes up. It still decides what cortex indexes here.)
primary_source() {
    # The loop runs in a subshell because of the pipe, so it cannot `return`
    # out of the function -- capture its output instead and decide here.
    _existing="$(manifest_targets | cut -f1 | while IFS= read -r src; do
        if [ -e "$src" ]; then printf '%s' "$src"; break; fi
    done)"
    if [ -n "$_existing" ]; then
        printf '%s' "$_existing"
    else
        # Nothing on disk: fall back to the first listed so the error names a
        # real configured path rather than an empty string.
        manifest_targets | head -1 | cut -f1
    fi
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

    # Count what was read and what was indexed, and report on both.
    #
    # "done. indexed configured sources" used to print unconditionally, so a
    # run that indexed NOTHING looked exactly like a run that indexed
    # everything. Both ways to get there are ordinary on a new machine: a
    # manifest of roots that are not checked out here, and a launcher that
    # cannot find the binary it reads the manifest with (see suite.env above) --
    # the second yields zero targets and used to be completely silent.
    TARGETS=0
    INDEXED=0

    # Process substitution rather than a temp file: no cleanup path to get
    # wrong, and no collision between two runs in the same second.
    while IFS="$(printf '	')" read -r SRC TNAME SCOPE; do
        [ -n "${SRC:-}" ] || continue
        TARGETS=$((TARGETS + 1))
        if [ ! -d "$SRC" ]; then
            say "WARN: skipping missing source path $SRC"
            continue
        fi
        INDEXED=$((INDEXED + 1))

        # quartz-ctx is the extractor: its api-graph carries full signatures that
        # cortex's own pass does not. Optional -- a missing binary degrades to
        # cortex extraction rather than failing the reindex.
        # The flag is --output, and the context directory is pinned rather than
        # guessed.
        #
        # It read --out, which the binary rejects with "unexpected argument",
        # and the whole call was wrapped in >/dev/null 2>&1 -- so the extractor
        # that carries full signatures had never run through this launcher, on
        # any workspace, and every reindex silently fell back to cortex's weaker
        # pass while reporting success. The candidate path was guessed too:
        # without --context-dir the tree lands under the source's own directory
        # name, so "primary" never existed for an unscoped target and the graph
        # would have been missed even had the call worked.
        #
        # Failures are reported now. Optional means "degrades to cortex
        # extraction", not "fails invisibly".
        GRAPH=""
        if [ -x "$QCTX_BINARY" ]; then
            CTXDIR="${SCOPE:-primary}"
            if "$QCTX_BINARY" generate --source "$SRC" --name "${TNAME:-$NAME}" \
                   --output "$QCTX_OUT" --context-dir "$CTXDIR" --include-private >/dev/null; then
                CANDIDATE="$QCTX_OUT/docs/$CTXDIR/api-graph.json"
                if [ -f "$CANDIDATE" ]; then
                    GRAPH="$CANDIDATE"
                else
                    say "WARN: api-graph not written for $SRC - indexing without it"
                fi
            else
                say "WARN: quartz-ctx generate failed for $SRC - indexing without api-graph"
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

    if [ "$TARGETS" -eq 0 ]; then
        say "read 0 targets from $INDEX_CONFIG - nothing was indexed."
        if [ ! -x "$BINARY" ] && [ ! -f "$CARGO" ]; then
            say "  the cortex binary and source are both unreachable from here."
            say "  expected: $BINARY"
            say "  fix: re-run the suite's setup script for this workspace, or set"
            say "       CORTEX_SUITE=/path/to/cortex_suite (or write it to .cortex/suite.env)"
        else
            say "  check that $INDEX_CONFIG has a non-empty \"targets\" list."
        fi
        exit 1
    fi

    if [ "$INDEXED" -eq 0 ]; then
        say "none of the $TARGETS configured source path(s) exist here - nothing was indexed."
        say "  edit $INDEX_CONFIG so its targets point at roots present in $ROOT"
        exit 1
    fi

    say "done. indexed $INDEXED of $TARGETS configured source(s) into $DB"
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

        # Does each command actually resolve to something runnable from here?
        #
        # This used to fail ANY absolute path, which was wrong in the one layout
        # the Quickstart recommends: a suite cloned outside the workspace can
        # only be named absolutely, and setup.sh writes exactly that. So the
        # checker called its own installer's output broken and told the user to
        # "resolve against the workspace root", which is impossible when the
        # binary is not under it -- a remediation that leaves you where it found
        # you.
        #
        # What actually breaks a host is a command that does not resolve: a
        # relative path against a cwd the host chose differently, a bare name
        # that is only on an interactive shell's PATH, or an absolute path from
        # somebody else's machine. Check for THAT, and treat portability as a
        # separate note rather than a failure.
        while IFS= read -r cmd; do
            [ -n "$cmd" ] || continue
            case "$cmd" in
                /*|[A-Za-z]:*)
                    if [ ! -x "$cmd" ]; then
                        say "BROKEN COMMAND in $f - absolute path does not exist here:"
                        say "    $cmd"
                        say "    (an absolute path is fine when the suite lives outside the"
                        say "     workspace, but it does not survive being copied to another"
                        say "     machine - re-run setup.sh there)"
                        problems=$((problems + 1))
                    fi
                    ;;
                */*)
                    if [ ! -x "$ROOT/$cmd" ]; then
                        say "BROKEN COMMAND in $f - not found relative to the workspace root:"
                        say "    $cmd  (looked in $ROOT)"
                        problems=$((problems + 1))
                    fi
                    ;;
                *)
                    # A bare name resolves through PATH -- and a GUI-launched
                    # editor inherits the launchd/systemd PATH, not the one your
                    # shell builds, so ~/.cargo/bin is typically absent there.
                    if ! command -v "$cmd" >/dev/null 2>&1; then
                        say "BROKEN COMMAND in $f - '$cmd' is not on PATH"
                        problems=$((problems + 1))
                    elif ! (env -i PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin" \
                            command -v "$cmd" >/dev/null 2>&1); then
                        say "NOTE: '$cmd' in $f resolves in this shell but not on a bare PATH."
                        say "    A GUI-launched editor will report it as not found."
                        say "    Fix: install or symlink it somewhere on the system PATH."
                    fi
                    ;;
            esac
        done <<EOF
$(grep -oE '"command"[[:space:]]*:[[:space:]]*"[^"]+"' "$f" | sed 's/.*: *//' | tr -d '"')
EOF
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
        say "mcp configs: every command resolves, and both hosts agree"
    else
        say "mcp configs: $problems problem(s)"
        exit 1
    fi
    ;;

doctor|selfcheck|status|recall|health-report|graph-diff|meta|prune|review|crystallize|adr|\
consolidate|correction|cluster-sessions|detect-skills|propose-gaps|propose-survival|\
consolidate-pipeline|consolidate-if-stale|review-proposals|skill-status|skill-approve|\
skill-reject|scoreboard|pattern|anti-pattern|prefs|annotate|context|graph|index|watch|\
fired)
    # `fired` is on this list because it was documented as a launcher command
    # and was not on it: an unlisted command falls through to the help text
    # below, which prints and exits 0. Asking "has this mechanism ever actually
    # run?" answered with a menu, successfully.
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
