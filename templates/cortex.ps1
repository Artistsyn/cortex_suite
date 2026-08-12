# cortex.ps1
# Unified launcher for a Cortex workspace (PowerShell).
#
# Nothing here is specific to one machine or one project: the workspace name is
# the directory it is installed into, and the indexed sources come from
# .cortex/index-sources.json. The bash companion is cortex.sh.
# Covers source roots in a single DB:
#   quartz/src               (official, unscoped units)
#   synful_quartz/quartz/src (experimental, scope: synful::)
#   path_forge/src           (PathForge, scope: path_forge::)
# DB: .cortex/memory.db
#
# Usage:
#   .\.cortex\cortex.ps1 serve             # Start MCP server (unified context)
#   .\.cortex\cortex.ps1 reindex           # Re-index all sources into memory.db
#   .\.cortex\cortex.ps1 doctor            # Run doctor preflight
#   .\.cortex\cortex.ps1 selfcheck         # Validate DB/source and run status+doctor
#   .\.cortex\cortex.ps1 mcp-ready         # Validate MCP baseline tools are advertised by the server
#   .\.cortex\cortex.ps1 smoke             # Validate baseline + extended MCP surface for recent tooling
#   .\.cortex\cortex.ps1 migrate-legacy    # Detect legacy DB markers and run in-place migration pathway
#   .\.cortex\cortex.ps1 status-lite       # Compact status summary
#   .\.cortex\cortex.ps1 doctor-lite       # Compact doctor summary
#   .\.cortex\cortex.ps1 recall <topic>    # Search memory for a topic (CLI)
#   .\.cortex\cortex.ps1 git-review        # Check git diff for pattern relevance
#   .\.cortex\cortex.ps1 index-self        # Re-index cortex/src itself (scope: cortex)
#   .\.cortex\cortex.ps1 status            # Show DB stats
#   .\.cortex\cortex.ps1 post-session      # AFTER EVERY SESSION: git-review + pending observations + annotation reminder
#   .\.cortex\cortex.ps1 quality-check     # Audit: 0-use patterns, survival flags, anti-pattern count
#   .\.cortex\cortex.ps1 health-report      # One-line system health: patterns, gaps, orphans, proposals
#   .\.cortex\cortex.ps1 consolidate-if-stale [-StalenessHours N]  # Run pipeline if > N hours stale (default: 8)
#   .\.cortex\cortex.ps1 cluster-sessions   # Cluster session snapshots by tool-sequence TF-IDF similarity
#   .\.cortex\cortex.ps1 detect-skills      # Draft SKILL.md files for repeated tool sequences
#   .\.cortex\cortex.ps1 propose-gaps       # Propose prefs notes for hot query gaps (>=3 misses)
#   .\.cortex\cortex.ps1 propose-survival   # Flag dying patterns (survival<40%, use>=3) for review
#   .\.cortex\cortex.ps1 review-proposals   # Show pending cross-session proposals (approve/reject via CLI)
#   .\.cortex\cortex.ps1 skill-status       # List detected skill candidates with confidence scores
#   .\.cortex\cortex.ps1 skill-approve <n>  # Mark a skill candidate as approved
#   .\.cortex\cortex.ps1 skill-reject <n>   # Mark a skill candidate as rejected
#   .\.cortex\cortex.ps1 session-orphans    # List sessions without a closeout record
#   .\.cortex\cortex.ps1 graph-diff         # Compare current graph.json vs latest snapshot (drift report)
#   .\.cortex\cortex.ps1 meta report        # Meta-analysis: rejection rates, fidelity trends, gap evolution
#   .\.cortex\cortex.ps1 meta propose       # Stage meta-proposals based on analysis findings
#   .\.cortex\cortex.ps1 meta apply <id>    # Apply an approved meta-proposal to its target file
#   .\.cortex\cortex.ps1 meta dry-run <id>  # Preview what meta apply would change
#   .\.cortex\cortex.ps1 init              # RE-SEED: force prefs.toml + workflow anti-patterns + MCP annotations (auto on first run)
#   .\.cortex\cortex.ps1 setup-mcp         # Create or repair .vscode/mcp.json Cortex entry (direct cortex.exe)
#   .\.cortex\cortex.ps1 sync-continue-mcp # Sync .vscode/mcp.json servers into ~/.continue/config.yaml mcpServers
#   .\.cortex\cortex.ps1 -- <args>         # Pass any cortex args directly
#
# Formatting controls for selfcheck/status-lite/doctor-lite:
#   -SelfCheckFormat text|line|json   (default: text)
#   -Quiet                            (suppress cargo output)
#
# Source scoping:
#   Official quartz units use unscoped IDs:  plugin::terrain_collision::TerrainCollisionPlugin
#   Synful quartz units use scoped IDs:      synful::plugin::terrain_collision::TerrainCollisionPlugin
#
# If the cortex binary becomes locked, use: .\.cortex\cortex-reset.ps1 -Aggressive -rebuild

param(
    [Parameter(Position=0)]
    [string]$Command = "serve",

    [Parameter(Position=1, ValueFromRemainingArguments=$true)]
    [string[]]$Rest,

    [ValidateSet("text", "line", "json")]
    [string]$SelfCheckFormat = "text",

    [switch]$Quiet
)

Push-Location "$PSScriptRoot\.."

$DB            = ".cortex\memory.db"
# First target in index-sources.json, so a workspace with no quartz/ still works.
$SOURCE        = $null  # resolved below, after $INDEX_TARGETS is read
# No per-project source constants. Sources come from index-sources.json; the
# launcher reports on however many a workspace has, rather than on two it was
# once written around.
$INDEX_CONFIG  = ".cortex\index-sources.json"
$REPO          = "."
# Derived, never hardcoded: the workspace name is whatever directory this is
# installed into. Baking in one project's name is how a launcher silently
# mislabels every other workspace that copies it.
$NAME          = if ($env:CORTEX_NAME) { $env:CORTEX_NAME } else { Split-Path -Leaf $REPO_ROOT }
$CARGO         = "cortex\Cargo.toml"
$BINARY        = "cortex\target\debug\cortex.exe"
# quartz-ctx is the extraction engine: its api-graph carries full method signatures
# with types, per-method docs and field docs that cortex's own parser discards.
# Optional - if the binary is absent, reindex falls back to cortex-only extraction.
$QCTX_BINARY   = "quartz-ctx\target\release\quartz-ctx.exe"
$QCTX_OUT      = ".cortex\apigraph"

function Write-Prefix {
    param([string]$Message)
    Write-Host "[cortex] $Message"
}

function Convert-ToSafeId {
    param([string]$Text)
    if (-not $Text) { return "source" }
    return (($Text -replace "[^A-Za-z0-9]", "_").Trim("_"))
}

function Get-DefaultIndexTargets {
    # Used only when .cortex/index-sources.json is absent, i.e. a brand-new
    # workspace. One conventional Rust source dir, unscoped -- listing another
    # project's crates here would index paths that do not exist and report a
    # wall of "skipping missing source" on a first run.
    #
    # Deliberately does NOT read $SOURCE: this runs before $SOURCE is resolved
    # from the manifest, so referencing it here yields a null target.
    $guess = if (Test-Path "src") { "src" } else { "." }
    return @(
        [pscustomobject]@{ source = $guess; name = $NAME; scope = $null }
    )
}

function Get-ConfiguredIndexTargets {
    $defaults = Get-DefaultIndexTargets
    if (-not (Test-Path $INDEX_CONFIG)) {
        return $defaults
    }

    try {
        $json = Get-Content -Raw -Path $INDEX_CONFIG | ConvertFrom-Json
    }
    catch {
        Write-Prefix "WARN: could not parse $INDEX_CONFIG; falling back to defaults"
        return $defaults
    }

    if (-not $json -or -not $json.targets) {
        Write-Prefix "WARN: $INDEX_CONFIG has no targets array; falling back to defaults"
        return $defaults
    }

    $targets = @()
    foreach ($t in $json.targets) {
        if (-not $t.source) { continue }
        $src = [string]$t.source
        $nm = if ($t.name) { [string]$t.name } else { "${NAME}_$(Convert-ToSafeId -Text $src)" }
        $sc = if ($null -ne $t.scope -and "" -ne [string]$t.scope) { [string]$t.scope } else { $null }
        $targets += [pscustomobject]@{ source = $src; name = $nm; scope = $sc }
    }

    if ($targets.Count -eq 0) {
        Write-Prefix "WARN: $INDEX_CONFIG resolved to zero targets; falling back to defaults"
        return $defaults
    }

    return $targets
}

function Get-ApiGraphForSource {
    <#
      Generate a quartz-ctx api-graph.json for one source and return its path.

      quartz-ctx extracts full method signatures with parameter and return types,
      per-method docs and field docs; cortex's own parser keeps only bare method
      names. Feeding the api-graph to `index` upgrades the indexed content for
      every public item.

      Returns $null when quartz-ctx is unavailable or generation fails, so a
      missing or broken extractor degrades to cortex-only extraction instead of
      failing the whole reindex.
    #>
    param(
        [Parameter(Mandatory = $true)][string] $Source,
        [Parameter(Mandatory = $true)][string] $Name,
        [string] $Scope
    )

    if (-not (Test-Path $QCTX_BINARY)) {
        Write-Prefix "  api-graph: quartz-ctx not built at $QCTX_BINARY - using cortex extraction only"
        return $null
    }

    # Key the context dir on scope: quartz\src and arty\synful_quartz\quartz\src
    # both slugify to "quartz", so without this they would overwrite each other.
    $ctxDir = if ($Scope) { $Scope } else { "primary" }

    try {
        # quartz-ctx writes its progress and copilot-instructions hint to stderr;
        # discard both. Redirect to $null rather than 2>&1 - in PS 5.1 merging a
        # native exe's stderr wraps every line in an ErrorRecord and sets $? false
        # even on a clean exit.
        & $QCTX_BINARY generate --source $Source --output $QCTX_OUT --context-dir $ctxDir --name $Name --minimal 2>$null | Out-Null
    }
    catch {
        Write-Prefix "  api-graph: generation threw for $Source - using cortex extraction only"
        return $null
    }

    if ($LASTEXITCODE -ne 0) {
        Write-Prefix "  api-graph: quartz-ctx exited $LASTEXITCODE for $Source - using cortex extraction only"
        return $null
    }

    $path = Join-Path $QCTX_OUT (Join-Path "docs" (Join-Path $ctxDir "api-graph.json"))
    if (-not (Test-Path $path)) {
        Write-Prefix "  api-graph: expected $path was not written - using cortex extraction only"
        return $null
    }

    return $path
}

function Add-ExtraIndexTargets {
    param(
        [object[]]$Targets,
        [string[]]$ExtraSources
    )

    $merged = @()
    if ($Targets) { $merged += $Targets }

    foreach ($srcRaw in $ExtraSources) {
        if (-not $srcRaw) { continue }
        if ($srcRaw.StartsWith("-")) { continue }
        $src = $srcRaw.Trim()
        if (-not $src) { continue }
        $exists = $false
        foreach ($t in $merged) {
            if ($t.source -eq $src) { $exists = $true; break }
        }
        if ($exists) { continue }

        $scope = Convert-ToSafeId -Text $src
        $name = "${NAME}_$(Convert-ToSafeId -Text $src)"
        $merged += [pscustomobject]@{ source = $src; name = $name; scope = $scope }
    }

    return $merged
}

function Set-McpConfig {
    $mcpPath = ".vscode\mcp.json"

    $cfg = $null
    if (Test-Path $mcpPath) {
        try {
            $cfg = Get-Content -Raw -Path $mcpPath | ConvertFrom-Json
        }
        catch {
            Write-Prefix "WARN: existing $mcpPath is invalid JSON; recreating a minimal config"
            $cfg = $null
        }
    }

    if (-not $cfg) {
        $cfg = [pscustomobject]@{}
    }
    if (-not $cfg.servers) {
        $cfg | Add-Member -NotePropertyName servers -NotePropertyValue ([pscustomobject]@{}) -Force
    }
    if (-not $cfg.inputs) {
        $cfg | Add-Member -NotePropertyName inputs -NotePropertyValue @() -Force
    }

    $cortexServer = [pscustomobject]@{
        type = "stdio"
        command = "cortex/target/debug/cortex.exe"
        args = @(
            "--db",
            ".cortex/memory.db",
            "serve",
            "--source",
            "quartz/src",
            "--repo",
            ".",
            "--name",
            $NAME
        )
        description = "$NAME Cortex MCP direct binary server. Reindex sources via .cortex/cortex.ps1 reindex (uses .cortex/index-sources.json)."
    }

    $cfg.servers | Add-Member -NotePropertyName cortex -NotePropertyValue $cortexServer -Force
    $json = $cfg | ConvertTo-Json -Depth 20
    Set-Content -Path $mcpPath -Value $json -Encoding UTF8
    Write-Prefix "Updated $mcpPath with a direct-binary Cortex MCP entry"
}

$INDEX_TARGETS = Get-ConfiguredIndexTargets
if ($INDEX_TARGETS -and $INDEX_TARGETS.Count -gt 0) {
    $SOURCE = [string]$INDEX_TARGETS[0].source
    if ($INDEX_TARGETS.Count -gt 1) {
    }
}

function Invoke-CargoCaptureWithRetry {
    param(
        [string[]]$BaseArgs,
        [string[]]$TailArgs = @(),
        [int]$MaxRetries = 3,
        [switch]$SuppressOutput
    )

    $attempt = 0
    while ($attempt -lt $MaxRetries) {
        try {
            $allArgs = @()
            $allArgs += $BaseArgs
            if ($TailArgs) { $allArgs += $TailArgs }

            # Prefer pre-built binary to avoid cargo build-lock contention.
            # cargo run holds a write lock on the .exe; calling the binary directly does not.
            $useBinary = (Test-Path $BINARY) -and ($allArgs.Count -gt 0) -and ($allArgs[0] -eq 'run')
            if ($useBinary) {
                $sepIdx = [Array]::IndexOf([string[]]$allArgs, '--')
                if ($sepIdx -ge 0 -and $sepIdx -lt ($allArgs.Count - 1)) {
                    $binArgs = $allArgs[($sepIdx + 1)..($allArgs.Count - 1)]
                    $output = (& $BINARY @binArgs 2>&1 | Out-String)
                } else {
                    $output = (& cargo @allArgs 2>&1 | Out-String)
                }
            } else {
                $output = (& cargo @allArgs 2>&1 | Out-String)
            }
            $exitCode = $LASTEXITCODE

            if ($output -and -not $SuppressOutput) {
                # Out-Host, NOT Write-Output.
                #
                # This function's caller assigns its result:
                #   $result = Invoke-CargoCaptureWithRetry ...
                # and a PowerShell function returns EVERYTHING it emits to the
                # success stream. Write-Output therefore did not print the
                # command's output — it appended it to the return value, where
                # the assignment swallowed it. Every subcommand routed through
                # here (skill-status, skill-approve, the -- passthrough) printed
                # nothing at all and still exited 0, which reads as success.
                # Out-Host writes to the console and stays out of the pipeline.
                $output.TrimEnd() | Out-Host
            }

            if ($exitCode -eq 0) {
                return [pscustomobject]@{
                    Ok = $true
                    ExitCode = $exitCode
                    Output = $output
                }
            }

            $isLockError = ($output -match "Access is denied") -or ($output -match "locked")
            if ($isLockError -and $attempt -lt ($MaxRetries - 1)) {
                # | Out-Host on both: this function's result is assigned by its
                # caller, so anything the reset script prints would otherwise be
                # appended to the RETURN VALUE rather than shown -- the same
                # fault that made skill-status silent. Here it would also
                # corrupt `.Output`, which Convert-CargoOutputToJson parses, so
                # a lock-retry could turn a valid JSON reply into an unparseable
                # mixture of reset chatter and payload.
                if ($attempt -eq 0) {
                    Write-Prefix "Binary lock detected, running soft reset..."
                    & "$PSScriptRoot\cortex-reset.ps1" -rebuild | Out-Host
                }
                else {
                    Write-Prefix "Lock persists, escalating to aggressive reset..."
                    & "$PSScriptRoot\cortex-reset.ps1" -rebuild -Aggressive | Out-Host
                }
                $attempt++
                continue
            }

            return [pscustomobject]@{
                Ok = $false
                ExitCode = $exitCode
                Output = $output
            }
        }
        catch {
            Write-Prefix "Error during cargo execution: $_"
            return [pscustomobject]@{
                Ok = $false
                ExitCode = 1
                Output = "$_"
            }
        }
    }

    return [pscustomobject]@{
        Ok = $false
        ExitCode = 1
        Output = "Exceeded max retry count"
    }
}

function Invoke-CargoWithRetry {
    param(
        [string[]]$BaseArgs,
        [string[]]$TailArgs = @(),
        [switch]$SuppressOutput
    )

    $result = Invoke-CargoCaptureWithRetry -BaseArgs $BaseArgs -TailArgs $TailArgs -SuppressOutput:$SuppressOutput
    return [bool]$result.Ok
}

function Invoke-OrExit {
    param(
        [string[]]$BaseArgs,
        [string[]]$TailArgs = @(),
        [switch]$SuppressOutput
    )

    $ok = Invoke-CargoWithRetry -BaseArgs $BaseArgs -TailArgs $TailArgs -SuppressOutput:$SuppressOutput
    if (-not $ok) {
        Pop-Location
        exit 1
    }
}

function Convert-CargoOutputToJson {
    param([string]$Text)

    if (-not $Text) {
        return $null
    }

    # Try whole-text parse first.
    try {
        return ($Text | ConvertFrom-Json)
    }
    catch {}

    # Then try line-by-line for tools that emit one JSON object per line.
    $lines = $Text -split "`r?`n"
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        if (-not $trimmed) { continue }
        if (-not (($trimmed.StartsWith("{")) -or ($trimmed.StartsWith("[")))) { continue }
        try {
            return ($trimmed | ConvertFrom-Json)
        }
        catch {}
    }

    # Fallback: prefer a JSON object that starts on a fresh line near the end.
    $start = $Text.LastIndexOf("`n{")
    if ($start -ge 0) {
        $start = $start + 1
    }
    else {
        $start = $Text.IndexOf("{")
    }
    $end = $Text.LastIndexOf("}")
    if ($start -lt 0 -or $end -lt $start) {
        return $null
    }

    $jsonText = $Text.Substring($start, $end - $start + 1)
    try {
        return ($jsonText | ConvertFrom-Json)
    }
    catch {
        return $null
    }
}

function Get-StatusCheck {
    $result = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "--format", "json", "status", "--full") -SuppressOutput:$Quiet
    $json = Convert-CargoOutputToJson -Text $result.Output

    $indexedUnits = 0
    if ($json -and $null -ne $json.indexed_units) {
        $indexedUnits = [int]$json.indexed_units
    }
    elseif ($json -and $json.index -and $json.index.metrics -and $null -ne $json.index.metrics.indexed_units) {
        $indexedUnits = [int]$json.index.metrics.indexed_units
    }

    return [pscustomobject]@{
        ok = [bool]$result.Ok
        indexed_units = $indexedUnits
        parsed = [bool]($null -ne $json)
    }
}

function Get-DoctorCheck {
    $result = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "--format", "json", "doctor", "workflow", "--repo", $REPO, "--source", $SOURCE, "--name", $NAME) -SuppressOutput:$Quiet
    $json = Convert-CargoOutputToJson -Text $result.Output

    $checksTotal = 0
    $checksPass = 0
    if ($json -and $json.checks) {
        $checksTotal = @($json.checks).Count
        $checksPass = @($json.checks | Where-Object { $_.pass -eq $true }).Count
    }

    return [pscustomobject]@{
        ok = [bool]$result.Ok
        doctor_ok = [bool]($json -and $json.ok -eq $true)
        checks_pass = $checksPass
        checks_total = $checksTotal
        parsed = [bool]($null -ne $json)
    }
}

function Write-SelfCheckResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor
    )

    if ($SelfCheckFormat -eq "json") {
        $payload = [pscustomobject]@{
            scope = "unified"
            pass = $Pass
            db = $DB
            source_official = $SOURCE
            sources = @($INDEX_TARGETS | ForEach-Object { $_.source })
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            timestamp = (Get-Date).ToString("s")
        }
        $payload | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        Write-Host ("CORTEX_SELFCHECK {0} scope=unified db={1} primary_source={2} sources={3} indexed_units={4} checks={5}/{6}" -f $resultText, $DB, $SOURCE, $INDEX_TARGETS.Count, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total)
        return
    }

    if ($Pass) {
        Write-Prefix "selfcheck: PASS"
    }
    else {
        Write-Prefix "selfcheck: FAIL"
    }
}

function Invoke-SelfCheck {
    $ok = $true

    if (-not (Test-Path $CARGO)) {
        Write-Prefix "FAIL: missing manifest path $CARGO"
        $ok = $false
    }
    if (-not (Test-Path $SOURCE)) {
        Write-Prefix "FAIL: missing source path $SOURCE"
        $ok = $false
    }
    # Every source the manifest lists, not one project's second crate. This
    # used to warn "synful source path not found" on every run in any workspace
    # that had no synful -- which is all of them but one, and reads to a new
    # user as a broken install.
    foreach ($t in $INDEX_TARGETS) {
        if (-not (Test-Path $t.source)) {
            Write-Prefix "WARN: configured source path not found: $($t.source)"
        }
    }
    if (-not (Test-Path $DB)) {
        Write-Prefix "WARN: DB file not found yet: $DB"
    }

    if (-not $ok) {
        return $false
    }

    if (-not $Quiet) {
        Write-Prefix "selfcheck: status"
    }
    $statusCheck = Get-StatusCheck

    if (-not $Quiet) {
        Write-Prefix "selfcheck: doctor"
    }
    $doctorCheck = Get-DoctorCheck

    $pass = $statusCheck.ok -and $doctorCheck.ok -and $doctorCheck.doctor_ok -and ($statusCheck.indexed_units -gt 0)
    Write-SelfCheckResult -Pass $pass -Status $statusCheck -Doctor $doctorCheck
    return $pass
}

function Invoke-StatusLite {
    $statusCheck = Get-StatusCheck
    if ($SelfCheckFormat -eq "json") {
        ([pscustomobject]@{
            scope = "unified"
            status_ok = $statusCheck.ok
            indexed_units = $statusCheck.indexed_units
            parsed = $statusCheck.parsed
            db = $DB
            source_official = $SOURCE
            sources = @($INDEX_TARGETS | ForEach-Object { $_.source })
        } | ConvertTo-Json -Compress) | Write-Host
        return $statusCheck.ok
    }

    if ($SelfCheckFormat -eq "line") {
        Write-Host ("CORTEX_STATUS scope=unified ok={0} indexed_units={1} db={2} primary_source={3} sources={4}" -f $statusCheck.ok.ToString().ToLowerInvariant(), $statusCheck.indexed_units, $DB, $SOURCE, $INDEX_TARGETS.Count)
        return $statusCheck.ok
    }

    Write-Prefix ("status-lite: ok={0} indexed_units={1}" -f $statusCheck.ok, $statusCheck.indexed_units)
    return $statusCheck.ok
}

function Invoke-DoctorLite {
    $doctorCheck = Get-DoctorCheck
    if ($SelfCheckFormat -eq "json") {
        ([pscustomobject]@{
            scope = "unified"
            doctor_ok = $doctorCheck.ok
            workflow_ok = $doctorCheck.doctor_ok
            checks_pass = $doctorCheck.checks_pass
            checks_total = $doctorCheck.checks_total
            parsed = $doctorCheck.parsed
            db = $DB
            source_official = $SOURCE
            sources = @($INDEX_TARGETS | ForEach-Object { $_.source })
        } | ConvertTo-Json -Compress) | Write-Host
        return ($doctorCheck.ok -and $doctorCheck.doctor_ok)
    }

    if ($SelfCheckFormat -eq "line") {
        Write-Host ("CORTEX_DOCTOR scope=unified ok={0} workflow_ok={1} checks={2}/{3} db={4} primary_source={5} sources={6}" -f $doctorCheck.ok.ToString().ToLowerInvariant(), $doctorCheck.doctor_ok.ToString().ToLowerInvariant(), $doctorCheck.checks_pass, $doctorCheck.checks_total, $DB, $SOURCE, $INDEX_TARGETS.Count)
        return ($doctorCheck.ok -and $doctorCheck.doctor_ok)
    }

    Write-Prefix ("doctor-lite: ok={0} workflow_ok={1} checks={2}/{3}" -f $doctorCheck.ok, $doctorCheck.doctor_ok, $doctorCheck.checks_pass, $doctorCheck.checks_total)
    return ($doctorCheck.ok -and $doctorCheck.doctor_ok)
}

function Read-JsonRpcResponse {
    param(
        [Parameter(Mandatory=$true)]
        [System.Diagnostics.Process]$Process,

        [Parameter(Mandatory=$true)]
        [int]$ExpectedId,

        [int]$TimeoutMs = 10000
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        $task = $Process.StandardOutput.ReadLineAsync()
        if (-not $task.Wait(500)) {
            continue
        }

        $line = $task.Result
        if ($null -eq $line) {
            break
        }

        $trim = $line.Trim()
        if (-not $trim) {
            continue
        }

        try {
            $json = $trim | ConvertFrom-Json
        }
        catch {
            continue
        }

        if ($null -ne $json.id -and [int]$json.id -eq $ExpectedId) {
            return $json
        }
    }

    return $null
}

function Invoke-LegacyMigrationPathway {
    param(
        [string]$TriggerCommand = ""
    )

    if (-not (Test-Path $DB)) {
        return $true
    }

    $result = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "--format", "json", "status", "--full") -SuppressOutput:$true
    if (-not $result.Ok) {
        Write-Prefix "WARN: legacy migration preflight failed for command '$TriggerCommand'."
        Write-Prefix "Run: .\\.cortex\\cortex.ps1 migrate-legacy"
        Write-Prefix "AI workflow prompt: PROTOCOL - CORTEX - migrate .cortex legacy DB and run smoke"
        return $false
    }

    $migrationLine = @($result.Output -split "`r?`n" | Where-Object { $_ -match "legacy outcome application markers detected" } | Select-Object -First 1)
    if ($migrationLine -and $migrationLine.Count -gt 0) {
        Write-Prefix $migrationLine[0].Trim()
        Write-Prefix "Legacy migration pathway applied automatically during startup."
        Write-Prefix "Recommended verification: .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json"
    }

    return $true
}

function Test-McpToolSurface {
    param(
        [string[]]$RequiredTools = @("get_delta", "get_preferences", "get_anti_patterns", "list_patterns", "get_context"),
        [int]$TimeoutMs = 12000
    )

    if (-not (Test-Path $BINARY)) {
        return [pscustomobject]@{
            ok = $false
            reason = "missing_binary"
            tools = @()
            tool_defs = @()
            missing_tools = $RequiredTools
        }
    }

    $proc = $null
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $BINARY
        $psi.Arguments = "--db $DB serve --source $SOURCE --repo $REPO --name $NAME"
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true

        $proc = New-Object System.Diagnostics.Process
        $proc.StartInfo = $psi
        $started = $proc.Start()
        if (-not $started) {
            return [pscustomobject]@{
                ok = $false
                reason = "process_start_failed"
                tools = @()
                tool_defs = @()
                missing_tools = $RequiredTools
            }
        }

        $initReq = @{
            jsonrpc = "2.0"
            id = 1
            method = "initialize"
            params = @{
                protocolVersion = "2024-11-05"
                capabilities = @{}
                clientInfo = @{ name = "cortex.ps1"; version = "1.0" }
            }
        } | ConvertTo-Json -Depth 8 -Compress

        $toolsReq = @{
            jsonrpc = "2.0"
            id = 2
            method = "tools/list"
            params = @{}
        } | ConvertTo-Json -Depth 8 -Compress

        $proc.StandardInput.WriteLine($initReq)
        $proc.StandardInput.Flush()
        $null = Read-JsonRpcResponse -Process $proc -ExpectedId 1 -TimeoutMs $TimeoutMs

        $proc.StandardInput.WriteLine($toolsReq)
        $proc.StandardInput.Flush()
        $toolsResp = Read-JsonRpcResponse -Process $proc -ExpectedId 2 -TimeoutMs $TimeoutMs

        if ($null -eq $toolsResp -or $null -eq $toolsResp.result -or $null -eq $toolsResp.result.tools) {
            return [pscustomobject]@{
                ok = $false
                reason = "tools_list_unavailable"
                tools = @()
                tool_defs = @()
                missing_tools = $RequiredTools
            }
        }

        $toolNames = @()
        foreach ($tool in $toolsResp.result.tools) {
            if ($tool.name) {
                $toolNames += [string]$tool.name
            }
        }

        $missing = @($RequiredTools | Where-Object { $toolNames -notcontains $_ })
        return [pscustomobject]@{
            ok = ($missing.Count -eq 0)
            reason = if ($missing.Count -eq 0) { "ok" } else { "missing_required_tools" }
            tools = $toolNames
            tool_defs = @($toolsResp.result.tools)
            missing_tools = $missing
        }
    }
    catch {
        return [pscustomobject]@{
            ok = $false
            reason = "exception"
            detail = [string]$_
            tools = @()
            tool_defs = @()
            missing_tools = $RequiredTools
        }
    }
    finally {
        if ($proc) {
            try {
                if (-not $proc.HasExited) {
                    $proc.Kill()
                }
            }
            catch {}
            $proc.Dispose()
        }
    }
}

function Write-McpReadyResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor,
        [object]$Probe
    )

    if ($SelfCheckFormat -eq "json") {
        $payload = [pscustomobject]@{
            scope = "unified"
            pass = $Pass
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            workflow_ok = $Doctor.doctor_ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            mcp_tools_ok = $Probe.ok
            mcp_reason = $Probe.reason
            missing_tools = $Probe.missing_tools
            tool_count = @($Probe.tools).Count
            timestamp = (Get-Date).ToString("s")
        }
        $payload | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        $missing = if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) { $Probe.missing_tools -join "," } else { "none" }
        Write-Host ("CORTEX_MCP_READY {0} status_ok={1} doctor_ok={2} workflow_ok={3} indexed_units={4} checks={5}/{6} mcp_tools_ok={7} missing_tools={8}" -f $resultText, $Status.ok, $Doctor.ok, $Doctor.doctor_ok, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total, $Probe.ok, $missing)
        return
    }

    if ($Pass) {
        Write-Prefix "mcp-ready: PASS"
        Write-Prefix "Baseline MCP tools are available."
    }
    else {
        Write-Prefix "mcp-ready: FAIL"
        if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) {
            Write-Prefix ("Missing required MCP tools: {0}" -f ($Probe.missing_tools -join ", "))
        }
        Write-Prefix "If mcp-ready passes but chat still lacks wrappers, the limitation is in chat tool exposure, not cortex server registration."
    }
}

function Invoke-McpReady {
    $statusCheck = Get-StatusCheck
    $doctorCheck = Get-DoctorCheck
    $probe = Test-McpToolSurface

    $healthOk = $statusCheck.ok -and $doctorCheck.ok -and $doctorCheck.doctor_ok -and ($statusCheck.indexed_units -gt 0)
    $pass = $healthOk -and $probe.ok

    Write-McpReadyResult -Pass $pass -Status $statusCheck -Doctor $doctorCheck -Probe $probe
    return $pass
}

function Test-ToolSchemaProperty {
    param(
        [object]$Probe,
        [string]$ToolName,
        [string]$PropertyName
    )

    if (-not $Probe -or -not $Probe.tool_defs) {
        return $false
    }

    $tool = @($Probe.tool_defs | Where-Object { $_.name -eq $ToolName } | Select-Object -First 1)
    if (-not $tool -or $tool.Count -eq 0) {
        return $false
    }

    $schema = $tool[0].inputSchema
    if (-not $schema -or -not $schema.properties) {
        return $false
    }

    $propertyNames = @($schema.properties.PSObject.Properties.Name)
    return ($propertyNames -contains $PropertyName)
}

function Write-SmokeResult {
    param(
        [bool]$Pass,
        [object]$Status,
        [object]$Doctor,
        [object]$Probe,
        [bool]$RelationFilterOk
    )

    if ($SelfCheckFormat -eq "json") {
        $payload = [pscustomobject]@{
            scope = "unified"
            pass = $Pass
            status_ok = $Status.ok
            doctor_ok = $Doctor.ok
            workflow_ok = $Doctor.doctor_ok
            indexed_units = $Status.indexed_units
            checks_pass = $Doctor.checks_pass
            checks_total = $Doctor.checks_total
            mcp_tools_ok = $Probe.ok
            relation_filter_ok = $RelationFilterOk
            missing_tools = $Probe.missing_tools
            tool_count = @($Probe.tools).Count
            timestamp = (Get-Date).ToString("s")
        }
        $payload | ConvertTo-Json -Compress | Write-Host
        return
    }

    if ($SelfCheckFormat -eq "line") {
        $resultText = if ($Pass) { "PASS" } else { "FAIL" }
        $missing = if ($Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) { $Probe.missing_tools -join "," } else { "none" }
        Write-Host ("CORTEX_SMOKE {0} status_ok={1} doctor_ok={2} workflow_ok={3} indexed_units={4} checks={5}/{6} mcp_tools_ok={7} relation_filter_ok={8} missing_tools={9}" -f $resultText, $Status.ok, $Doctor.ok, $Doctor.doctor_ok, $Status.indexed_units, $Doctor.checks_pass, $Doctor.checks_total, $Probe.ok, $RelationFilterOk, $missing)
        return
    }

    if ($Pass) {
        Write-Prefix "smoke: PASS"
        Write-Prefix "Baseline and extended MCP tooling checks passed."
    }
    else {
        Write-Prefix "smoke: FAIL"
        if (-not $Probe.ok -and $Probe.missing_tools -and $Probe.missing_tools.Count -gt 0) {
            Write-Prefix ("Missing required tools: {0}" -f ($Probe.missing_tools -join ", "))
        }
        if (-not $RelationFilterOk) {
            Write-Prefix "simulate_change schema missing relation_filter property"
        }
    }
}

function Invoke-Smoke {
    $requiredTools = @(
        "get_delta",
        "get_preferences",
        "get_anti_patterns",
        "list_patterns",
        "get_context",
        "get_usage_examples",
        "get_helper",
        "explain_dependency_path",
        "simulate_change"
    )

    $statusCheck = Get-StatusCheck
    $doctorCheck = Get-DoctorCheck
    $probe = Test-McpToolSurface -RequiredTools $requiredTools
    $relationFilterOk = Test-ToolSchemaProperty -Probe $probe -ToolName "simulate_change" -PropertyName "relation_filter"

    $healthOk = $statusCheck.ok -and $doctorCheck.ok -and $doctorCheck.doctor_ok -and ($statusCheck.indexed_units -gt 0)
    $pass = $healthOk -and $probe.ok -and $relationFilterOk

    Write-SmokeResult -Pass $pass -Status $statusCheck -Doctor $doctorCheck -Probe $probe -RelationFilterOk $relationFilterOk
    return $pass
}

$skipLegacyMigrationPreflight = @("setup-mcp", "migrate-legacy")
if ($skipLegacyMigrationPreflight -notcontains $Command) {
    $null = Invoke-LegacyMigrationPathway -TriggerCommand $Command
}

switch ($Command) {
    "serve" {
        Write-Prefix "Starting MCP server - unified context (indexed sources: $($INDEX_TARGETS.Count))"
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "serve", "--source", $SOURCE, "--repo", $REPO, "--name", $NAME) -TailArgs $Rest
    }
    "deploy" {
        # Rebuild without stopping the MCP server.
        #
        # Windows locks a running executable against deletion and writing, but
        # NOT against rename -- which is how self-updaters work. Renaming the
        # live binary out of the way frees the name for cargo, the running
        # process keeps its old image until it restarts naturally, and the next
        # restart picks up the new build.
        #
        # Verified on this machine: Remove-Item on the running cortex.exe is
        # blocked with Access Denied; Rename-Item succeeds.
        #
        # This replaces the old ritual of hunting cortex processes and killing
        # them before every build, which raced Claude Code's MCP auto-restart
        # and failed roughly half the time.
        $binDir = Split-Path $BINARY -Parent

        # Sweep previous swaps whose processes have since exited. Still-locked
        # ones simply stay until next time -- they cost a few MB, not
        # correctness.
        Get-ChildItem -Path $binDir -Filter "cortex.exe.old-*" -ErrorAction SilentlyContinue | ForEach-Object {
            try { Remove-Item $_.FullName -Force -ErrorAction Stop } catch { }
        }

        $parked = $null
        if (Test-Path $BINARY) {
            $parked = Join-Path $binDir ("cortex.exe.old-" + (Get-Date -Format "yyyyMMddHHmmss"))
            try {
                Rename-Item -Path $BINARY -NewName (Split-Path $parked -Leaf) -ErrorAction Stop
                Write-Prefix "parked running binary -> $(Split-Path $parked -Leaf)"
            }
            catch {
                Write-Prefix "could not park the binary: $($_.Exception.Message)"
                Pop-Location
                exit 1
            }
        }

        Write-Prefix "building..."
        & cargo build --manifest-path $CARGO
        $buildExit = $LASTEXITCODE

        if ($buildExit -ne 0) {
            # Put the working binary back, or a failed build leaves no cortex at
            # all -- worse than the stale one it was replacing.
            if ($parked -and (Test-Path $parked)) {
                Rename-Item -Path $parked -NewName "cortex.exe" -ErrorAction SilentlyContinue
                Write-Prefix "build failed - restored the previous binary"
            }
            Pop-Location
            exit $buildExit
        }

        if (Test-Path $BINARY) {
            $stamp = (Get-Item $BINARY).LastWriteTime.ToString("HH:mm:ss")
            Write-Prefix "deployed: $BINARY (built $stamp)"
            Write-Prefix "the running server keeps its old image until it restarts."
        }
        else {
            # A build can succeed and still not produce the artifact; ask the
            # filesystem rather than trusting the exit code.
            Write-Prefix "build reported success but $BINARY is missing"
            Pop-Location
            exit 1
        }
    }
    "reindex" {
        $extraSources = @()
        if ($Rest -and $Rest.Count -gt 0) {
            $extraSources = $Rest
        }
        $targets = Add-ExtraIndexTargets -Targets $INDEX_TARGETS -ExtraSources $extraSources
        if (-not $targets -or $targets.Count -eq 0) {
            Write-Prefix "No index targets configured. Add targets in $INDEX_CONFIG or pass paths to reindex."
            Pop-Location
            exit 1
        }

        foreach ($t in $targets) {
            if (-not (Test-Path $t.source)) {
                Write-Prefix "WARN: skipping missing source path $($t.source)"
                continue
            }

            $args = @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "index", "--source", $t.source, "--name", $t.name)
            if ($t.scope) {
                Write-Prefix "Re-indexing $($t.source) (scope: $($t.scope)) -> $DB"
                $args += @("--scope", $t.scope)
            }
            else {
                Write-Prefix "Re-indexing $($t.source) (unscoped) -> $DB"
            }

            # Generate this source's api-graph first and hand it to the indexer.
            # The context dir is keyed on scope so quartz/src and the synful fork -
            # which both slugify to "quartz" - cannot overwrite each other.
            $apiGraph = Get-ApiGraphForSource -Source $t.source -Name $t.name -Scope $t.scope
            if ($apiGraph) {
                $args += @("--api-graph", $apiGraph)
            }

            Invoke-OrExit -BaseArgs $args
        }

        Write-Prefix "Done. Indexed configured sources into $DB"
    }
    "doctor" {
        Write-Prefix "Running doctor preflight (unified DB)..."
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "--format", "json", "doctor", "workflow", "--repo", $REPO, "--source", $SOURCE, "--name", $NAME)
    }
    "selfcheck" {
        $ok = Invoke-SelfCheck
        if (-not $ok) {
            Pop-Location
            exit 1
        }
    }
    "mcp-ready" {
        $ok = Invoke-McpReady
        if (-not $ok) {
            Pop-Location
            exit 1
        }
    }
    "smoke" {
        $ok = Invoke-Smoke
        if (-not $ok) {
            Pop-Location
            exit 1
        }
    }
    "migrate-legacy" {
        $ok = Invoke-LegacyMigrationPathway -TriggerCommand $Command
        if (-not $ok) {
            Pop-Location
            exit 1
        }
        Write-Prefix "legacy migration pathway complete."
        Write-Prefix "next: .\\.cortex\\cortex.ps1 smoke -SelfCheckFormat json"
    }
    "status-lite" {
        $ok = Invoke-StatusLite
        if (-not $ok) {
            Pop-Location
            exit 1
        }
    }
    "doctor-lite" {
        $ok = Invoke-DoctorLite
        if (-not $ok) {
            Pop-Location
            exit 1
        }
    }
    "recall" {
        if (-not $Rest -or $Rest.Count -eq 0) {
            Write-Error "Usage: cortex.ps1 recall <topic>"
            Pop-Location
            exit 1
        }
        $Topic = $Rest[0]
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "recall", $Topic)
    }
    "index-self" {
        # Re-index cortex/src itself into the DB with scope 'cortex'
        Write-Prefix "Indexing cortex/src (scope: cortex)..."
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "index", "--source", "cortex/src", "--scope", "cortex", "--name", "Cortex")
    }
    "status" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "--format", "json", "status", "--full")
    }
    "init" {
        # Re-seed workflow anti-patterns, MCP tool annotations, and prefs.toml template.
        # NOTE: As of the first-run init feature, all of this happens automatically when a new
        # DB is created. Use 'init' to force-seed an existing DB that predates this feature,
        # or to re-add annotations after a DB wipe. Anti-patterns always append;
        # MCP annotations are skipped if they already exist (dedup by topic).
        Write-Prefix '=== CORTEX INIT ==='
        Write-Prefix ''

        # ── prefs.toml ────────────────────────────────────────────────────────
        $PrefsPath = '.cortex\prefs.toml'
        if (Test-Path $PrefsPath) {
            Write-Prefix "prefs.toml already exists — skipping template write."
            Write-Prefix "  To reset: delete $PrefsPath and re-run init."
        } else {
            $prefsContent = @'
[style]
line_length = 100
indent = "4 spaces"
naming = "snake_case functions and variables, PascalCase types and enums"
error_handling = "use Result<T, E>; no unwrap() in engine code; ? operator preferred"
comments = "/// doc comments on all public API; inline only for non-obvious logic"

[project]
name = "MyProject"
language = "Rust"
notes = [
    "MANDATORY PRE-CODE CHECK (no PROTOCOL required): before writing any factory/tick/spawn/physics function call get_anti_patterns + get_preferences + list_patterns",
    "MANDATORY MID-TASK CORTEX USAGE: after first approach fails call recall <error_keyword> before retrying. After two failed attempts STOP and call recall or semantic_search before a third.",
    "session-end mandatory: after any coding session run post-session then annotate new bugs as anti-patterns and working implementations as patterns",
]
'@
            Set-Content -Path $PrefsPath -Value $prefsContent -Encoding UTF8
            Write-Prefix "prefs.toml created at $PrefsPath — edit [project].name and add your API notes."
        }

        Write-Prefix ''

        # ── Core workflow anti-patterns ───────────────────────────────────────
        Write-Prefix 'Seeding core workflow anti-patterns...'

        $EXE = $BINARY
        $DBArg = $DB

        & $EXE --db $DBArg anti-pattern add `
            --description 'Skipping cortex recall after the first approach fails - proceeding to a second attempt without checking memory costs a full debug cycle when the answer is already recorded' `
            --wrong 'First approach failed; immediately try different approach without checking cortex' `
            --correct 'First approach failed -> recall <error_keyword> or semantic_search <description> -> if nothing found, THEN try next approach and note the gap' `
            --tags 'workflow,meta,cortex,recall,blocked,debug-cycle'
        Write-Prefix "  [1/4] skip-recall-when-blocked added (exit $LASTEXITCODE)"

        & $EXE --db $DBArg anti-pattern add `
            --description 'Passing a multiline PowerShell string variable to an external exe CLI flag - each newline becomes a separate positional argument causing unexpected argument errors' `
            --wrong '& cortex.exe --body $multiLineVar  // each line of var is a separate arg' `
            --correct '& cortex.exe --body ''Single line. No newlines.''  // all cortex CLI flag values must be single-line' `
            --tags 'powershell,cli,external-exe,multiline,cortex-cli,string-args'
        Write-Prefix "  [2/4] powershell-multiline-cli-arg added (exit $LASTEXITCODE)"

        & $EXE --db $DBArg anti-pattern add `
            --description 'Using && for command chaining in PowerShell 5.1 - not valid, causes parse errors; use semicolon or explicit LASTEXITCODE check' `
            --wrong 'cortex.exe status && cargo build  // && is bash syntax, not valid in PS 5.1' `
            --correct 'cortex.exe status; cargo build  // semicolon for sequential; if ($LASTEXITCODE -eq 0) for conditional' `
            --tags 'powershell,command-chaining,syntax,ps5,bash-habit,cortex-cli'
        Write-Prefix "  [3/4] powershell-and-and-chaining added (exit $LASTEXITCODE)"

        & $EXE --db $DBArg anti-pattern add `
            --description 'Em-dash unicode char in external CLI arg values from PowerShell - some arg parsers treat em-dash as flag separator, splitting following word as separate positional arg' `
            --wrong 'cortex.exe --body ''result is great - no issues''  // em-dash before a word: cortex parser may read that word as a flag' `
            --correct 'cortex.exe --body ''result is great - no issues''  // use ASCII hyphen-minus in all CLI arg values' `
            --tags 'powershell,cli,em-dash,unicode,string-args,cortex-cli,arg-parsing'
        Write-Prefix "  [4/4] powershell-em-dash-cli added (exit $LASTEXITCODE)"

        Write-Prefix ''
        Write-Prefix 'Seeding MCP tool annotations...'
        & "$PSScriptRoot\seed_mcp_annotations.ps1"
        Write-Prefix ''
        Write-Prefix '=== INIT COMPLETE ==='
        Write-Prefix ''
        Write-Prefix 'Next steps:'
        Write-Prefix '  1. Edit .cortex\prefs.toml: set [project].name, add [api] and [patterns] sections'
        Write-Prefix '  2. Run: .\.cortex\cortex.ps1 setup-mcp (write or repair .vscode/mcp.json Cortex entry)'
        Write-Prefix '  3. Run: .\.cortex\cortex.ps1 reindex   (index your source)'
        Write-Prefix '  4. Run: .\.cortex\cortex.ps1 serve     (start MCP server for VS Code)'
        Write-Prefix '  5. Copy the copilot-instructions.md snippet from cortex/README.md'
        Write-Prefix '     into your .github/copilot-instructions.md'
    }
    "post-session" {
        # Run after every coding session: git-review + pending observations + annotation reminder
        Write-Prefix '=== POST-SESSION CORTEX CHECKLIST ==='
        Write-Prefix ''
        Write-Prefix 'Step 1: git-review - scanning changed files for pattern relevance...'
        $reviewResult = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "git-review") -SuppressOutput:$false
        Write-Prefix ''
        Write-Prefix 'Step 2: Pending observations (queued by watch)...'
        $pendingResult = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "review") -SuppressOutput:$false
        Write-Prefix ''
        Write-Prefix '=== ANNOTATION CHECKLIST ==='
        Write-Prefix 'Bug fixed this session? Add anti-pattern:'
        Write-Prefix '  cortex.ps1 -- anti-pattern add --description DESC --wrong BAD --correct GOOD --tags tags'
        Write-Prefix ''
        Write-Prefix 'Feature verified working? Add pattern:'
        Write-Prefix '  cortex.ps1 -- pattern add --name NAME --intent INTENT --body BODY'
        Write-Prefix '  (suffix body with: Trust: verified YYYY-MM-DD)'
        Write-Prefix ''
        Write-Prefix 'New API fact discovered? Add to .cortex/prefs.toml notes array.'
        Write-Prefix ''
        Write-Prefix '=== POST-SESSION COMPLETE ==='
    }
    "quality-check" {
        # Audit: show patterns with 0 uses, flag low survival, count anti-patterns
        Write-Prefix "=== CORTEX QUALITY AUDIT ==="
        Write-Prefix ""
        Write-Prefix "Pattern list (flagging 0-use patterns and survival below 40%):"
        $patResult = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "pattern", "list") -SuppressOutput:$true
        $patResult.Output -split "`n" | Where-Object { $_ -match "used 0x|survival 0%|survival [0-3][0-9]%" } | ForEach-Object { Write-Host "  $_" }
        Write-Prefix ""
        Write-Prefix "Anti-pattern count:"
        $apResult = Invoke-CargoCaptureWithRetry -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "anti-pattern", "list") -SuppressOutput:$true
        $apCount = ($apResult.Output -split "`n" | Where-Object { $_ -match "^\s*\[" } | Measure-Object).Count
        Write-Prefix "  Total anti-patterns: $apCount"
        Write-Prefix ""
        Write-Prefix "To dismiss a dead pattern: cortex.ps1 -- dismiss ID"
        Write-Prefix "=== QUALITY AUDIT COMPLETE ==="
    }
    # ── Phase 1: Session mining + consolidation commands ─────────────────────
    "cluster-sessions" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "cluster-sessions") -TailArgs $Rest
    }
    "detect-skills" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "detect-skills") -TailArgs $Rest
    }
    "propose-gaps" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "propose-gaps") -TailArgs $Rest
    }
    "propose-survival" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "propose-survival") -TailArgs $Rest
    }
    "consolidate-pipeline" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "consolidate-pipeline") -TailArgs $Rest
    }
    "consolidate-if-stale" {
        $stalenessArg = if ($Rest -contains "-StalenessHours") {
            $idx = [array]::IndexOf($Rest, "-StalenessHours")
            if ($idx -ge 0 -and $idx + 1 -lt $Rest.Count) { @("--staleness-hours", $Rest[$idx + 1]) } else { @() }
        } else { @() }
        Invoke-OrExit -BaseArgs (@("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "consolidate-if-stale") + $stalenessArg) -TailArgs @()
    }
    "review-proposals" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "review-proposals") -TailArgs $Rest
    }
    "skill-status" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "skill-status") -TailArgs $Rest
    }
    "skill-approve" {
        if ($Rest.Count -eq 0) { Write-Error "Usage: cortex.ps1 skill-approve <name>"; exit 1 }
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "skill-approve", $Rest[0]) -TailArgs @()
    }
    "skill-reject" {
        if ($Rest.Count -eq 0) { Write-Error "Usage: cortex.ps1 skill-reject <name>"; exit 1 }
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "skill-reject", $Rest[0]) -TailArgs @()
    }
    "session-orphans" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "session-orphans") -TailArgs $Rest
    }
    "health-report" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "health-report") -TailArgs $Rest
    }
    "graph-diff" {
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "graph-diff") -TailArgs $Rest
    }
    "meta" {
        # Usage: cortex.ps1 meta report|propose|apply <id>|dry-run <id>
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, "meta") -TailArgs $Rest
    }
    # ─────────────────────────────────────────────────────────────────────────
    "setup-mcp" {
        Set-McpConfig
        Write-Prefix "MCP setup complete. If VS Code still shows stale state, reload the window."
    }
    "sync-continue-mcp" {
        $syncScript = Join-Path $PSScriptRoot "sync-continue-mcp.ps1"
        if (-not (Test-Path $syncScript)) {
            Write-Error "Missing sync helper script: $syncScript"
            Pop-Location
            exit 1
        }

        & $syncScript @Rest
        $syncExit = $LASTEXITCODE
        if ($syncExit -ne 0) {
            Pop-Location
            exit $syncExit
        }
    }
    "--" {
        # Pass-through: any cortex args directly with the unified DB set
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB) -TailArgs $Rest
    }
    default {
        # Treat unknown command as a cortex subcommand pass-through
        Invoke-OrExit -BaseArgs @("run", "--quiet", "--manifest-path", $CARGO, "--", "--db", $DB, $Command) -TailArgs $Rest
    }
}

Pop-Location
