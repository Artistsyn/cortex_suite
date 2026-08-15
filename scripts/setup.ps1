#Requires -Version 5.1
<#
.SYNOPSIS
    Build and wire the cortex + quartz-ctx MCP suite into a workspace (Windows).

.DESCRIPTION
    Builds both binaries, writes the MCP configs and the source manifest, then
    runs a first index. Safe to re-run: it never overwrites an existing config
    without -Force.

.EXAMPLE
    .\setup.ps1 -Workspace C:\code\my-workspace
    .\setup.ps1 -Workspace . -Force
#>
param(
    # Root of the project you want indexed. Configs are written here.
    [Parameter(Mandatory = $true)][string] $Workspace,
    # Overwrite configs that already exist.
    [switch] $Force,
    # Skip cargo build (binaries already built).
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$SuiteRoot = Split-Path -Parent $PSScriptRoot

# Run a native command without PowerShell treating its stderr as failure.
#
# With $ErrorActionPreference = 'Stop', ANY stderr output from a native exe is
# raised as a terminating NativeCommandError. cargo writes every "Compiling ..."
# line to stderr, so a completely successful build aborted this script on the
# first crate. Exit code is the only trustworthy signal for a native process.
function Invoke-Native {
    param([scriptblock] $Command)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Command 2>&1 | ForEach-Object { "$_" } }
    finally { $ErrorActionPreference = $prev }
}

function Say([string] $m)  { Write-Host "[setup] $m" }
function Warn([string] $m) { Write-Host "[setup] WARN: $m" -ForegroundColor Yellow }
function Die([string] $m)  { Write-Host "[setup] ERROR: $m" -ForegroundColor Red; exit 1 }

# ── Preflight ────────────────────────────────────────────────────────────────
if (-not (Test-Path $Workspace)) { Die "workspace not found: $Workspace" }
$Workspace = (Resolve-Path $Workspace).Path
Say "workspace: $Workspace"
Say "suite:     $SuiteRoot"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Die "cargo not found. Install Rust from https://rustup.rs and reopen the terminal."
}
Say ("rust: " + (rustc --version))

# ── Stop running servers ─────────────────────────────────────────────────────
# A running MCP server holds an open handle on its own binary, so cargo cannot
# replace it. The build FAILS with 'Access is denied (os error 5)' and leaves the
# OLD binary in place - which looks like a build that succeeded but changed
# nothing. Always stop first.
$running = Get-Process cortex, quartz-ctx -ErrorAction SilentlyContinue
if ($running) {
    Say "stopping $($running.Count) running server process(es) so the binaries can be replaced"
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 700
}

# ── Build ────────────────────────────────────────────────────────────────────
if (-not $SkipBuild) {
    Say "building cortex (debug)... first run compiles dependencies and can take a few minutes"
    Push-Location (Join-Path $SuiteRoot 'cortex')
    Invoke-Native { cargo build } | Select-Object -Last 3 | ForEach-Object { Say "  $_" }
    if ($LASTEXITCODE -ne 0) { Pop-Location; Die "cortex build failed - see the output above" }
    Pop-Location

    # Release, because the MCP configs point at target/release for quartz-ctx.
    # It also compiles the tree-sitter grammars from C, so a missing C toolchain
    # surfaces here rather than as a confusing runtime gap.
    Say "building quartz-ctx (release)... compiles tree-sitter grammars, needs a C toolchain"
    Push-Location (Join-Path $SuiteRoot 'quartz-ctx')
    Invoke-Native { cargo build --release } | Select-Object -Last 3 | ForEach-Object { Say "  $_" }
    if ($LASTEXITCODE -ne 0) {
        Pop-Location
        Die "quartz-ctx build failed. If the error mentions a C compiler, cc or link.exe, install the Visual Studio Build Tools with the C++ workload and reopen the terminal."
    }
    Pop-Location
}

$CortexExe = Join-Path $SuiteRoot 'cortex\target\debug\cortex.exe'
$QctxExe   = Join-Path $SuiteRoot 'quartz-ctx\target\release\quartz-ctx.exe'
foreach ($exe in @($CortexExe, $QctxExe)) {
    if (-not (Test-Path $exe)) { Die "expected binary missing: $exe" }
}
# Ask the artifact its version rather than trusting the exit code: a failed build
# leaves the previous binary in place and reports success upstream.
Say ("cortex:     " + (Invoke-Native { & $CortexExe --version } | Select-Object -First 1))
Say ("quartz-ctx: " + (Invoke-Native { & $QctxExe --version } | Select-Object -First 1))

# ── Paths, relative to the workspace, forward slashes ────────────────────────
#
# [System.IO.Path]::GetRelativePath is .NET Core / .NET 5+ only. Windows
# PowerShell 5.1 runs on .NET Framework 4.x, where that method does not exist —
# it fails with "does not contain a method named 'GetRelativePath'" on every
# stock Windows machine. Uri.MakeRelativeUri is available on both.
function RelPath([string] $target) {
    $fromUri = New-Object System.Uri (($Workspace.TrimEnd('\','/')) + [System.IO.Path]::DirectorySeparatorChar)
    $toUri   = New-Object System.Uri $target
    if ($fromUri.Scheme -ne $toUri.Scheme) {
        # Different volumes have no relative form; an absolute path still works.
        return $target.Replace('\', '/')
    }
    $rel = [System.Uri]::UnescapeDataString($fromUri.MakeRelativeUri($toUri).ToString()).Replace('\', '/')

    # A relative path is nicer when the suite sits near the workspace, and
    # actively worse when it does not: cloning the suite elsewhere produced
    # ../../../../../../../../../ which is correct, unreadable, and breaks the
    # moment either directory moves. Past a few hops, absolute is the safer form.
    if (($rel -split '\.\./').Count - 1 -gt 3) {
        return $target.Replace('\', '/')
    }
    return $rel
}
$cortexRel = RelPath $CortexExe
$qctxRel   = RelPath $QctxExe

# ── Write configs ────────────────────────────────────────────────────────────
New-Item -ItemType Directory -Force -Path (Join-Path $Workspace '.cortex')  | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $Workspace '.vscode') | Out-Null

function WriteIfAbsent([string] $path, [string] $content, [string] $label) {
    if ((Test-Path $path) -and -not $Force) {
        Warn "$label exists, leaving it alone (re-run with -Force to overwrite): $path"
        return
    }
    # UTF8 without BOM: JSON parsers in some hosts choke on a BOM.
    [System.IO.File]::WriteAllText($path, $content, (New-Object System.Text.UTF8Encoding $false))
    Say "wrote $label"
}

$manifest = Join-Path $Workspace '.cortex\index-sources.json'
if (-not (Test-Path $manifest) -or $Force) {
    Copy-Item (Join-Path $SuiteRoot 'templates\index-sources.json') $manifest -Force
    Say "wrote .cortex/index-sources.json  <-- EDIT THIS: list your crates"
} else {
    Warn "index-sources.json exists, leaving it alone"
}

$mcpClaude = @"
{
  "mcpServers": {
    "cortex": {
      "command": "$cortexRel",
      "args": ["--db", ".cortex/memory.db", "serve", "--repo", ".", "--name", "$(Split-Path $Workspace -Leaf)"]
    },
    "quartz-ctx": {
      "command": "$qctxRel",
      "args": ["serve", "--sources-from", ".cortex/index-sources.json", "--name", "$(Split-Path $Workspace -Leaf)"]
    }
  }
}
"@
WriteIfAbsent (Join-Path $Workspace '.mcp.json') $mcpClaude '.mcp.json (Claude Code)'

$mcpVscode = @"
{
  "servers": {
    "cortex": {
      "type": "stdio",
      "command": "$cortexRel",
      "args": ["--db", ".cortex/memory.db", "serve", "--repo", ".", "--name", "$(Split-Path $Workspace -Leaf)"],
      "description": "Project memory: patterns, anti-patterns, decisions, code index."
    },
    "quartz-ctx": {
      "type": "stdio",
      "command": "$qctxRel",
      "args": ["serve", "--sources-from", ".cortex/index-sources.json", "--name", "$(Split-Path $Workspace -Leaf)"],
      "description": "API ground truth parsed live from source. Start coding tasks with get_api_context(hint)."
    }
  },
  "inputs": []
}
"@
WriteIfAbsent (Join-Path $Workspace '.vscode\mcp.json') $mcpVscode '.vscode/mcp.json (VS Code)'

foreach ($doc in @(
    @{ src = 'templates\CLAUDE.md';               dst = 'CLAUDE.md';                            label = 'CLAUDE.md' },
    @{ src = 'templates\copilot-instructions.md'; dst = '.github\copilot-instructions.md';      label = 'copilot-instructions.md' },
    # Both launchers regardless of platform: a mixed team shares one workspace,
    # so the macOS developer needs cortex.sh from the same checkout.
    @{ src = 'templates\cortex.ps1';             dst = '.cortex\cortex.ps1';                   label = 'cortex.ps1' },
    @{ src = 'templates\cortex.sh';              dst = '.cortex\cortex.sh';                    label = 'cortex.sh' }
)) {
    $dstPath = Join-Path $Workspace $doc.dst
    New-Item -ItemType Directory -Force -Path (Split-Path $dstPath -Parent) | Out-Null
    if ((Test-Path $dstPath) -and -not $Force) {
        Warn "$($doc.label) exists, leaving it alone"
    } else {
        Copy-Item (Join-Path $SuiteRoot $doc.src) $dstPath -Force
        Say "wrote $($doc.label)"
    }
}

# ── First index ──────────────────────────────────────────────────────────────
Say ""
Say "NEXT: edit .cortex/index-sources.json to list your projects, then run:"
Say "  .\.cortex\cortex.ps1 reindex"
Say "  .\.cortex\cortex.ps1 check-mcp      # confirms both MCP configs agree"
Say ""
Say "  For a Rust library, point a target at its 'src'. For anything with more"
Say "  than one language, point at the APPLICATION directory instead - a web app"
Say "  is ONE root covering both its backend and its frontend. Rooting at"
Say "  'app/frontend/src' indexes the callers and not the routes they call, and"
Say "  every call then reports as 'no matching route'."
Say ""
Say "  Check it with:  $qctxRel boundaries --source ."
Say "  A long 'calls with no matching route' list usually means a missing root."
Say ""
Say "Then restart your editor so it picks up the MCP config."
Say "Verify with:  $cortexRel --db .cortex/memory.db doctor"
Say ""
Say "Setup complete."
