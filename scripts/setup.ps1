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
    Say "building cortex (debug)..."
    Push-Location (Join-Path $SuiteRoot 'cortex')
    cargo build
    if ($LASTEXITCODE -ne 0) { Pop-Location; Die "cortex build failed" }
    Pop-Location

    Say "building quartz-ctx (release)..."
    Push-Location (Join-Path $SuiteRoot 'quartz-ctx')
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Pop-Location; Die "quartz-ctx build failed" }
    Pop-Location
}

$CortexExe = Join-Path $SuiteRoot 'cortex\target\debug\cortex.exe'
$QctxExe   = Join-Path $SuiteRoot 'quartz-ctx\target\release\quartz-ctx.exe'
foreach ($exe in @($CortexExe, $QctxExe)) {
    if (-not (Test-Path $exe)) { Die "expected binary missing: $exe" }
}
# Ask the artifact its version rather than trusting the exit code: a failed build
# leaves the previous binary in place and reports success upstream.
Say ("cortex:     " + (& $CortexExe --version 2>&1 | Select-Object -First 1))
Say ("quartz-ctx: " + (& $QctxExe --version 2>&1 | Select-Object -First 1))

# ── Paths, relative to the workspace, forward slashes ────────────────────────
function RelPath([string] $target) {
    $rel = [System.IO.Path]::GetRelativePath($Workspace, $target)
    return $rel.Replace('\', '/')
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
    @{ src = 'templates\copilot-instructions.md'; dst = '.github\copilot-instructions.md';      label = 'copilot-instructions.md' }
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
Say "NEXT: edit .cortex/index-sources.json to list your crates, then run:"
Say "  $cortexRel --db .cortex/memory.db index --source <crate>/src --name <Name>"
Say ""
Say "Then restart your editor so it picks up the MCP config."
Say "Verify with:  $cortexRel --db .cortex/memory.db doctor"
Say ""
Say "Setup complete."
