# Mneme - one-line installer for Windows (v0.3.1+)
#
# Usage (PowerShell, as current user - elevation is optional, see below):
#   iwr -useb https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.ps1 | iex
#
# Flags:
#   -NoToolchain        skip the G1-G10 dev-toolchain auto-install
#                       (Rust / Tauri CLI / Python / SQLite CLI). Use this
#                       in CI / scripted contexts where you control deps
#                       out-of-band.
#   -NoBunCacheClear    skip the Bun cache scrub at step 5b (dev / debug).
#   -WithMultimodal     additionally install the OPTIONAL multimodal native
#                       deps via winget:
#                         G9  Tesseract OCR  (UB-Mannheim.TesseractOCR)
#                         G10 ImageMagick    (ImageMagick.ImageMagick)
#                       Without this flag those two are only DETECTED at
#                       capability-check time and skipped if missing -
#                       mneme falls back to dimensions+EXIF-only image
#                       indexing. Pass `-WithMultimodal` to enable real
#                       OCR + image conversion. Each install is wrapped in
#                       try/catch and a failure is non-fatal: the rest of
#                       the install proceeds normally.
#   -LocalZip <path>    skip the GitHub release fetch and use a local
#                       mneme-windows-x64.zip (or any zip with the same
#                       layout) as the source for step 3/8 extraction.
#                       Use this for air-gapped installs, locally-built
#                       betas, or test ships where you've already pscp'd
#                       a zip onto the target machine. The path must
#                       exist; the file's tag_name is read as 'local-zip'.
#   -SkipDownload       skip BOTH step 2/8 (release metadata) AND step
#                       3/8 (download + extract). Assumes ~/.mneme/ is
#                       already populated (e.g. you manually unzipped a
#                       beta into the target dir). install.ps1 verifies
#                       ~/.mneme/bin/mneme.exe exists before continuing
#                       to step 4/8 (Defender exclusions); if missing,
#                       it errors with a clear remediation hint.
#
# What it does, in order:
#   1. Ensures Bun is installed (runs the official Bun installer if not
#      already present). Bun is the only runtime dependency - mneme's MCP
#      server is TypeScript that Bun runs. Rust, Node, Python are NOT
#      needed: mneme ships as pre-built binaries.
#   2. Downloads mneme-windows-x64.zip from the latest GitHub release.
#   3. Extracts to %USERPROFILE%\.mneme\ (bin/, mcp/, plugin/).
#   4. Adds Windows Defender exclusions for %USERPROFILE%\.mneme\ and
#      %USERPROFILE%\.claude\ (requires admin; falls back to a printed
#      one-liner if not elevated). Prevents Defender's heuristic ML
#      classifier from false-positiving on agent-automation patterns
#      in mneme memory/log files. (A7-010: dropped specific classifier
#      name -- the family-id taxonomy gets renamed/retired across
#      monthly Defender signature updates.)
#   5. Adds the bin directory to the user PATH (persistent, user-scope only).
#   6. Starts the mneme daemon in the background.
#   7. Registers the mneme MCP server with Claude Code AND registers
#      the 8 hook entries under ~/.claude/settings.json by default
#      (K1 fix in v0.3.2). To skip the hook write, pass --no-hooks /
#      --skip-hooks to `mneme install` (see commands/install.rs).
#      Without those hooks the persistent-memory pipeline (history.db,
#      tasks.db, tool_cache.db, livestate.db) stays empty and mneme
#      degrades to a query-only MCP surface.
#   8. Prints next steps and verification commands.
#
# Safe to re-run. Every step is idempotent; a step that fails prints a
# clear message and does not abort the remaining steps (except when
# download / extract themselves fail, which is unrecoverable).
#
# Zero-prereq guarantee: running this script on a stock Windows machine
# with PowerShell produces a working mneme install WITHOUT the user
# pre-installing anything else.
#
# Uninstall: `mneme uninstall --platform claude-code` + remove
# %USERPROFILE%\.mneme\ manually. A full `mneme uninstall` command with
# rollback receipts lands in v0.3.2.

[CmdletBinding()]
param(
    [switch]$NoToolchain,     # skip G1-G10 auto-install (CI / scripted contexts)
    [switch]$NoBunCacheClear, # skip clearing bun cache (dev / debugging)
    # Idempotent-2: when invoked in unattended mode (LocalZip / non-interactive
    # host), the Bun cache wipe at step 5b nukes `~/.bun/install/cache` AND
    # `%LOCALAPPDATA%/Bun/Cache` - kills other Bun projects' caches. Default
    # OFF for unattended paths unless the user explicitly opts in.
    [switch]$ForceBunCacheClear,
    # A7-012 (2026-05-04): zero-question install is the design principle.
    # Even an interactive shell now defaults to "skip + print mitigation"
    # and only prompts when this flag is explicitly set.
    [switch]$PromptForBunCacheClear,
    [switch]$WithMultimodal,  # also install G9 Tesseract OCR + G10 ImageMagick via winget
    [Parameter()]
    [string]$LocalZip = $null, # path to a local mneme zip; skips GitHub fetch in step 2/8 and uses this in step 3/8
    [Parameter()]
    [switch]$SkipDownload,    # skip BOTH step 2/8 + step 3/8; assume ~/.mneme is already populated
    # ---- v0.3.2 install-reliability additions (step 6 + 7b + 7c) ----
    [Parameter()]
    [string]$ModelsPath = $null,  # explicit models bundle dir; if null, step 7b auto-detects <bundle>/models next to -LocalZip
    [Parameter()]
    [switch]$NoModels,            # skip step 7b entirely (don't auto-install models even if found)
    [Parameter()]
    [switch]$NoScheduledTask      # skip step 6 schtasks registration; fall back to Start-Process spawn (legacy path)
)

$ErrorActionPreference = 'Stop'

$Repo       = 'omanishay-cyber/mneme'
$Asset      = 'mneme-windows-x64.zip'
$MnemeHome  = Join-Path $env:USERPROFILE '.mneme'
$BinDir     = Join-Path $MnemeHome 'bin'
$ClaudeHome = Join-Path $env:USERPROFILE '.claude'
$ManifestFile = Join-Path $MnemeHome '.install-manifest.json'

# NEW-005: --upgrade vs --fresh-install distinction.
#   The script is normally invoked via `iwr | iex`, which can't take
#   command-line parameters cleanly. Drive the choice via env vars set
#   BEFORE the iex line:
#     $env:MNEME_INSTALL_MODE = 'upgrade'        # keep daemon running
#     $env:MNEME_INSTALL_MODE = 'fresh-install'  # full clean install
#   Default heuristic: if a prior install manifest exists, treat as upgrade.
$IsUpgrade = $false
$Mode = $env:MNEME_INSTALL_MODE
if ($Mode -eq 'upgrade') {
    $IsUpgrade = $true
} elseif ($Mode -eq 'fresh-install') {
    $IsUpgrade = $false
} elseif (Test-Path $ManifestFile) {
    $IsUpgrade = $true
}

# Tell child `mneme install` invocations the script-level steps already
# ran so they don't print the cli-only-install warning (NEW-002).
$env:MNEME_INSTALLED_BY_SCRIPT = '1'

# ----------------------------------------------------------------------------
# CONSOLE ENCODING -- force UTF-8
# ----------------------------------------------------------------------------
# Without this, Windows PowerShell 5.1 inherits the OEM code page (CP437 on
# US-English Windows). When the script (or its child processes -- `mneme.exe
# register-mcp`, `mneme.exe doctor`) writes Unicode glyphs (✓ U+2713,
# ✗ U+2717, ─ U+2500, -- U+2014), the console renders the UTF-8 byte
# sequences as separate CP437 chars: ✓ → "Γ£ô", ─ → "ΓöÇ", -- → "ΓÇö".
# Setting both [Console]::OutputEncoding (governs how PS reads child stdout)
# and $OutputEncoding (governs how PS encodes when piping to children) to
# UTF-8 fixes both directions. Wrapped in try/catch because some hosts
# (ISE legacy, Constrained Language Mode) reject [Console] modification.
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding            = [System.Text.Encoding]::UTF8
} catch { }

# B5: silence Invoke-WebRequest "Writing web request" progress chatter
# script-wide. Every Invoke-WebRequest call site in this installer
# (G1 Rust, G2 Bun, G3 Node, G4 Git, G7 SQLite, daemon health probe,
# zip download) inherits this. Set at script scope so we don't have
# to wrap each inline IWR.
$ProgressPreference = 'SilentlyContinue'

function Write-Step {
    param([string]$Message, [string]$Color = 'Cyan')
    Write-Host ("==> {0}" -f $Message) -ForegroundColor $Color
}
function Write-Info {
    param([string]$Message)
    Write-Host ("    {0}" -f $Message)
}
function Write-Warn {
    param([string]$Message)
    Write-Host ("    warning: {0}" -f $Message) -ForegroundColor Yellow
}
function Write-OK {
    param([string]$Message)
    Write-Host ("    ok: {0}" -f $Message) -ForegroundColor Green
}
function Write-Fail {
    param([string]$Message)
    Write-Host ("    error: {0}" -f $Message) -ForegroundColor Red
}

function Test-IsElevated {
    try {
        $id  = [Security.Principal.WindowsIdentity]::GetCurrent()
        $p   = New-Object Security.Principal.WindowsPrincipal($id)
        return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    } catch {
        return $false
    }
}

# B-006 follow-on: generic native-exe probe.
#
# install.ps1 runs with `$ErrorActionPreference = 'Stop'` so any
# unhandled error aborts the install. That includes `NativeCommandError`
# raised when a native exe writes to stderr OR exits non-zero. Every G1-G10
# probe of the form `& $exe.Source --version 2>&1` is therefore a tripwire:
# the moment a probed tool isn't installed (cargo tauri without tauri-cli,
# python.exe pointing at the Microsoft-Store stub, sqlite3 missing, etc.)
# the probe blows up and the rest of the install never runs.
#
# `Invoke-NativeProbe` is the ONE pattern install.ps1 should use everywhere
# it calls a native exe to probe presence/version. It:
#   - returns a structured result instead of throwing,
#   - swallows the EAP=Stop tripwire by setting EAP='Continue' inside,
#   - captures combined stdout+stderr so the caller can grep for hints
#     ("no such command", "is not installed", etc.),
#   - treats $null `$LASTEXITCODE` (PowerShell's first-native-call quirk)
#     as success.
#
# Returns a PSCustomObject with:
#   .Success  $true if exit code 0 (or null -> treated as 0)
#   .Output   raw 2>&1 output (string or array)
#   .ExitCode integer exit code, -1 if exe missing or threw
#
# Safe to call with EAP=Stop set in the parent scope. Never throws.
function Invoke-NativeProbe {
    param(
        [string]$ExePath,
        [string[]]$ProbeArgs = @('--version'),
        [int]$TimeoutSec = 10
    )
    if (-not $ExePath) {
        return [PSCustomObject]@{ Success = $false; Output = $null; ExitCode = -1 }
    }
    if (-not (Test-Path -LiteralPath $ExePath)) {
        return [PSCustomObject]@{ Success = $false; Output = $null; ExitCode = -1 }
    }
    try {
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $rawOut = & $ExePath @ProbeArgs 2>&1
        $code = $LASTEXITCODE
        $ErrorActionPreference = $prevEAP
        # $LASTEXITCODE is $null on the first native-cmd call in a fresh
        # runspace; treat $null OR 0 as a clean exit. Non-zero numeric is
        # the only true failure signal.
        if ($null -eq $code) { $code = 0 }
        return [PSCustomObject]@{
            Success  = ($code -eq 0)
            Output   = $rawOut
            ExitCode = $code
        }
    } catch {
        # Last-resort containment: if some odd PSHost still surfaces a
        # NativeCommandError despite EAP=Continue, fall through here. The
        # caller treats Success=$false the same way regardless of why.
        return [PSCustomObject]@{
            Success  = $false
            Output   = $_.Exception.Message
            ExitCode = -1
        }
    }
}

# B-006 fix: Microsoft-Store Python stub detection.
#
# `Get-Command python` on stock Windows resolves to the Microsoft-Store stub
# at `C:\Users\<user>\AppData\Local\Microsoft\WindowsApps\python.exe`. The
# stub prints "Python was not found; ... Microsoft Store ..." to stderr,
# returns a non-zero exit code, AND triggers the Store-install popup. Under
# `$ErrorActionPreference = 'Stop'` (this script's mode), that throws a
# `NativeCommandError` and aborts the install before our regex check can
# fire - the symptom seen on EC2 (SESSION-2026-04-27-EC2-TEST-LOG B-006).
#
# This function detects the stub by PATH inspection FIRST (no exec, so no
# popup), then optionally probes `--version` with stop-action contained.
# Returns:
#   $true   - exe is a real Python (multi-MB cpython, returns "Python X.Y.Z")
#   $false  - null path, file missing, WindowsApps stub, or version probe failed
#
# Safe to call before any `--version` invocation. Never throws.
function Test-PythonRealOrStub {
    param([string]$ExePath)
    if (-not $ExePath) { return $false }
    if (-not (Test-Path $ExePath)) { return $false }
    # PATH check is the cheap, popup-free signal. The stub lives EXCLUSIVELY
    # under `*\WindowsApps\*` (per Microsoft Store app-execution-alias spec).
    # Real Python lives under `*\Programs\Python\*`, `*\Program Files\*`,
    # `*\.pyenv\*`, `*\Miniconda*`, `*\Anaconda*`, etc. - never WindowsApps.
    if ($ExePath -like '*\WindowsApps\*') { return $false }
    # Belt-and-suspenders: capture --version output AND exit code. Wrap the
    # invocation so $ErrorActionPreference='Stop' can't abort the script on
    # a NativeCommandError. Real Python prints "Python 3.X.Y" on stdout
    # and exits 0; the stub prints to stderr and exits non-zero.
    #
    # Subtle gotcha: piping `& exe --version 2>&1 | Select-Object -First 1`
    # closes the pipeline early, which can leave $LASTEXITCODE unset
    # ($null) on fast-exiting commands. Capture the full output first,
    # check $LASTEXITCODE, THEN narrow to first line.
    try {
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $rawOut = & $ExePath --version 2>&1
        $code = $LASTEXITCODE
        $ErrorActionPreference = $prevEAP
        # $LASTEXITCODE may be $null on the first native call in a runspace;
        # treat $null OR 0 as "exited cleanly". Non-zero numeric = failure.
        if ($null -ne $code -and $code -ne 0) { return $false }
        if ($null -eq $rawOut) { return $false }
        # $rawOut may be a single string OR an array. Take first line.
        $firstLine = if ($rawOut -is [array]) { $rawOut[0] } else { $rawOut }
        $outStr = [string]$firstLine
        if ($outStr -match 'Microsoft Store|App execution aliases|was not found') { return $false }
        return ($outStr -match '^Python \d+\.\d+(\.\d+)?')
    } catch {
        return $false
    }
}

# Test-MsvcLinker - Bug F gate.
#
# Purpose: detect whether MSVC's link.exe + cl.exe are on PATH BEFORE
# `cargo install tauri-cli` (G4) is attempted. cargo install on Windows
# downloads ~53 MB / 560 crates and compiles for 3-5 minutes - and then
# fails at the link stage with `linker 'link.exe' not found` if MSVC
# Build Tools aren't present. Pre-checking saves the wasted minutes and
# gives the user actionable remediation up-front.
#
# Returns: $true if BOTH link.exe and cl.exe resolve via Get-Command;
#          $false otherwise. Uses -ErrorAction SilentlyContinue so the
#          probe never throws under EAP=Stop.
function Test-MsvcLinker {
    $linkExe = Get-Command link.exe -ErrorAction SilentlyContinue
    $clExe   = Get-Command cl.exe -ErrorAction SilentlyContinue
    return ($linkExe -ne $null -and $clExe -ne $null)
}

# ============================================================================
# -LocalZip / -SkipDownload validation
# ============================================================================
#
# Resolve the install source mode BEFORE the banner so it can render the
# active source line ('github' / 'local zip <path>' / 'pre-extracted'). We
# also resolve $LocalZip to an absolute path here so any later step that
# logs it shows the canonical path (Resolve-Path errors out if the file
# does not exist - which is exactly the validation we want).
#
# Mutually-exclusive: passing BOTH -LocalZip and -SkipDownload is almost
# certainly a mistake (LocalZip implies extract; SkipDownload implies skip
# extract). Fail fast rather than guess intent.

if ($LocalZip -and $SkipDownload) {
    Write-Host "==> mneme - one-line installer" -ForegroundColor Cyan
    Write-Host "    error: -LocalZip and -SkipDownload are mutually exclusive" -ForegroundColor Red
    Write-Host "           -LocalZip <path>  : extract from a local zip (skip GitHub fetch)" -ForegroundColor Red
    Write-Host "           -SkipDownload     : skip BOTH fetch + extract (assume ~/.mneme already populated)" -ForegroundColor Red
    exit 1
}

if ($LocalZip) {
    if (-not (Test-Path -LiteralPath $LocalZip)) {
        Write-Host "==> mneme - one-line installer" -ForegroundColor Cyan
        Write-Host ("    error: -LocalZip path does not exist: {0}" -f $LocalZip) -ForegroundColor Red
        exit 1
    }
    # Canonicalise so logs show the resolved path, not whatever relative
    # form the caller passed.
    $LocalZip = (Resolve-Path -LiteralPath $LocalZip).Path
}

if ($SkipDownload -or $LocalZip) {
    $InstallSource = if ($SkipDownload) { 'pre-extracted (-SkipDownload, no fetch + no extract)' } else { "local zip $LocalZip" }
} else {
    $InstallSource = ("github releases ({0}/releases/latest)" -f $Repo)
}

Write-Step "mneme - one-line installer"
Write-Info ("target      : {0}" -f $MnemeHome)
Write-Info ("bin         : {0}" -f $BinDir)
Write-Info ("elevated    : {0}" -f (Test-IsElevated))
Write-Info ("source      : {0}" -f $InstallSource)
Write-Info ("toolchain   : {0}" -f $(if ($NoToolchain) { 'skipped (-NoToolchain)' } else { 'auto-install G1-G10 (Rust/Bun/Node/Git/Tauri/Python/SQLite)' }))
Write-Info ("multimodal  : {0}" -f $(if ($WithMultimodal) { 'enabled (-WithMultimodal): G9 Tesseract OCR + G10 ImageMagick via winget' } else { 'detect-only (pass -WithMultimodal to auto-install Tesseract+ImageMagick)' }))
Write-Host ""

# ============================================================================
# Step 0 - Stop any running mneme processes (upgrade safety)
# ============================================================================
#
# If an existing daemon is running, the mneme.exe / mneme-daemon.exe /
# worker binaries are file-locked. Expand-Archive silently skips locked
# files, leaving a mixed-version install where the *.dll metadata says
# v0.3.1 but some of the executable bodies are still v0.3.0. That looks
# identical to "install succeeded" but actually shipped broken binaries.
#
# Unconditional stop is safe: if no daemon is running, this is a no-op.
# The supervisor is restarted later in step 6.

Write-Step "step 0/8 - stop any existing mneme daemon + workers"

# NEW-053: 3-pass kill ladder. Stop-Process by PID is the friendly first
# attempt; if procs still hold file locks after that, escalate to
# taskkill /F /T which closes child handles too (supervisor + workers).
# Last pass is per-binary taskkill /F /IM as belt-and-suspenders for any
# orphan worker that lost its parent ancestry.

# Pass 1-2: graceful Stop-Process per PID
$tries = 0
do {
    $running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^mneme' }
    if ($running) {
        Write-Info ("Stop-Process pass {0}: stopping {1} mneme proc(s): {2}" -f ($tries+1), $running.Count, (($running.ProcessName | Sort-Object -Unique) -join ', '))
        $running | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
    $tries++
} while ($running -and $tries -lt 2)

# Pass 3: nuclear taskkill /F /T (tree-kill via toolhelp ancestry)
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^mneme' }
if ($running) {
    Write-Info "Stop-Process did not clear all procs - escalating to taskkill /F /T"
    cmd /c "taskkill /F /T /IM mneme-daemon.exe" 2>$null | Out-Null
    foreach ($exe in @(
        'mneme.exe',
        'mneme-store.exe',
        'mneme-parsers.exe',
        'mneme-scanners.exe',
        'mneme-livebus.exe',
        'mneme-md-ingest.exe',
        'mneme-brain.exe',
        'mneme-multimodal.exe'
    )) {
        cmd /c ("taskkill /F /IM {0}" -f $exe) 2>$null | Out-Null
    }
    Start-Sleep -Seconds 2
}

$leftover = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^mneme' }
if ($leftover) {
    # Hard abort: if even nuclear taskkill cannot clear the procs, install
    # WILL ship mixed-version binaries. Better to fail loudly here than
    # let Expand-Archive silently skip locked files.
    # BONUS-1: was `Write-Err` (undefined function). Use Write-Fail which
    # is defined at the top of this script - calling Write-Err triggered
    # `CommandNotFoundException` and short-circuited the FATAL branch
    # before `exit 1`, leaving install to march on with locked binaries.
    Write-Fail ("FATAL: {0} mneme process(es) still running after Stop-Process + taskkill /F /T" -f $leftover.Count)
    Write-Fail ("       still alive: {0}" -f (($leftover.ProcessName | Sort-Object -Unique) -join ', '))
    Write-Fail "       close any mneme window or VS Code Claude session, then rerun install.ps1"
    exit 1
} else {
    Write-OK "no mneme processes running - safe to extract"
}

# ============================================================================
# Step 1 - Check + install runtime prerequisites
# ============================================================================
#
# Three tools matter for a full mneme + Claude-Code experience:
#
#   Bun       - mneme's MCP server (`mneme mcp stdio`) launches TypeScript
#               via `bun`. Required for `/mn-*` commands in Claude Code.
#   Node.js   - only needed if the user wants the Claude Code CLI
#               (`npm install -g @anthropic-ai/claude-code`). Not strictly
#               required to run mneme itself, but the whole point of mneme
#               is to serve Claude Code, so we install it by default.
#   git       - only needed for `mneme build` on git repos (so mneme can
#               pin the indexed commit SHA per project). Mneme works
#               without it; just no git metadata in the graph.
#
# Rust is deliberately NOT installed - mneme ships pre-built binaries.
# Python is not needed for v0.3.1 (multimodal sidecar is feature-gated).
#
# Every check below follows the same pattern: detect on PATH, detect at
# standard user-scope install path, install if missing. All installs are
# user-scope where possible (no admin required); fall back to system
# installer where the official path does. Idempotent on every re-run.

function Test-Tool {
    param([string]$Name, [string]$FallbackPath)
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    if ($FallbackPath -and (Test-Path $FallbackPath)) { return $FallbackPath }
    return $null
}

# --- 1a. Bun (required for MCP server) --------------------------------------
Write-Step "step 1/8 - Bun runtime (required for mneme MCP)"

$BunFallback = Join-Path $env:USERPROFILE '.bun\bin\bun.exe'
$BunExe = Test-Tool -Name 'bun' -FallbackPath $BunFallback

if ($BunExe) {
    # Use Invoke-NativeProbe so a malformed bun.exe / stale install can't
    # abort the script under EAP=Stop.
    $bunProbe = Invoke-NativeProbe -ExePath $BunExe -ProbeArgs @('--version')
    if ($bunProbe.Success) {
        $BunVer = ([string]$bunProbe.Output).Trim()
        Write-OK ("bun $BunVer present at $BunExe")
    } else {
        Write-OK ("bun present at $BunExe (version check failed, continuing)")
    }
} else {
    Write-Info "bun not found - installing via direct GitHub release download"
    try {
        # The bun.sh/install.ps1 script uses `curl.exe -#` which errors in
        # non-interactive sessions (see v0.3.0 install-report). We pull the
        # release ZIP ourselves - works in any shell context.
        $bunBin = Join-Path $env:USERPROFILE '.bun\bin'
        New-Item -ItemType Directory -Force -Path $bunBin | Out-Null
        $bunZip = Join-Path $env:TEMP 'bun-windows-x64.zip'
        Invoke-WebRequest -Uri 'https://github.com/oven-sh/bun/releases/latest/download/bun-windows-x64.zip' -OutFile $bunZip -UseBasicParsing
        Expand-Archive -Path $bunZip -DestinationPath $env:TEMP -Force
        Copy-Item (Join-Path $env:TEMP 'bun-windows-x64\bun.exe') $bunBin -Force
        # Persist PATH (user-scope)
        $userPath = [Environment]::GetEnvironmentVariable('PATH','User')
        if ($userPath -notmatch [regex]::Escape($bunBin)) {
            [Environment]::SetEnvironmentVariable('PATH', "$userPath;$bunBin", 'User')
        }
        $env:PATH = "$env:PATH;$bunBin"
        $BunExe = Join-Path $bunBin 'bun.exe'
        $bunProbePost = Invoke-NativeProbe -ExePath $BunExe -ProbeArgs @('--version')
        $bunVerStr = if ($bunProbePost.Success) { ([string]$bunProbePost.Output).Trim() } else { '?' }
        Write-OK ("bun $bunVerStr installed at $BunExe")
    } catch {
        # Bug G-13 (2026-05-01): Bun is the runtime for the MCP server.
        # Without it, every MCP tool registered with Claude Code fails
        # to start. Previously we Write-Warn'd and continued, exiting
        # with code 0 -- the user got an "install complete" message and
        # then "mneme: disconnected" forever in Claude Code. Failing
        # loud here stops the install at the right step so the user can
        # see and act.
        Write-Fail ("Bun install failed: {0}" -f $_.Exception.Message)
        Write-Fail "Bun is REQUIRED for the mneme MCP server (Claude Code integration)."
        Write-Fail "Manual install: https://bun.sh/install"
        Write-Fail "Then re-run install.ps1."
        exit 1
    }
}

# --- 1b. Node.js + npm (for Claude Code CLI) --------------------------------
Write-Step "step 1b/8 - Node.js + npm (for Claude Code CLI)"

$NodeExe = Test-Tool -Name 'node' -FallbackPath 'C:\Program Files\nodejs\node.exe'

if ($NodeExe) {
    $nodeProbe = Invoke-NativeProbe -ExePath $NodeExe -ProbeArgs @('--version')
    $NodeVer = if ($nodeProbe.Success) { ([string]$nodeProbe.Output).Trim() } else { '?' }
    Write-OK ("node $NodeVer present at $NodeExe")
} else {
    Write-Info "node not found - installing Node.js LTS via direct MSI"
    try {
        $nodeUrl = 'https://nodejs.org/dist/v22.13.1/node-v22.13.1-x64.msi'
        $nodeMsi = Join-Path $env:TEMP 'node-lts.msi'
        Invoke-WebRequest -Uri $nodeUrl -OutFile $nodeMsi -UseBasicParsing
        $p = Start-Process msiexec.exe -ArgumentList '/i', "`"$nodeMsi`"", '/qn', '/norestart' -Wait -PassThru
        if ($p.ExitCode -eq 0) {
            # Refresh session PATH so subsequent steps find npm
            $env:PATH = [Environment]::GetEnvironmentVariable('PATH','Machine') + ';' + [Environment]::GetEnvironmentVariable('PATH','User')
            $NodeExe = Test-Tool -Name 'node' -FallbackPath 'C:\Program Files\nodejs\node.exe'
            if ($NodeExe) {
                $nodeProbePost = Invoke-NativeProbe -ExePath $NodeExe -ProbeArgs @('--version')
                $nodeVerStr = if ($nodeProbePost.Success) { ([string]$nodeProbePost.Output).Trim() } else { '?' }
                Write-OK ("node $nodeVerStr installed")
            } else {
                Write-Warn "node installer exited 0 but node not on PATH - re-open shell"
            }
        } else {
            Write-Warn ("node MSI exited with code {0}" -f $p.ExitCode)
        }
    } catch {
        Write-Warn ("Node.js install failed: {0}" -f $_.Exception.Message)
        Write-Warn "Claude Code CLI will not be installable until Node is present"
        Write-Warn "Manual install: https://nodejs.org/"
    }
}

# --- 1c. git (optional, for `mneme build` on git repos) ---------------------
Write-Step "step 1c/8 - git (optional, for richer project metadata)"

$GitExe = Test-Tool -Name 'git' -FallbackPath 'C:\Program Files\Git\cmd\git.exe'

if ($GitExe) {
    $gitProbe = Invoke-NativeProbe -ExePath $GitExe -ProbeArgs @('--version')
    $GitVer = if ($gitProbe.Success) { ([string]$gitProbe.Output).Trim() } else { '?' }
    Write-OK ("git $GitVer present at $GitExe")
} else {
    Write-Info "git not found - installing Git for Windows (silent)"
    try {
        $gitUrl = 'https://github.com/git-for-windows/git/releases/download/v2.48.1.windows.1/Git-2.48.1-64-bit.exe'
        $gitExe = Join-Path $env:TEMP 'git-setup.exe'
        Invoke-WebRequest -Uri $gitUrl -OutFile $gitExe -UseBasicParsing
        $p = Start-Process $gitExe -ArgumentList '/VERYSILENT','/NORESTART','/NOCANCEL','/SP-','/SUPPRESSMSGBOXES' -Wait -PassThru
        if ($p.ExitCode -eq 0) {
            $env:PATH = [Environment]::GetEnvironmentVariable('PATH','Machine') + ';' + [Environment]::GetEnvironmentVariable('PATH','User')
            Write-OK "git installed"
        } else {
            Write-Warn ("git installer exited with code {0}" -f $p.ExitCode)
        }
    } catch {
        Write-Warn ("git install failed: {0}" -f $_.Exception.Message)
        Write-Warn "mneme will still work without git; just no commit-SHA metadata"
    }
}

# --- 1d. Dev-toolchain auto-install (G1-G10 from phase-a-issues.md) ---------
#
# G-fix (project directive 2026-04-26): "mneme should check, host pc has bun, rust,
# sqlite, node, python, cargo all others installed or not and it should
# pull installation and setup environment as i didnt have tauri yesterday
# and went into issues."
#
# Auto-install the high-value tools (Rust, Tauri CLI, Python, SQLite CLI)
# so that subsequent `mneme view` / Tauri builds / multimodal flows just
# work out-of-the-box. Pass `-NoToolchain` to skip the auto-install for
# CI / scripted contexts.

Write-Step "step 1d/8 - dev-toolchain auto-install (G1-G10)"

if ($NoToolchain) {
    Write-Info "  -NoToolchain set - skipping G1-G10 auto-install"
} else {
    # G1: Rust toolchain (HIGH) - required for vision/tauri build and any
    # future Rust-port work. Install via rustup-init.exe (Microsoft signed,
    # widely trusted, idempotent).
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Info "[G1] Rust missing - installing via rustup-init.exe"
        try {
            $rustupExe = Join-Path $env:TEMP 'rustup-init.exe'
            Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustupExe -UseBasicParsing
            $p = Start-Process $rustupExe -ArgumentList '-y','--default-toolchain','stable','--no-modify-path' -Wait -PassThru
            if ($p.ExitCode -eq 0) {
                # rustup-init drops to ~/.cargo/bin; add to session PATH so
                # subsequent steps see cargo without shell restart.
                $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
                if (Test-Path $cargoBin) {
                    $env:PATH = "$env:PATH;$cargoBin"
                    $userPath = [Environment]::GetEnvironmentVariable('PATH','User')
                    if ($userPath -notmatch [regex]::Escape($cargoBin)) {
                        [Environment]::SetEnvironmentVariable('PATH', "$userPath;$cargoBin", 'User')
                    }
                    $cargoExePath = Join-Path $cargoBin 'cargo.exe'
                    $cargoProbeInstall = Invoke-NativeProbe -ExePath $cargoExePath -ProbeArgs @('--version')
                    $cargoVer = if ($cargoProbeInstall.Success) { ([string]$cargoProbeInstall.Output).Trim() } else { '?' }
                    Write-OK "[G1] Rust installed: $cargoVer"
                } else {
                    Write-Warn "[G1] rustup-init exited 0 but ~/.cargo/bin missing"
                }
            } else {
                Write-Warn ("[G1] rustup-init exited {0}" -f $p.ExitCode)
            }
        } catch {
            Write-Warn ("[G1] Rust install failed: {0}" -f $_.Exception.Message)
        }
    } else {
        $cargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
        if ($cargoCmd) {
            $cargoProbe = Invoke-NativeProbe -ExePath $cargoCmd.Source -ProbeArgs @('--version')
            if ($cargoProbe.Success) {
                Write-OK ("[G1] Rust present: " + ([string]$cargoProbe.Output).Trim())
            } else {
                Write-OK "[G1] Rust present"
            }
        } else {
            Write-OK "[G1] Rust present"
        }
    }

    # G6: Python + Pillow. Used for PNG->ICO conversion (Tauri icon synth)
    # and multimodal sidecar fallbacks. winget delivers a real Python
    # (not the Microsoft Store stub that hijacks `python.exe`).
    $pythonExe = Get-Command python -ErrorAction SilentlyContinue
    if (-not $pythonExe) { $pythonExe = Get-Command python3 -ErrorAction SilentlyContinue }
    # B-006 fix: detect the Microsoft Store stub via PATH inspection BEFORE
    # invoking `--version`. Calling the stub triggers a Store-install popup
    # AND a NativeCommandError that aborts under $ErrorActionPreference='Stop'.
    # `Test-PythonRealOrStub` returns $false for stub OR missing OR broken
    # python, so the install branch below proceeds in all those cases.
    $pythonReal = $false
    $pythonExePath = $null
    if ($pythonExe) {
        $pythonExePath = $pythonExe.Source
        if (Test-PythonRealOrStub -ExePath $pythonExePath) {
            $pythonReal = $true
        } else {
            Write-Info ("[G6] python at {0} is the Microsoft-Store stub or non-functional - reinstalling" -f $pythonExePath)
        }
    }
    if (-not $pythonReal) {
        Write-Info "[G6] Python missing - installing via winget Python.Python.3.12 (silent)"
        try {
            $w = Get-Command winget -ErrorAction SilentlyContinue
            if ($w) {
                $p = Start-Process winget -ArgumentList 'install','-e','--id','Python.Python.3.12','--silent','--accept-source-agreements','--accept-package-agreements' -Wait -PassThru
                if ($p.ExitCode -eq 0 -or $p.ExitCode -eq -1978335189) {
                    # Refresh PATH so subsequent calls see python
                    $env:PATH = [Environment]::GetEnvironmentVariable('PATH','Machine') + ';' + [Environment]::GetEnvironmentVariable('PATH','User')
                    # B-006 fix: PATH order may still resolve the WindowsApps
                    # stub before the freshly-installed real Python. Walk all
                    # `python.exe` matches via `Get-Command -All` and pick the
                    # first one that passes Test-PythonRealOrStub.
                    $candidates = @(Get-Command python -All -ErrorAction SilentlyContinue) +
                                  @(Get-Command python3 -All -ErrorAction SilentlyContinue)
                    $realExe = $null
                    foreach ($c in $candidates) {
                        if ($c -and $c.Source -and (Test-PythonRealOrStub -ExePath $c.Source)) {
                            $realExe = $c.Source
                            break
                        }
                    }
                    # Fallback: probe standard winget install locations directly.
                    if (-not $realExe) {
                        $probePaths = @(
                            "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe",
                            "$env:LOCALAPPDATA\Programs\Python\Python313\python.exe",
                            "$env:ProgramFiles\Python312\python.exe",
                            "$env:ProgramFiles\Python313\python.exe"
                        )
                        foreach ($pp in $probePaths) {
                            if (Test-PythonRealOrStub -ExePath $pp) { $realExe = $pp; break }
                        }
                    }
                    if ($realExe) {
                        $pyProbePost = Invoke-NativeProbe -ExePath $realExe -ProbeArgs @('--version')
                        $verStr = if ($pyProbePost.Success) { ([string]$pyProbePost.Output).Trim() } else { '' }
                        Write-OK ("[G6] Python installed: {0} ({1})" -f $verStr, $realExe)
                        # Pillow for PNG->ICO conversion. pip itself can write to
                        # stderr on a clean install (download progress, etc.) and
                        # would fire NativeCommandError under EAP=Stop. Contain it.
                        $null = Invoke-NativeProbe -ExePath $realExe -ProbeArgs @('-m','pip','install','--quiet','--user','Pillow')
                    } else {
                        Write-Warn "[G6] winget reported success but no real Python found on PATH or standard locations - re-open shell and rerun"
                    }
                } else {
                    Write-Warn ("[G6] winget Python install exited {0}" -f $p.ExitCode)
                }
            } else {
                Write-Warn "[G6] winget not present - install Python manually from https://www.python.org/downloads/"
            }
        } catch {
            Write-Warn ("[G6] Python install failed: {0}" -f $_.Exception.Message)
        }
    } else {
        $pyProbe = Invoke-NativeProbe -ExePath $pythonExePath -ProbeArgs @('--version')
        if ($pyProbe.Success) {
            Write-OK ("[G6] Python present: " + ([string]$pyProbe.Output).Trim())
        } else {
            Write-OK "[G6] Python present"
        }
    }

    # G4: Tauri probe REMOVED (2026-05-02, AWS install regression cycle).
    # The release ships prebuilt `mneme-vision.exe` (Tauri-built binary)
    # inside the zip. End users do NOT build vision/tauri from source via
    # the live iex (irm) install path. Anyone rebuilding vision themselves
    # can run `cargo install tauri-cli` on their own. Probing here added a
    # confusing "SKIPPED" line in install output that suggested Tauri was a
    # missing dependency, when in reality nothing in the install path or
    # runtime path needs it.

    # G7: sqlite3 CLI. Optional but valuable for shard diagnostics.
    #
    # Bug G fix: try winget primary, then HEAD-probe a candidate list of
    # sqlite.org URLs before downloading. The previous implementation
    # hardcoded `https://www.sqlite.org/2025/sqlite-tools-win-x64-3470100.zip`
    # which sqlite.org rotated out of /2025/ - a live 404 on 2026-04-29.
    # winget removes our dependency on sqlite.org's filename schema entirely;
    # the portable fallback only runs if winget is unavailable or fails
    # (e.g. corporate networks blocking the winget CDN).
    if (-not (Get-Command sqlite3 -ErrorAction SilentlyContinue)) {
        $sqliteInstalled = $false

        # Primary path: winget. Adds sqlite3 to system PATH automatically.
        $wingetExe = Get-Command winget -ErrorAction SilentlyContinue
        if ($wingetExe) {
            Write-Info "[G7] SQLite CLI missing - trying winget install SQLite.SQLite"
            try {
                $wp = Start-Process winget -ArgumentList 'install','--id','SQLite.SQLite','--silent','--accept-source-agreements','--accept-package-agreements' -Wait -PassThru -NoNewWindow
                if ($wp.ExitCode -eq 0) {
                    Write-OK "[G7] SQLite installed via winget"
                    $sqliteInstalled = $true
                } else {
                    Write-Warn ("[G7] winget install SQLite.SQLite exited {0} - trying portable fallback" -f $wp.ExitCode)
                }
            } catch {
                Write-Warn ("[G7] winget install threw: {0} - trying portable fallback" -f $_.Exception.Message)
            }
        } else {
            Write-Info "[G7] winget unavailable - using portable fallback for SQLite"
        }

        # Fallback path: HEAD-probe a list of candidate sqlite.org URLs. Each
        # URL is verified with `Invoke-WebRequest -Method Head` BEFORE
        # downloading, so a rotated/404 URL is skipped instead of crashing
        # the install. Order: newest forward-looking URL first, then the
        # historical 2025 URL, so future sqlite.org filename rotations are
        # accommodated by adding a new entry to the top.
        if (-not $sqliteInstalled) {
            Write-Info "[G7] downloading SQLite portable to ~/.mneme/bin/"
            $sqliteUrls = @(
                'https://www.sqlite.org/2026/sqlite-tools-win-x64-3490000.zip',
                'https://www.sqlite.org/2025/sqlite-tools-win-x64-3470100.zip'
            )
            $resolvedUrl = $null
            foreach ($candidate in $sqliteUrls) {
                try {
                    $head = Invoke-WebRequest -Uri $candidate -Method Head -UseBasicParsing -ErrorAction Stop
                    if ($head.StatusCode -eq 200) {
                        $resolvedUrl = $candidate
                        break
                    }
                } catch {
                    # 404 / DNS / TLS - try next candidate.
                    continue
                }
            }
            if ($resolvedUrl) {
                try {
                    $sqliteZip = Join-Path $env:TEMP 'sqlite-tools.zip'
                    $sqliteDir = Join-Path $env:TEMP 'sqlite-tools'
                    Invoke-WebRequest -Uri $resolvedUrl -OutFile $sqliteZip -UseBasicParsing -ErrorAction Stop
                    if (Test-Path $sqliteDir) { Remove-Item $sqliteDir -Recurse -Force }
                    Expand-Archive -Path $sqliteZip -DestinationPath $sqliteDir -Force
                    $sqliteBin = Get-ChildItem -Path $sqliteDir -Recurse -Filter sqlite3.exe -ErrorAction SilentlyContinue | Select-Object -First 1
                    if ($sqliteBin) {
                        $bin = Join-Path $env:USERPROFILE '.mneme\bin'
                        if (-not (Test-Path $bin)) { New-Item -ItemType Directory -Path $bin -Force | Out-Null }
                        Copy-Item $sqliteBin.FullName (Join-Path $bin 'sqlite3.exe') -Force
                        Write-OK "[G7] sqlite3.exe installed to ~/.mneme/bin/"
                        $sqliteInstalled = $true
                    } else {
                        Write-Warn "[G7] sqlite3.exe not found inside downloaded zip"
                    }
                } catch {
                    Write-Warn ("[G7] SQLite portable install failed: {0}" -f $_.Exception.Message)
                }
            } else {
                Write-Warn "[G7] no working sqlite.org URL found - sqlite3 is optional, install manually if you need shard diagnostics"
            }
        }

        if (-not $sqliteInstalled) {
            Write-Warn "[G7] SQLite CLI install skipped - sqlite3 is optional. Install manually:  winget install --id SQLite.SQLite"
        }
    } else {
        $sqliteCmd = Get-Command sqlite3 -ErrorAction SilentlyContinue
        if ($sqliteCmd) {
            $sqliteProbe = Invoke-NativeProbe -ExePath $sqliteCmd.Source -ProbeArgs @('-version')
            if ($sqliteProbe.Success) {
                Write-OK ("[G7] SQLite present: " + ([string]$sqliteProbe.Output).Trim())
            } else {
                Write-OK "[G7] SQLite present"
            }
        } else {
            Write-OK "[G7] SQLite present"
        }
    }

    # G9 + G10: optional multimodal native deps (Tesseract OCR + ImageMagick).
    # Only installed when the caller passes `-WithMultimodal`. Without these
    # the shipped `mneme-multimodal` binary still indexes images for
    # dimensions + EXIF, but cannot OCR text and cannot do image format
    # conversion. Every install is wrapped in try/catch with a non-fatal
    # warning on failure - the rest of install.ps1 always proceeds.
    if ($WithMultimodal) {
        $wingetExe = Get-Command winget -ErrorAction SilentlyContinue

        # G9: Tesseract OCR (UB-Mannheim build is the de-facto Windows
        # distribution; ships libtesseract + leptonica + traineddata).
        $tessCmd = Get-Command tesseract -ErrorAction SilentlyContinue
        if ($tessCmd) {
            $tessProbe = Invoke-NativeProbe -ExePath $tessCmd.Source -ProbeArgs @('--version')
            if ($tessProbe.Success) {
                $tessOut = $tessProbe.Output
                $firstLine = if ($tessOut -is [array]) { $tessOut[0] } else { $tessOut }
                Write-OK ("[G9] Tesseract OCR present: " + ([string]$firstLine).Trim())
            } else {
                Write-OK "[G9] Tesseract OCR present"
            }
        } else {
            Write-Info "[G9] Tesseract OCR missing - installing UB-Mannheim.TesseractOCR via winget"
            try {
                if ($wingetExe) {
                    $p = Start-Process winget -ArgumentList 'install','--id','UB-Mannheim.TesseractOCR','--silent','--accept-source-agreements','--accept-package-agreements' -Wait -PassThru
                    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq -1978335189) {
                        # B8: refresh PATH so the immediate capability check picks up the new install
                        $machinePath = [Environment]::GetEnvironmentVariable("PATH", "Machine")
                        $userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
                        $env:PATH = "$machinePath;$userPath"
                        # A7-005 (2026-05-04): the Machine PATH update from winget
                        # propagates via a registry broadcast that the current
                        # process never receives, so even the refresh above can
                        # miss the just-added Tesseract entry. Probe the default
                        # install location directly and inject it into the
                        # current process PATH so the immediate G9 capability
                        # check + the subsequent register-mcp child process
                        # (which inherits this env) both see Tesseract.
                        if (-not (Get-Command tesseract -ErrorAction SilentlyContinue)) {
                            $tessDefault = Join-Path $env:ProgramFiles 'Tesseract-OCR\tesseract.exe'
                            if (Test-Path $tessDefault) {
                                $tessDir = Split-Path -Parent $tessDefault
                                if (-not ($env:PATH.Split(';') -contains $tessDir)) {
                                    $env:PATH = "$env:PATH;$tessDir"
                                }
                                [Environment]::SetEnvironmentVariable('PATH', $env:PATH, 'Process')
                                Write-OK ("[G9] Tesseract OCR installed at {0} (PATH session-injected)" -f $tessDefault)
                            } else {
                                # B6: literal "Tesseract" (was `tesseract` -- backtick-t was a PowerShell tab escape that ate the T)
                                Write-OK "[G9] Tesseract OCR installed (re-open shell if Tesseract not on PATH yet)"
                            }
                        } else {
                            Write-OK "[G9] Tesseract OCR installed (PATH refresh picked up new install)"
                        }
                    } else {
                        Write-Warn ("[G9] winget Tesseract install exited {0} - non-fatal, continuing" -f $p.ExitCode)
                    }
                } else {
                    Write-Warn "[G9] winget not present - install Tesseract manually from https://github.com/UB-Mannheim/tesseract/wiki"
                }
            } catch {
                Write-Warn ("[G9] Tesseract install failed (non-fatal): {0}" -f $_.Exception.Message)
            }
        }

        # G10: ImageMagick (image format conversion + thumbnail generation
        # for the multimodal pipeline).
        $magickCmd = Get-Command magick -ErrorAction SilentlyContinue
        if ($magickCmd) {
            $magickProbe = Invoke-NativeProbe -ExePath $magickCmd.Source -ProbeArgs @('-version')
            if ($magickProbe.Success) {
                $magickOut = $magickProbe.Output
                $firstLine = if ($magickOut -is [array]) { $magickOut[0] } else { $magickOut }
                Write-OK ("[G10] ImageMagick present: " + ([string]$firstLine).Trim())
            } else {
                Write-OK "[G10] ImageMagick present"
            }
        } else {
            Write-Info "[G10] ImageMagick missing - installing ImageMagick.ImageMagick via winget"
            try {
                if ($wingetExe) {
                    $p = Start-Process winget -ArgumentList 'install','--id','ImageMagick.ImageMagick','--silent','--accept-source-agreements','--accept-package-agreements' -Wait -PassThru
                    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq -1978335189) {
                        $env:PATH = [Environment]::GetEnvironmentVariable('PATH','Machine') + ';' + [Environment]::GetEnvironmentVariable('PATH','User')
                        Write-OK "[G10] ImageMagick installed (re-open shell if `magick` not on PATH yet)"
                    } else {
                        Write-Warn ("[G10] winget ImageMagick install exited {0} - non-fatal, continuing" -f $p.ExitCode)
                    }
                } else {
                    Write-Warn "[G10] winget not present - install ImageMagick manually from https://imagemagick.org/script/download.php#windows"
                }
            } catch {
                Write-Warn ("[G10] ImageMagick install failed (non-fatal): {0}" -f $_.Exception.Message)
            }
        }
    } else {
        Write-Info "[G9/G10] -WithMultimodal not set - skipping Tesseract OCR + ImageMagick auto-install"
        Write-Info "         pass -WithMultimodal to install these via winget for OCR + image conversion"
    }
}

# ============================================================================
# Step 2 - Fetch latest release metadata (skipped under -LocalZip / -SkipDownload)
# ============================================================================
#
# Three sources are supported:
#   default        : fetch latest release metadata from GitHub, then download
#                    the asset in step 3/8.
#   -LocalZip      : skip the GitHub API call. Use the caller-supplied zip
#                    in step 3/8. tag_name is reported as 'local-zip'.
#   -SkipDownload  : skip BOTH step 2 and step 3 entirely. Assume the user
#                    has already extracted the zip into ~/.mneme/. We just
#                    sanity-check that mneme.exe is present before moving
#                    on to step 4/8.
#
# The variables consumed downstream are:
#   $UseLocalZip / $UsePreExtracted - mode flags (set above the banner)
#   $LocalZipPath                   - resolved absolute path (LocalZip mode)
#   $ReleaseTag                     - human-readable tag for banner / manifest
#   $AssetEntry                     - GitHub asset metadata (default mode only)

$UseLocalZip     = [bool]$LocalZip
$UsePreExtracted = [bool]$SkipDownload
$LocalZipPath    = $LocalZip
$AssetEntry      = $null
$Headers         = @{ 'User-Agent' = 'mneme-installer' }

if ($UsePreExtracted) {
    Write-Step "step 2/8 - SKIPPED (-SkipDownload set; using existing ~/.mneme contents)"
    Write-Info "no GitHub API call, no download. install.ps1 will verify ~/.mneme/bin/mneme.exe in step 3/8."
    $ReleaseTag = 'pre-extracted'
} elseif ($UseLocalZip) {
    Write-Step "step 2/8 - SKIPPED (-LocalZip set; using local archive)"
    try {
        $zipBytes = (Get-Item -LiteralPath $LocalZipPath).Length
        Write-OK ("using local zip {0} ({1:N1} MB)" -f $LocalZipPath, ($zipBytes / 1MB))
    } catch {
        Write-OK ("using local zip {0}" -f $LocalZipPath)
    }
    $ReleaseTag = 'local-zip'
} else {
    Write-Step "step 2/8 - fetching latest release metadata"

    $ApiUrl = "https://api.github.com/repos/$Repo/releases/latest"

    try {
        $Release = Invoke-RestMethod -Uri $ApiUrl -Headers $Headers
    } catch {
        Write-Fail ("GitHub API unreachable: {0}" -f $_.Exception.Message)
        exit 1
    }

    $AssetEntry = $Release.assets | Where-Object { $_.name -eq $Asset } | Select-Object -First 1
    if ($null -eq $AssetEntry) {
        Write-Warn ("{0} not yet attached to release {1}" -f $Asset, $Release.tag_name)
        Write-Warn "       the release workflow may still be building - retry in ~15 min."
        exit 1
    }
    Write-OK ("release {0} - asset {1} ({2:N1} MB)" -f $Release.tag_name, $Asset, ($AssetEntry.size / 1MB))
    $ReleaseTag = $Release.tag_name
}

# ============================================================================
# Step 3 - Download + extract (or verify pre-extracted layout)
# ============================================================================

# NEW-001: manifest tracking helper. Hoisted out of step 3 so the
# pre-extracted branch can still write a fresh manifest after verifying
# layout. Skips $ManifestFile from its own diff set.
function Get-MnemeManifest {
    param([string]$Root)
    if (-not (Test-Path $Root)) { return @() }
    Get-ChildItem -Path $Root -Recurse -File -Force -ErrorAction SilentlyContinue |
        ForEach-Object {
            $rel = $_.FullName.Substring($Root.Length).TrimStart('\','/')
            if ($rel -ne '.install-manifest.json') { $rel }
        }
}

if ($UsePreExtracted) {
    Write-Step "step 3/8 - SKIPPED (-SkipDownload set; verifying existing extraction)"

    # The whole point of -SkipDownload is that the user has already put a
    # zip's contents into ~/.mneme. If mneme.exe is missing the rest of
    # the install is doomed (step 6 daemon start, step 7 register-mcp both
    # call $MnemeBin). Bail loudly with a remediation hint rather than
    # produce a half-installed silent failure later.
    $MnemeExePath = Join-Path $BinDir 'mneme.exe'
    if (-not (Test-Path -LiteralPath $MnemeExePath)) {
        Write-Fail ("-SkipDownload was set but {0} does not exist" -f $MnemeExePath)
        Write-Fail ("       expected layout under {0}:" -f $MnemeHome)
        Write-Fail  "         bin\mneme.exe"
        Write-Fail  "         bin\mneme-daemon.exe (and 6 worker exes)"
        Write-Fail  "         mcp\, plugin\, static\vision\..."
        Write-Fail  "       remediation:"
        Write-Fail  "         1. extract the mneme zip into ~/.mneme  (Expand-Archive)"
        Write-Fail  "         2. re-run install.ps1 -SkipDownload"
        Write-Fail  "       OR drop -SkipDownload to fetch from GitHub Releases."
        Write-Fail  "       OR pass -LocalZip <path> to extract a local zip from this script."
        exit 1
    }

    # A7-021 (2026-05-04): the narrow mneme.exe-only guard above gives a
    # nice remediation message, but doesn't catch the case where mneme.exe
    # is present but the 7 worker exes are missing (e.g. user extracted a
    # damaged or partial zip into ~/.mneme). Run the full 8-binary check
    # here too so -SkipDownload mode gets the same hard-fail floor as the
    # extract-fresh branch. The unconditional check at the bottom of step
    # 3/8 still runs (defensive double-check), but failing here lets us
    # exit BEFORE the manifest-write step records a corrupt baseline.
    $SdMissing = @()
    foreach ($bin in @('mneme.exe', 'mneme-daemon.exe', 'mneme-store.exe',
                       'mneme-parsers.exe', 'mneme-scanners.exe',
                       'mneme-livebus.exe', 'mneme-md-ingest.exe',
                       'mneme-brain.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $BinDir $bin))) {
            $SdMissing += $bin
        }
    }
    if ($SdMissing.Count -gt 0) {
        Write-Fail ("-SkipDownload verification failed -- missing binaries:")
        foreach ($m in $SdMissing) { Write-Fail ("  - bin\{0}" -f $m) }
        Write-Fail "remediation: re-extract the mneme zip into ~/.mneme  (Expand-Archive)"
        exit 1
    }
    Write-OK ("mneme.exe present at {0}; all 8 core binaries verified; extraction skipped" -f $MnemeExePath)

    # Refresh the manifest so a future upgrade can still diff. We don't
    # try to detect orphans (we never extracted anything to compare against).
    $NewManifest = Get-MnemeManifest -Root $MnemeHome
    try {
        $manifestPayload = @{
            version   = $ReleaseTag
            installed = (Get-Date).ToUniversalTime().ToString('o')
            mode      = 'pre-extracted'
            files     = $NewManifest
        } | ConvertTo-Json -Depth 4
        # A7-016 (2026-05-04): atomic manifest write via .tmp + Move-Item.
        # Direct Set-Content on the final filename leaves the manifest as
        # corrupt JSON if power dies mid-write, breaking `mneme doctor` and
        # the upgrade-time delta diff. Move-Item is atomic on NTFS so the
        # final file either has the new content or the old content -- never
        # a half-written mix.
        $ManifestTmp = $ManifestFile + '.tmp'
        Set-Content -LiteralPath $ManifestTmp -Value $manifestPayload -Encoding UTF8 -Force
        Move-Item -LiteralPath $ManifestTmp -Destination $ManifestFile -Force
        Write-OK ("manifest written: {0} ({1} file(s))" -f $ManifestFile, $NewManifest.Count)
    } catch {
        Write-Warn ("could not write install manifest: {0}" -f $_.Exception.Message)
    }
} else {
    Write-Step "step 3/8 - downloading + extracting"

    $Tmp = Join-Path $env:TEMP ("mneme-install-{0}" -f ([System.Guid]::NewGuid().ToString('N').Substring(0, 8)))
    New-Item -ItemType Directory -Path $Tmp -Force | Out-Null

    if ($UseLocalZip) {
        # Local-zip mode: reference the caller-supplied path directly. We
        # don't copy it into $Tmp because there's nothing to clean up
        # after extract - the source is the user's file, not ours.
        $ZipPath = $LocalZipPath
        Write-Info ("source: {0} (local, no download)" -f $ZipPath)
    } else {
        $ZipPath = Join-Path $Tmp $Asset
        try {
            Invoke-WebRequest -Uri $AssetEntry.browser_download_url -OutFile $ZipPath -UseBasicParsing -Headers $Headers
        } catch {
            Write-Fail ("download failed: {0}" -f $_.Exception.Message)
            Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
            exit 1
        }
    }

    if (-not (Test-Path $MnemeHome)) {
        New-Item -ItemType Directory -Path $MnemeHome -Force | Out-Null
    }

    $OldManifest = @()
    if ($IsUpgrade) {
        Write-Info "upgrade detected  -  snapshotting existing manifest before extract"
        $OldManifest = Get-MnemeManifest -Root $MnemeHome
    } else {
        Write-Info "fresh install  -  no prior manifest"
    }

    # ----------------------------------------------------------------------
    # AGGRESSIVE CLEAN-STALE -- wipe code dirs the release zip OWNS so
    # leftover files from prior versions can't survive an upgrade.
    # ----------------------------------------------------------------------
    # User data is preserved (projects/, snapshots/, models/, logs/,
    # run/, install-receipts/, meta.db). Without this step, files
    # present in v0.3.0 but absent in v0.3.2 (or vice versa) can stick
    # around and confuse the daemon -- exactly the "stale shit bugs us"
    # symptom users flagged.
    $StaleCodeDirs = @('bin', 'mcp', 'scripts', 'plugin', 'static')
    $cleanedAny = $false
    foreach ($d in $StaleCodeDirs) {
        $abs = Join-Path $MnemeHome $d
        if (Test-Path -LiteralPath $abs) {
            try {
                Remove-Item -LiteralPath $abs -Recurse -Force -ErrorAction Stop
                Write-Info ("clean-stale: wiped {0}\" -f $d)
                $cleanedAny = $true
            } catch {
                Write-Warn ("clean-stale: could not remove {0}: {1}" -f $abs, $_.Exception.Message)
            }
        }
    }
    # Also drop the previous install-manifest so a corrupted one can't
    # mislead the orphan-cleanup pass below.
    $oldManifestFile = Join-Path $MnemeHome '.install-manifest.json'
    if (Test-Path -LiteralPath $oldManifestFile) {
        Remove-Item -LiteralPath $oldManifestFile -Force -ErrorAction SilentlyContinue
    }
    if ($cleanedAny) {
        Write-OK "clean-stale: code dirs wiped (user data preserved: projects/, snapshots/, models/, logs/, run/)"
    }

    try {
        Expand-Archive -Path $ZipPath -DestinationPath $MnemeHome -Force
    } catch {
        Write-Fail ("extract failed: {0}" -f $_.Exception.Message)
        Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
        exit 1
    }

    # NEW-001: orphan removal pass.
    $NewManifest = Get-MnemeManifest -Root $MnemeHome
    if ($IsUpgrade -and $OldManifest.Count -gt 0) {
        $NewSet = @{}
        foreach ($n in $NewManifest) { $NewSet[$n] = $true }
        $Orphans = @($OldManifest | Where-Object { -not $NewSet.ContainsKey($_) })
        if ($Orphans.Count -gt 0) {
            Write-Info ("orphan-cleanup: {0} file(s) from prior install no longer in zip" -f $Orphans.Count)
            foreach ($o in $Orphans) {
                $abs = Join-Path $MnemeHome $o
                # B4: Test-Path before Remove-Item so files already gone
                # (e.g. cleaned by a prior partial run, or never extracted
                # in the first place) don't trigger a "could not remove
                # orphan" warning. The end-of-loop "removed N orphan(s)"
                # summary line still prints — it's the per-file warnings
                # that were noisy (41 fired on a typical AWS upgrade).
                if (Test-Path -LiteralPath $abs) {
                    try {
                        Remove-Item -LiteralPath $abs -Force -ErrorAction SilentlyContinue
                    } catch {
                        Write-Warn ("could not remove orphan {0}: {1}" -f $abs, $_.Exception.Message)
                    }
                }
            }
            Write-OK ("removed {0} orphan file(s)" -f $Orphans.Count)
        } else {
            Write-OK "no orphan files from prior install"
        }
    }

    # Persist the manifest so the next upgrade can repeat the diff.
    try {
        $manifestPayload = @{
            version   = $ReleaseTag
            installed = (Get-Date).ToUniversalTime().ToString('o')
            mode      = if ($IsUpgrade) { 'upgrade' } elseif ($UseLocalZip) { 'local-zip' } else { 'fresh-install' }
            files     = $NewManifest
        } | ConvertTo-Json -Depth 4
        # A7-016 (2026-05-04): atomic write via .tmp + Move-Item; see the
        # pre-extracted branch above for full rationale.
        $ManifestTmp = $ManifestFile + '.tmp'
        Set-Content -LiteralPath $ManifestTmp -Value $manifestPayload -Encoding UTF8 -Force
        Move-Item -LiteralPath $ManifestTmp -Destination $ManifestFile -Force
        Write-OK ("manifest written: {0} ({1} file(s))" -f $ManifestFile, $NewManifest.Count)
    } catch {
        Write-Warn ("could not write install manifest: {0}" -f $_.Exception.Message)
    }

    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
    Write-OK ("extracted to {0}" -f $MnemeHome)
}

# NEW-007: post-extract verification.
# Hard fail if any expected core binary is missing. The release pipeline
# (NEW-006) is now release-blocking on these too, but a corrupted /
# truncated download still has to surface here, before we point Claude
# Code at a half-installed bin dir.
$ExpectedBinaries = @('mneme.exe', 'mneme-daemon.exe', 'mneme-store.exe',
                      'mneme-parsers.exe', 'mneme-scanners.exe',
                      'mneme-livebus.exe', 'mneme-md-ingest.exe',
                      'mneme-brain.exe')
$MissingBinaries = @()
foreach ($bin in $ExpectedBinaries) {
    $abs = Join-Path $BinDir $bin
    if (-not (Test-Path -LiteralPath $abs)) {
        $MissingBinaries += $bin
    }
}
if ($MissingBinaries.Count -gt 0) {
    Write-Fail ("post-extract verification failed  -  missing binaries:")
    foreach ($m in $MissingBinaries) { Write-Fail ("  - {0}" -f $m) }
    Write-Fail "this is a hard install error  -  the release zip is incomplete or corrupted."
    Write-Fail "please re-download or open an issue at https://github.com/$Repo/issues"
    exit 1
}
Write-OK ("post-extract verification: all {0} core binaries present" -f $ExpectedBinaries.Count)

# Mneme OS branding alias: expose `mnemeos.exe` alongside `mneme.exe` in
# the bin dir so users on the new canonical brand name get the same
# binary. Hard link is preferred (no extra disk) but falls back to a
# copy on filesystems that don't support it. Idempotent: removes any
# stale alias before re-creating.
$MnemeExe   = Join-Path $BinDir 'mneme.exe'
$MnemeosExe = Join-Path $BinDir 'mnemeos.exe'
if (Test-Path -LiteralPath $MnemeExe) {
    if (Test-Path -LiteralPath $MnemeosExe) {
        Remove-Item -LiteralPath $MnemeosExe -Force -ErrorAction SilentlyContinue
    }
    try {
        New-Item -ItemType HardLink -Path $MnemeosExe -Value $MnemeExe -ErrorAction Stop | Out-Null
        Write-OK "Mneme OS alias: mnemeos.exe -> mneme.exe (hard link)"
    } catch {
        try {
            Copy-Item -LiteralPath $MnemeExe -Destination $MnemeosExe -Force -ErrorAction Stop
            Write-OK "Mneme OS alias: mnemeos.exe -> mneme.exe (copy fallback)"
        } catch {
            Write-Warn ("could not create mnemeos.exe alias: {0}" -f $_.Exception.Message)
        }
    }
}

# F1 D1: verify the Vision SPA static bundle landed at the canonical
# production layout the daemon expects (~/.mneme/static/vision/index.html).
# The daemon's tower-http ServeDir mount in supervisor/src/health.rs
# resolves `<MNEME_HOME>/static/vision/` before falling back to the
# in-repo dev path. Missing static dir is non-fatal (daemon logs a
# warning and continues with API-only endpoints), but `mneme view` /
# the browser fallback at http://127.0.0.1:7777/ would 404. We surface
# the gap loudly so the user knows the visual layer is unavailable.
$VisionStaticDir   = Join-Path $MnemeHome 'static\vision'
$VisionIndexFile   = Join-Path $VisionStaticDir 'index.html'
$VisionAssetsDir   = Join-Path $VisionStaticDir 'assets'
if (-not (Test-Path -LiteralPath $VisionIndexFile)) {
    Write-Warn "vision SPA missing: $VisionIndexFile not found"
    Write-Warn "  the daemon will start API-only; http://127.0.0.1:7777/ will 404."
    Write-Warn "  this means the release zip was built without the vision/dist payload."
    Write-Warn "  open an issue at https://github.com/$Repo/issues citing 'A12 / vision/dist missing'."
} elseif (-not (Test-Path -LiteralPath $VisionAssetsDir)) {
    Write-Warn "vision SPA index.html present but assets/ missing at $VisionAssetsDir"
    Write-Warn "  the SPA will load index.html but every chunk will 404."
} else {
    $assetCount = (Get-ChildItem -LiteralPath $VisionAssetsDir -File -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-OK ("vision SPA staged at {0} ({1} asset file(s))" -f $VisionStaticDir, $assetCount)
}

# ============================================================================
# Step 4 - Windows Defender exclusions
# ============================================================================
#
# Defender's heuristic ML classifier may false-positive on mneme's
# memory files because they contain dense agent-automation patterns
# ("hook", "pre-tool", "blocked", "inject", "subprocess", "exec"). Without
# an exclusion, random mneme data files will be silently quarantined,
# which looks like mysterious data loss to the user. (A7-010: avoiding
# specific classifier-family names since Defender renames + retires
# them across monthly signature updates.)
#
# This step attempts to add exclusions via Add-MpPreference. Requires
# admin. If not elevated, we print the exact one-liner the user can run
# from an elevated shell later.

Write-Step "step 4/8 - Windows Defender exclusions"

$ExcludeDirs = @($MnemeHome, $ClaudeHome)
$DefenderFailed = $false

# Idempotent-3: pre-check existing exclusions BEFORE attempting Add-MpPreference.
# Add-MpPreference itself is idempotent (won't add dupes), but on re-runs it
# still calls into Windows Defender's CIM bridge, which on a non-elevated
# shell prints "Run as admin" every single time even when the exclusion is
# already set. We pre-check via Get-MpPreference and silently skip if the
# exclusion is already present.

function Get-DefenderExclusions {
    # Returns an array of currently-set ExclusionPath entries, or $null
    # on error (e.g. Defender service unavailable). NEVER throws.
    try {
        $prefs = Get-MpPreference -ErrorAction Stop
        if ($null -eq $prefs.ExclusionPath) { return @() }
        return @($prefs.ExclusionPath)
    } catch {
        return $null
    }
}

# Normalize paths so equality survives trailing-slash / case differences.
function Get-NormalizedPath {
    param([string]$Path)
    if (-not $Path) { return '' }
    return ($Path -replace '\\+$', '').ToLowerInvariant()
}

$ExistingExclusions = Get-DefenderExclusions
$ExistingNormalized = @()
if ($null -ne $ExistingExclusions) {
    $ExistingNormalized = $ExistingExclusions | ForEach-Object { Get-NormalizedPath $_ }
}

# Compute which dirs still need to be added.
$DirsToAdd = @()
foreach ($dir in $ExcludeDirs) {
    $normDir = Get-NormalizedPath $dir
    if ($ExistingNormalized -contains $normDir) {
        Write-OK ("already excluded: {0}" -f $dir)
    } else {
        $DirsToAdd += $dir
    }
}

if ($DirsToAdd.Count -eq 0) {
    Write-OK "all Defender exclusions already in place"
} elseif (Test-IsElevated) {
    foreach ($dir in $DirsToAdd) {
        try {
            Add-MpPreference -ExclusionPath $dir -ErrorAction Stop
            Write-OK ("excluded {0}" -f $dir)
        } catch {
            Write-Warn ("could not exclude {0}: {1}" -f $dir, $_.Exception.Message)
            $DefenderFailed = $true
        }
    }
} else {
    # Idempotent-3: only emit the admin warning when there's REAL work
    # to do. On a re-run with all exclusions already set we already
    # printed "all Defender exclusions already in place" and skipped this
    # branch entirely.
    Write-Warn "not running elevated - cannot add Defender exclusions"
    $DefenderFailed = $true
}

if ($DefenderFailed) {
    # A7-007 (2026-05-04): collapsed from a 10-line block (3 blank lines,
    # 2 prose paragraphs, the SAgent.HAG!MTB rant) to 2 lines + the
    # commands. The full explanation lives in mneme doctor --strict and
    # the README; the install log just needs to point users at the fix.
    # A7-010 (2026-05-04): dropped the specific "SAgent.HAG!MTB" name
    # from user-facing strings -- that classifier may be renamed/retired
    # by Defender, and the underlying behaviour (heuristic ML false
    # positive on agent-automation patterns) is what users care about.
    Write-Host "    not elevated - run this in admin PowerShell to add Defender exclusions:" -ForegroundColor Yellow
    foreach ($dir in $DirsToAdd) {
        Write-Host ("      Add-MpPreference -ExclusionPath `"$dir`"") -ForegroundColor White
    }
}

# ============================================================================
# Step 5 - Add bin dir to user PATH (PREPEND so real mneme always wins)
# ============================================================================
#
# 2026-05-04 hardening (B-PATH): PREPEND `~/.mneme/bin` (not append) so
# the real mneme binary always resolves first. Also detect any non-mneme
# `mneme.exe` stub on PATH (e.g. an unrelated PyPI `mneme` package by
# Risto Stevcev installs a ~100 KB Python entry-point launcher to
# `<Python>/Scripts/mneme.exe` that intercepts every `mneme` invocation).
# Without this defense, hooks fired by Claude Code call the wrong binary,
# every action returns an error, and the install appears broken on
# machines that happen to have a foreign `mneme` PyPI package installed.

Write-Step "step 5/8 - updating user PATH"

# Detect impostor `mneme.exe` already on PATH that is NOT our binary.
# Heuristic: real mneme.exe is ~50 MB Rust binary; Python entry-point
# launchers are 100-200 KB stubs. Any `mneme.exe` smaller than 1 MB AND
# not in our `$BinDir` is almost certainly a foreign package's entry point.
$impostors = @()
foreach ($p in ($env:PATH -split ';')) {
    if ([string]::IsNullOrWhiteSpace($p)) { continue }
    $candidate = Join-Path $p 'mneme.exe'
    if ((Test-Path $candidate -PathType Leaf) -and ($candidate -ne (Join-Path $BinDir 'mneme.exe'))) {
        try {
            $sz = (Get-Item $candidate -ErrorAction Stop).Length
            if ($sz -lt 1MB) { $impostors += @{ Path = $candidate; Size = $sz } }
        } catch { }
    }
}
if ($impostors.Count -gt 0) {
    Write-Warn "detected non-mneme mneme.exe stub(s) on PATH (would intercept hook calls):"
    foreach ($i in $impostors) {
        $kb = [math]::Round($i.Size / 1KB, 1)
        Write-Host ("    {0} ({1} KB)" -f $i.Path, $kb) -ForegroundColor Yellow
    }
    Write-Host "    likely from an unrelated PyPI 'mneme' package (e.g. flask-mneme)." -ForegroundColor Yellow
    Write-Host "    PATH is being prepended below so real mneme wins resolution." -ForegroundColor Yellow
    Write-Host "    Optional cleanup if you do not need the foreign package:" -ForegroundColor Yellow
    Write-Host "      pip uninstall -y mneme" -ForegroundColor Yellow
    Write-Host "      Remove-Item <path-above> -Force   # if file persists after pip uninstall" -ForegroundColor Yellow
}

$UserPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($null -eq $UserPath) { $UserPath = '' }

# PREPEND (not append) so $BinDir wins over any other PATH entry that
# also has a mneme.exe (Python Scripts dirs, conda envs, etc.).
# Idempotent: if $BinDir already appears anywhere in PATH, we strip the
# old position and re-insert at the front.
#
# A7-015 (2026-05-04, KNOWN-LIMITATION, deferred to v0.3.3+):
# `[Environment]::SetEnvironmentVariable(..., 'User')` is a non-atomic
# read-modify-write on HKCU\Environment\Path. If a parallel installer
# (winget, MSI, another `install.ps1`, or another PowerShell session
# the user opened) updates User PATH between the GetEnvironmentVariable
# at line 1441 above and the SetEnvironmentVariable below, one of the
# two updates is silently overwritten. Concretely: install.ps1 invokes
# `winget install` for Tesseract / Python / SQLite which themselves
# update User PATH; on a slow machine those updates can race with this
# block. Mitigation considered (NOT shipped in v0.3.2):
#   1. Re-read User PATH and abort if it changed mid-block.
#   2. Use RegistryChangeNotification + retry.
# Both are correct fixes but require careful interaction with the
# registry-broadcast WM_SETTINGCHANGE flow that downstream code expects.
# Documented here so a future maintainer doesn't trip on it silently.
$existingSegments = $UserPath.Split(';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and $_ -ne $BinDir }
$NewPath = (@($BinDir) + $existingSegments) -join ';'
if ($NewPath -ne $UserPath) {
    [Environment]::SetEnvironmentVariable('PATH', $NewPath, 'User')
    Write-OK ("prepended {0} to user PATH (real mneme wins resolution)" -f $BinDir)
} else {
    Write-OK "bin already at front of PATH"
}
# Also rewrite session PATH so the rest of this install run sees the new order.
$sessionSegments = $env:PATH.Split(';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and $_ -ne $BinDir }
$env:PATH = (@($BinDir) + $sessionSegments) -join ';'

# ============================================================================
# Step 5b - Clear Bun's install cache to prevent stale-bytecode failures
# ============================================================================
#
# Project finding 2026-04-26: on EC2 we hit `SyntaxError: Export named '$ZodTuple'
# not found in module 'zod/v4/core/schemas.js'` even with a fresh
# bun install + identical zod version + identical schemas.js SHA256.
# Clearing `~/.bun/install/cache` and `%LOCALAPPDATA%/Bun/Cache` resolved
# it instantly. Conclusion: Bun cached compiled bytecode from a prior
# bun version that didn't know about new zod exports. Clear at install
# time so a fresh user never hits this.
#
# Idempotent-2: the global wipe is too aggressive for users with OTHER
# Bun projects on the same machine - we kill their caches too. Bun's
# install cache is keyed by content-hash (no per-package scoping API
# we can hook into), so true scope-narrowing isn't feasible. Instead
# we make the wipe explicit:
#
#   - `-NoBunCacheClear`         -> always skip (dev / debug, unchanged)
#   - `-ForceBunCacheClear`      -> always wipe (CI, headless, "I don't care")
#   - interactive shell, no flag -> prompt the user
#   - unattended (LocalZip /     -> default to skip; print the one-liner
#      non-interactive, no flag)    they can run later if MCP fails

function Test-IsInteractiveSession {
    # Heuristic: if stdin is a real console AND we're not running with
    # NonInteractive switch AND we're not in a CI env. Returns $true when
    # we can safely block on Read-Host without hanging an automation pipe.
    if ([Environment]::UserInteractive -eq $false) { return $false }
    if ($Host.Name -eq 'ServerRemoteHost') { return $false }
    if ($env:CI -or $env:GITHUB_ACTIONS -or $env:BUILDKITE -or $env:JENKINS_HOME) {
        return $false
    }
    # iwr | iex piping breaks UserInteractive in some shells. Final guard:
    # if [Console]::IsInputRedirected is $true, stdin is piped and we
    # cannot prompt.
    try {
        if ([Console]::IsInputRedirected) { return $false }
    } catch { return $false }
    return $true
}

# Detect non-mneme bun cache contents BEFORE we decide to wipe. If the
# cache only contains mneme's own deps (or is empty), wiping is safe.
function Test-BunCacheHasOtherProjects {
    foreach ($p in @(
        (Join-Path $env:USERPROFILE '.bun\install\cache'),
        (Join-Path $env:LOCALAPPDATA 'Bun\Cache')
    )) {
        if (-not (Test-Path $p)) { continue }
        # Cheap heuristic: any subdir whose name doesn't reference mneme
        # is a "foreign" cached package. Bun caches under
        # `<cache>/<package>@<version>/` so the immediate child names
        # are package names. We treat ANY child as foreign - Bun never
        # caches mneme itself there (mneme is a Rust binary, not an npm
        # package), so any cached package belongs to another project.
        try {
            $children = Get-ChildItem -LiteralPath $p -Force -ErrorAction SilentlyContinue
            if ($children -and $children.Count -gt 0) { return $true }
        } catch { continue }
    }
    return $false
}

if (-not $NoBunCacheClear) {
    Write-Step "step 5b/8 - install MCP node_modules (bun install --frozen-lockfile)"

    # The unattended path: LocalZip means scripted ship, the iwr|iex pipe
    # means Read-Host won't work. Default to NOT wiping - but skip the
    # prompt if the user already passed -ForceBunCacheClear.
    $isUnattended = ($LocalZip -ne $null -and $LocalZip -ne '') -or `
                    -not (Test-IsInteractiveSession)

    $shouldWipe = $false
    if ($ForceBunCacheClear) {
        $shouldWipe = $true
        Write-Info "explicit -ForceBunCacheClear flag set - will wipe Bun cache"
    } elseif (-not (Test-BunCacheHasOtherProjects)) {
        # Cache is empty or has no foreign packages - safe to wipe (or
        # nothing to wipe). Proceed without prompting.
        $shouldWipe = $true
    } elseif ($isUnattended) {
        # Foreign packages present + unattended host = SKIP. Spare other
        # projects. Print the one-liner the user can run later.
        Write-Warn "non-mneme Bun cache entries detected - leaving them in place"
        Write-Warn "(unattended install: pass -ForceBunCacheClear to wipe anyway)"
        Write-Host ""
        Write-Host "    If MCP later fails with 'SyntaxError: Export named ...':" -ForegroundColor Yellow
        Write-Host "      Remove-Item -Recurse -Force `"$(Join-Path $env:USERPROFILE '.bun\install\cache')`"" -ForegroundColor White
        Write-Host "      Remove-Item -Recurse -Force `"$(Join-Path $env:LOCALAPPDATA 'Bun\Cache')`"" -ForegroundColor White
        Write-Host ""
        $shouldWipe = $false
    } elseif ($PromptForBunCacheClear) {
        # A7-012 (2026-05-04): zero-question install design principle --
        # the interactive prompt now requires an explicit opt-in flag.
        # Without -PromptForBunCacheClear, fall through to the
        # skip-with-mitigation branch below, matching what the unattended
        # path already does. This keeps the install non-blocking for the
        # 99% case (clean install / fresh Bun cache) and only surfaces
        # the choice for users who specifically asked to be asked.
        Write-Host ""
        Write-Host "    Bun's install cache contains entries from OTHER projects." -ForegroundColor Yellow
        Write-Host "    Wiping prevents stale-bytecode failures in mneme's MCP," -ForegroundColor Yellow
        Write-Host "    but it ALSO forces other Bun projects to re-download deps." -ForegroundColor Yellow
        Write-Host ""
        $resp = Read-Host "    Wipe Bun cache? [y/N]"
        $shouldWipe = ($resp -match '^(y|yes)$')
        if (-not $shouldWipe) {
            Write-Info "skipped - pass -ForceBunCacheClear later if MCP fails"
        }
    } else {
        # Default (no -PromptForBunCacheClear): skip + print mitigation.
        # Same one-liner as the unattended path so users have a clear
        # recovery action if MCP later fails on stale bytecode.
        Write-Warn "non-mneme Bun cache entries detected - leaving them in place (zero-question default)"
        Write-Warn "(pass -ForceBunCacheClear to wipe automatically, or -PromptForBunCacheClear to be asked)"
        Write-Host ""
        Write-Host "    If MCP later fails with 'SyntaxError: Export named ...':" -ForegroundColor Yellow
        Write-Host ("      Remove-Item -Recurse -Force `"{0}`"" -f (Join-Path $env:USERPROFILE '.bun\install\cache')) -ForegroundColor White
        Write-Host ("      Remove-Item -Recurse -Force `"{0}`"" -f (Join-Path $env:LOCALAPPDATA 'Bun\Cache')) -ForegroundColor White
        Write-Host ""
        $shouldWipe = $false
    }

    if ($shouldWipe) {
        foreach ($p in @(
            (Join-Path $env:USERPROFILE '.bun\install\cache'),
            (Join-Path $env:LOCALAPPDATA 'Bun\Cache')
        )) {
            if (Test-Path $p) {
                try {
                    Remove-Item -Recurse -Force $p -ErrorAction SilentlyContinue
                    Write-OK ("cleared {0}" -f $p)
                } catch {
                    Write-Warn ("could not clear {0}: {1}" -f $p, $_.Exception.Message)
                }
            }
        }
    }

    # B1 (2026-05-02): actually run `bun install --frozen-lockfile` in mcp/.
    # Without this the staged ~/.mneme/mcp/node_modules/ may be missing zod /
    # @modelcontextprotocol/sdk / ajv (B2 silently shipped an empty
    # node_modules from AWS install test 2026-05-02). Bun starts MCP server and
    # immediately ENOENTs on the first import. The cache wipe above prevents
    # stale bytecode; this step ensures the deps actually exist on disk.
    Push-Location "$BinDir\..\mcp"
    try {
        $bunExe = "$env:USERPROFILE\.bun\bin\bun.exe"
        if (Test-Path $bunExe) {
            Write-Step "running bun install --frozen-lockfile in mcp/"
            & $bunExe install --frozen-lockfile 2>&1 | ForEach-Object { "    $_" }
            if ($LASTEXITCODE -ne 0) {
                Write-Warn "bun install failed with exit $LASTEXITCODE - MCP server may not start"
            } else {
                Write-OK "MCP node_modules installed (bun install --frozen-lockfile)"
            }
        } else {
            Write-Warn "bun.exe not found at $bunExe - skipping bun install (MCP may not work)"
        }
    } finally { Pop-Location }
}

# ============================================================================
# Step 6 - Register + start mneme daemon (Windows Scheduled Task)
# ============================================================================
#
# WHY a Scheduled Task instead of Start-Process / `mneme daemon start`:
#
# Spawning the daemon as a child of the install shell makes it inherit the
# shell's Windows Job Object. PowerShell shells under SSH, VS Code's
# integrated terminal, certain CI runners, and remoting all run inside Job
# Objects that do NOT permit JOB_OBJECT_LIMIT_BREAKAWAY_OK. The Rust daemon's
# CREATE_BREAKAWAY_FROM_JOB call returns ERROR_ACCESS_DENIED (os error 5);
# the spawn falls back to non-detached, and the daemon dies the moment the
# install shell closes. Observed verbatim from `mneme.exe daemon start`
# stderr on a fresh VM (2026-04-29):
#
#     WARN spawn with CREATE_BREAKAWAY_FROM_JOB failed; retrying without it
#          error=Access is denied. (os error 5)
#     WARN supervisor spawned without job-object breakaway;
#          daemon will exit if the parent job is terminated
#
# `schtasks.exe /Create + /Run` runs the daemon under the Schedule service
# (svchost), bypassing every Job Object in the shell hierarchy. The daemon
# survives:
#   - the install shell closing
#   - SSH session terminations
#   - reboots (via /SC ONLOGON it auto-respawns at next user logon)
#
# Pass -NoScheduledTask to fall back to the legacy Start-Process path.

Write-Step "step 6/8 - register + start mneme daemon (Windows Scheduled Task)"

$MnemeBin = Join-Path $BinDir 'mneme.exe'
if (-not (Test-Path $MnemeBin)) {
    Write-Warn ("mneme.exe not found at {0} - did extraction succeed?" -f $MnemeBin)
    Write-Warn "skipping daemon start. run manually later: mneme daemon start"
} else {
    # Probe an already-healthy daemon first (idempotent reinstall path).
    $alreadyUp = $false
    try {
        $h = Invoke-WebRequest -Uri 'http://127.0.0.1:7777/health' -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
        if ($h.StatusCode -eq 200) { $alreadyUp = $true }
    } catch { }

    if ($IsUpgrade -and $alreadyUp) {
        Write-OK "upgrade: daemon already healthy (HTTP /health 200) - not bouncing"
    } else {
        $taskName    = 'MnemeDaemon'
        $useSchTask  = -not $NoScheduledTask
        $spawnedHow  = $null

        if ($useSchTask) {
            # /F overwrites if it exists (idempotent reinstall).
            # /IT keeps it interactive so it appears in Task Scheduler's
            #     per-user view (no admin needed for non-elevated daemons).
            # /SC ONLOGON triggers next logon (after reboot) so the daemon
            #     auto-respawns without the user ever running `mneme daemon
            #     start` themselves.
            # We /Run it explicitly below so the daemon is up for the
            # current session without waiting for a logoff/logon cycle.
            #
            # Bug-2026-05-02 (AWS install regression cycle): on a non-elevated
            # standard user account (e.g. a non-admin user on the AWS test fleet), schtasks
            # /Create returns "Access is denied" + writes to stderr. With
            # the script-global $ErrorActionPreference='Stop' (line 95),
            # PS5.1's `2>&1` redirect on a native command wraps each stderr
            # line as a NativeCommandError object, which Stop turns into a
            # TERMINATING exception BEFORE the if-LASTEXITCODE-ne-0 check
            # runs. Result: install bombs out at step 6 with "FAIL: inner
            # installer failed with exit code 1" instead of falling through
            # to the Start-Process fallback. The fix: wrap the whole
            # schtasks block in try/catch and run it with a *local*
            # $ErrorActionPreference='Continue' so LASTEXITCODE drives the
            # fallback, not exception flow. Schtasks failure is non-fatal
            # by design here (Start-Process fallback handles current-session
            # daemon start; ONLOGON auto-respawn is a nice-to-have).
            $tr = ('"{0}" daemon start' -f $MnemeBin)
            try {
                $prevEAP = $ErrorActionPreference
                $ErrorActionPreference = 'Continue'
                # A7-006 (2026-05-04): filter out ErrorRecord objects from
                # the merged stdout/stderr stream. schtasks.exe writes its
                # "Access is denied" failure on non-elevated runs to stderr,
                # PowerShell wraps each stderr line as ErrorRecord whose
                # Out-String form includes the full CategoryInfo +
                # FullyQualifiedErrorId noise -- 6+ lines of CLR-style trace
                # for what is fundamentally "you're not admin." We keep the
                # plain string lines (which contain the human-readable
                # "ERROR: Access is denied.") and drop the metadata wrapper.
                $createOut = & schtasks.exe /Create /TN $taskName /TR $tr /SC ONLOGON /F /IT 2>&1 |
                    Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] } |
                    Out-String
                $createExit = $LASTEXITCODE
            } catch {
                $createOut = $_.Exception.Message
                $createExit = 1
            } finally {
                $ErrorActionPreference = $prevEAP
            }
            if ($createExit -ne 0) {
                Write-Warn ("schtasks /Create failed (Access denied on non-elevated user is normal; falling back to Start-Process): {0}" -f $createOut.Trim())
                $useSchTask = $false
            } else {
                Write-OK ("scheduled task '{0}' registered (auto-start at user logon)" -f $taskName)
                try {
                    $prevEAP = $ErrorActionPreference
                    $ErrorActionPreference = 'Continue'
                    # A7-006: see Create call above for ErrorRecord-filter rationale.
                    $runOut = & schtasks.exe /Run /TN $taskName 2>&1 |
                        Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] } |
                        Out-String
                    $runExit = $LASTEXITCODE
                } catch {
                    $runOut = $_.Exception.Message
                    $runExit = 1
                } finally {
                    $ErrorActionPreference = $prevEAP
                }
                if ($runExit -ne 0) {
                    Write-Warn ("schtasks /Run failed: {0}" -f $runOut.Trim())
                    Write-Warn "falling back to Start-Process daemon spawn"
                    $useSchTask = $false
                } else {
                    $spawnedHow = 'schtasks'
                }
            }
        }

        if (-not $useSchTask) {
            # Legacy fallback path. Note: per the comment block above, this
            # may NOT survive parent-shell closure if the parent is in a
            # restrictive Job Object. Used only when schtasks is forbidden
            # (locked-down hosts) or the user passed -NoScheduledTask.
            try {
                Start-Process -FilePath $MnemeBin -ArgumentList 'daemon','start' -WindowStyle Hidden -ErrorAction Stop | Out-Null
                $spawnedHow = 'Start-Process'
            } catch {
                Write-Warn ("Start-Process daemon spawn failed: {0}" -f $_.Exception.Message)
                Write-Warn "run manually later: mneme daemon start"
            }
        }

        # Verify via HTTP /health on port 7777 -- proves both supervisor +
        # workers are up AND the supervisor.pipe discovery file is in sync.
        # (Polling `mneme daemon status` would re-trigger the auto-spawn
        # loop if the discovery file is stale; HTTP /health is safe.)
        $waited = 0
        $up     = $false
        while ($waited -lt 25000) {
            Start-Sleep -Milliseconds 500
            $waited += 500
            try {
                $h = Invoke-WebRequest -Uri 'http://127.0.0.1:7777/health' -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
                if ($h.StatusCode -eq 200) { $up = $true; break }
            } catch { }
        }
        if ($up) {
            $how = if ($spawnedHow) { $spawnedHow } else { 'unknown' }
            Write-OK ("daemon up (HTTP /health 200 in ~{0}ms via {1})" -f $waited, $how)
        } else {
            Write-Warn "daemon did not report healthy via HTTP /health within 25s"
            Write-Warn "check: mneme doctor | mneme daemon logs"
        }
    }
}

# ============================================================================
# Step 7 - Register MCP with Claude Code (HOOKS DEFAULT-ON, NO manifest)
# ============================================================================
#
# v0.3.2 K1 fix (Bug B): the installer writes the mcpServers.mneme entry into
# ~/.claude.json AND registers the 8 mneme hook entries under
# ~/.claude/settings.json::hooks by default. Without those hooks the
# persistent-memory pipeline (history.db, tasks.db, tool_cache.db,
# livestate.db) stays empty and mneme degrades to a query-only MCP
# surface. Pass --no-hooks / --skip-hooks to `mneme install` to opt out.
#
# We invoke `mneme install --platform=claude-code --skip-manifest` here.
# The previous version called `mneme register-mcp --platform claude-code`,
# which hardcodes skip_hooks: true internally (cli/src/commands/register_mcp.rs)
# and therefore never wrote the hook entries - that is the root cause of
# settings_hook_count=0 in the 2026-04-29 phase6_smoke postmortem. Switching
# to `mneme install` opts in to the K1 default-on hooks pipeline.
#
# --skip-manifest is preserved because install.ps1's invariant is "MCP +
# hooks only, leave CLAUDE.md / AGENTS.md alone." Power users who want the
# manifest block can re-run `mneme install --platform=claude-code` without
# the flag later.
#
# The v0.3.0 install incident (F-011/F-012) is architecturally impossible
# now: every hook binary reads STDIN JSON and exits 0 on any internal error,
# so a mneme bug can never block tool calls.

Write-Step "step 7/8 - registering MCP + hooks with Claude Code"

if (-not (Test-Path $MnemeBin)) {
    Write-Warn "mneme.exe not present, skipping MCP / hooks registration"
} else {
    try {
        # Combined Bug B + LIE-3: invoke `mneme register-mcp --with-hooks --json`
        # so we get BOTH hook registration (Bug B) AND structured per-field
        # status (LIE-3) instead of trusting `$LASTEXITCODE` alone. Pre-Bug-B
        # the banner implied hooks registered when register-mcp had skipped
        # them (LIE) - now we only claim what the JSON itself confirms,
        # AND we ask register-mcp to actually write hooks via --with-hooks.
        $rawOut = & $MnemeBin register-mcp --platform claude-code --with-hooks --json 2>&1
        $exit = $LASTEXITCODE

        # The JSON line is the LAST line of stdout; everything else
        # (including stderr lines mixed by `2>&1`) is informational.
        $jsonLine = $null
        $infoLines = @()
        foreach ($line in @($rawOut)) {
            $s = [string]$line
            if ($s.TrimStart().StartsWith('{') -and $s.TrimEnd().EndsWith('}')) {
                $jsonLine = $s
            } else {
                $infoLines += $s
            }
        }
        foreach ($l in $infoLines) { if ($l -ne '') { Write-Info $l } }

        $status = $null
        if ($jsonLine) {
            try { $status = $jsonLine | ConvertFrom-Json }
            catch { Write-Warn ("could not parse register-mcp JSON: {0}" -f $_.Exception.Message) }
        }

        if ($status) {
            # Per-field reporting. Each line states ONE concrete claim;
            # if the claim is false the user sees it explicitly.
            if ($status.mcp_entry_written) {
                Write-OK ("mcp_entry_written: yes ({0})" -f $status.mcp_config_path)
            } else {
                Write-Warn ("mcp_entry_written: NO ({0}) - mneme is not in the host's MCP config" -f $status.mcp_config_path)
            }

            $expected = [int]$status.hooks_expected
            $registered = [int]$status.hooks_registered
            if ($expected -gt 0) {
                if ($registered -eq $expected) {
                    Write-OK ("hooks_registered: {0}/{1} ({2})" -f $registered, $expected, $status.settings_json_path)
                } else {
                    Write-Warn ("hooks_registered: {0}/{1} ({2}) - persistent-memory pipeline incomplete" -f $registered, $expected, $status.settings_json_path)
                }
            } else {
                Write-Info "hooks_registered: 0/0 (skipped via --no-hooks)"
            }

            foreach ($err in @($status.errors)) {
                if ($err) { Write-Warn ("register-mcp error: {0}" -f $err) }
            }

            if ($status.ok) {
                Write-OK "Claude Code MCP + hooks registered (verified per-field, persistent-memory pipeline live)"
            } else {
                Write-Warn "Claude Code MCP / hooks registration INCOMPLETE - see per-field reports above"
                Write-Warn "run manually later: mneme register-mcp --platform claude-code --with-hooks --json"
            }
        } else {
            # JSON missing or unparseable. Fall back to exit-code reporting
            # but DO NOT print the old "complete" line - that was the lie
            # LIE-3 came to fix.
            if ($exit -eq 0) {
                Write-Warn "mneme register-mcp returned exit 0 but emitted no JSON status - claims unverified"
            } else {
                Write-Warn ("mneme register-mcp exited {0} - MCP / hooks may not be registered" -f $exit)
            }
            Write-Warn "run manually later: mneme register-mcp --platform claude-code --with-hooks --json"
        }
    } catch {
        Write-Warn ("MCP / hooks registration error: {0}" -f $_.Exception.Message)
        Write-Warn "run manually later: mneme register-mcp --platform claude-code --with-hooks --json"
    }
}

# B1.5 (2026-05-02): register the mneme plugin (commands / agents /
# skills) with Claude Code so the slash commands `/mn-build`,
# `/mn-recall`, `/mn-why`, etc. autocomplete in Claude Code. Without
# this step the staged `plugin/` dir lives at
# %USERPROFILE%\.mneme\plugin but Claude Code only scans
# %USERPROFILE%\.claude\plugins for plugins, so users see "MCP works
# but where are the slash commands?" and have no obvious next step.
#
# Symlink first (zero-copy, always reflects updates inside ~/.mneme).
# Symlinks on Windows require Developer Mode OR an elevated shell --
# fall back to a recursive copy if `New-Item -SymbolicLink` is denied.
$pluginSrc = Join-Path $MnemeHome 'plugin'
$claudePluginsDir = Join-Path $env:USERPROFILE '.claude\plugins'
$mnemePluginDest = Join-Path $claudePluginsDir 'mneme'
if (Test-Path $pluginSrc) {
    if (-not (Test-Path $claudePluginsDir)) {
        New-Item -ItemType Directory -Path $claudePluginsDir -Force | Out-Null
    }
    if (Test-Path $mnemePluginDest) {
        Remove-Item -Recurse -Force $mnemePluginDest -ErrorAction SilentlyContinue
    }
    try {
        # Symlink first (no copy, always fresh)
        New-Item -ItemType SymbolicLink -Path $mnemePluginDest -Target $pluginSrc -ErrorAction Stop | Out-Null
        Write-OK ("plugin registered (symlink): {0} -> {1}" -f $mnemePluginDest, $pluginSrc)
    } catch {
        # Symlink requires Developer Mode or admin; fallback to copy.
        Copy-Item -Recurse -Force $pluginSrc $mnemePluginDest
        Write-OK ("plugin registered (copy): {0}" -f $mnemePluginDest)
    }
} else {
    Write-Warn ("plugin directory not found at {0} - slash commands /mn-* won't work in Claude Code" -f $pluginSrc)
}

# ============================================================================
# Step 7b - Auto-install bundled models (BGE + GGUFs)
# ============================================================================
#
# When this script is invoked from an extracted bundle (e.g. via
# `install-bundle.ps1` or as Path B / AUTO from INSTALL-AT-HOME.md), the
# bundle layout is:
#
#     <bundle>/
#       release/mneme-v0.3.2-windows-x64.zip   <- the -LocalZip target
#       models/                                 <- BGE + tokenizer + GGUFs
#
# Step 7b auto-detects models/ next to release/ and runs
#     mneme models install --from-path <bundle>/models
# so users don't have to remember the manual step (per START-HERE.md "step
# 4"). Pass -ModelsPath <dir> to override the auto-detection; pass -NoModels
# to skip entirely. Skipping is non-fatal: mneme still works on the
# pure-Rust hashing-trick fallback embedder, just with lower recall.

Write-Step "step 7b/8 - auto-install bundled models"

if ($NoModels) {
    Write-Info "(-NoModels set: skipping models installation)"
} elseif (-not (Test-Path -LiteralPath $MnemeBin)) {
    Write-Warn "mneme.exe missing, skipping models install"
} else {
    $autoModelsPath = $null
    $detection      = ''
    if ($ModelsPath) {
        if (Test-Path -LiteralPath $ModelsPath) {
            $autoModelsPath = (Resolve-Path -LiteralPath $ModelsPath).Path
            $detection      = 'explicit -ModelsPath'
        } else {
            Write-Warn ("-ModelsPath given but not found: {0}" -f $ModelsPath)
        }
    } elseif ($LocalZip -and (Test-Path -LiteralPath $LocalZip)) {
        try {
            # <bundle>/release/<localzip>  →  <bundle>/models
            $bundleRoot = Split-Path -Parent (Split-Path -Parent (Resolve-Path -LiteralPath $LocalZip).Path)
            $candidate  = Join-Path $bundleRoot 'models'
            if (Test-Path -LiteralPath $candidate) {
                $autoModelsPath = (Resolve-Path -LiteralPath $candidate).Path
                $detection      = 'auto-detected next to -LocalZip'
            }
        } catch { }
    }

    if ($autoModelsPath) {
        $files   = Get-ChildItem -LiteralPath $autoModelsPath -File -ErrorAction SilentlyContinue |
                   Where-Object { $_.Extension -in '.onnx','.gguf','.bin','.json' }
        $totalMB = if ($files) { [math]::Round((($files | Measure-Object Length -Sum).Sum)/1MB, 1) } else { 0 }
        Write-Info ("source: {0} ({1})" -f $autoModelsPath, $detection)
        Write-Info ("  {0} candidate file(s), {1} MB" -f $files.Count, $totalMB)
        if ($files.Count -gt 0) {
            try {
                $modOut  = & $MnemeBin models install --from-path $autoModelsPath 2>&1 | Out-String
                $modExit = $LASTEXITCODE
                $modOut.Trim().Split("`n") | Where-Object { $_.Trim() } | ForEach-Object { Write-Info $_.TrimEnd() }
                if ($modExit -eq 0) {
                    Write-OK ("models registered into {0}\.mneme\models" -f $env:USERPROFILE)
                } else {
                    Write-Warn ("models install exited {0}" -f $modExit)
                }
            } catch {
                Write-Warn ("models install threw: {0}" -f $_.Exception.Message)
            }
        }
    } else {
        Write-Info "no bundled models detected; skipping"
        Write-Info "to install models manually: mneme models install --from-path <models-dir>"
    }
}

# ============================================================================
# Step 7c - MSVC build-toolchain PATH augmentation (for `mneme doctor`)
# ============================================================================
#
# Without this, `mneme doctor` reports:
#     link.exe : MISSING -- not on PATH
#     cl.exe   : MISSING -- not on PATH
#     summary  : FAIL -- MSVC Build Tools missing
#
# even when VS Build Tools IS installed (the same doctor output shows
# `VC Tools : ok` and `Windows SDK : ok` -- the FAIL is only because the
# default PATH doesn't include the MSVC bin; Microsoft only adds it inside
# the "x64 Native Tools" cmd shim's transient environment).
#
# We resolve the MSVC bin via vswhere.exe and prepend it to user PATH so
# the next `mneme doctor` run finds link.exe + cl.exe on PATH and reports
# pass instead of fail. This is mneme-runtime-irrelevant (mneme ships as
# pre-built binaries; MSVC is only needed for `cargo install tauri-cli`
# from source) but it removes a confusing FAIL line that scares users.
# Idempotent: skipped if MSVC bin is already on PATH.

Write-Step "step 7c/8 - MSVC PATH augmentation (for clean mneme doctor)"

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    Write-Info "vswhere.exe not present (VS Installer absent); skipping (mneme runtime unaffected)"
} else {
    try {
        $vsRoot = & $vswhere -products '*' -requires Microsoft.VisualCpp.Tools.HostX64.TargetX64 -property installationPath -latest 2>$null
        $vsRoot = ($vsRoot | Select-Object -First 1)
        if (-not $vsRoot -or -not (Test-Path -LiteralPath $vsRoot)) {
            Write-Info "VS BuildTools not found via vswhere; skipping (mneme runtime unaffected)"
        } else {
            $verFile = Join-Path $vsRoot 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt'
            if (-not (Test-Path -LiteralPath $verFile)) {
                Write-Info "VC tools version file missing; skipping"
            } else {
                $vcVer   = (Get-Content -LiteralPath $verFile -Raw).Trim()
                $msvcBin = Join-Path $vsRoot ("VC\Tools\MSVC\{0}\bin\Hostx64\x64" -f $vcVer)
                if (-not (Test-Path -LiteralPath $msvcBin)) {
                    Write-Info ("MSVC bin not found at {0}; skipping" -f $msvcBin)
                } else {
                    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
                    $segments = if ($userPath) { $userPath -split ';' } else { @() }
                    if ($segments -contains $msvcBin) {
                        Write-OK ("MSVC bin already on user PATH: {0}" -f $msvcBin)
                    } else {
                        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $msvcBin } else { "$msvcBin;$userPath" }
                        [Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
                        $env:PATH = "$msvcBin;$env:PATH"
                        Write-OK ("MSVC bin prepended to user PATH: {0}" -f $msvcBin)
                        Write-Info "next 'mneme doctor' run (in a NEW shell) reports link.exe + cl.exe ok"
                    }
                }
            }
        }
    } catch {
        Write-Warn ("MSVC PATH augmentation threw: {0}" -f $_.Exception.Message)
    }
}

# ============================================================================
# Step 8 - Done
# ============================================================================

Write-Step "step 8/8 - complete"
Write-Host ""
Write-Host "================================================================" -ForegroundColor Green
Write-Host "  mneme installed - $ReleaseTag" -ForegroundColor Green
Write-Host "================================================================" -ForegroundColor Green
Write-Host ""
Write-Host "  Hooks registered (8/8) - persistent-memory pipeline live" -ForegroundColor Green
Write-Host "    history.db, tasks.db, tool_cache.db, livestate.db will fill"
Write-Host "    as Claude Code uses tools. Disable via: mneme uninstall --platform claude-code"
Write-Host ""
Write-Host "  Next steps:" -ForegroundColor White
Write-Host "    1. Restart Claude Code so it picks up the new MCP server"
Write-Host "    2. Open a project directory and run: mneme build ."
Write-Host "    3. Inside Claude Code, try:  /mn-recall `"what does auth do`""
Write-Host ""
Write-Host "  Verify:" -ForegroundColor White
Write-Host "    mneme daemon status"
Write-Host "    mneme --version"
Write-Host "    mneme doctor --strict       # full pre-flight: G1-G10 toolchain + binary self-test"
Write-Host ""
if (-not $WithMultimodal) {
    Write-Host "  Optional - enable multimodal OCR / image conversion:" -ForegroundColor White
    Write-Host "    re-run install.ps1 with -WithMultimodal to install:"
    Write-Host "      G9  Tesseract OCR    (winget UB-Mannheim.TesseractOCR)"
    Write-Host "      G10 ImageMagick      (winget ImageMagick.ImageMagick)"
    Write-Host ""
}
Write-Host "  Uninstall:" -ForegroundColor White
Write-Host "    mneme uninstall --platform claude-code"
Write-Host "    Remove-Item -Recurse -Force $MnemeHome"
Write-Host ""
# B11 (2026-05-02): the soft "Open a NEW terminal..." line was the LAST
# thing the user saw, but in plain Yellow text it competed with every
# other Yellow OK / WARN line above it -- and users reliably missed it,
# then hit "mneme not found" running `mneme doctor` in the same shell.
# Promote it to a boxed banner so it cannot be skimmed past. MUST stay
# the last visible block in the success path so the next thing the
# user reads after install is "open a NEW terminal".
Write-Host ""
Write-Host "  +---------------------------------------------------------+" -ForegroundColor Yellow
Write-Host "  |  IMPORTANT: open a NEW PowerShell terminal before       |" -ForegroundColor Yellow
Write-Host "  |  running 'mneme doctor' or 'mneme build' -- the PATH    |" -ForegroundColor Yellow
Write-Host "  |  change just applied is not visible in this session.    |" -ForegroundColor Yellow
Write-Host "  +---------------------------------------------------------+" -ForegroundColor Yellow
Write-Host ""
