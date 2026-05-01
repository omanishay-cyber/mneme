# bootstrap-install.ps1
# ----------------------
# One-liner Windows installer for mneme — TRULY one-command, all included.
#
# Usage (PowerShell, any user, no admin needed):
#
#   iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
#
# Or, equivalently:
#
#   irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1 | iex
#
# What it does:
#   1. Picks a release version (default: v0.3.2; override with $env:MNEME_VERSION)
#   2. Downloads mneme-<ver>-windows-x64.zip from the GitHub Release
#   3. Expands it into ~/.mneme/
#   4. Runs ~/.mneme/scripts/install.ps1 with -LocalZip + -WithMultimodal
#   5. Downloads model assets (bge, qwen-embed, qwen-coder, phi-3 parts) and
#      installs them via `mneme models install --from-path`
#   6. Reports success / next steps
#
# Opt-outs:
#   -NoModels        skip the model download/install step (legacy behaviour)
#   -NoMultimodal    skip Tesseract OCR + ImageMagick install
#   -NoToolchain     skip toolchain auto-install (G1-G10)
#   -KeepDownload    keep the temp download dir for inspection
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:MNEME_VERSION) { $env:MNEME_VERSION } else { 'v0.3.2' }),
    [switch]$NoToolchain,
    [switch]$NoMultimodal,
    [switch]$NoModels,
    [switch]$KeepDownload
)

$ErrorActionPreference = 'Stop'

function Section($name) { Write-Host "" -NoNewline; Write-Host ("== $name ==") -ForegroundColor Cyan }
function OK($msg)       { Write-Host "  OK: $msg" -ForegroundColor Green }
function Step($msg)     { Write-Host "  -> $msg" -ForegroundColor Yellow }
function WarnLine($msg) { Write-Host "  WARN: $msg" -ForegroundColor DarkYellow }
function Fail($msg)     { Write-Host "  FAIL: $msg" -ForegroundColor Red; throw $msg }

Section "mneme bootstrap installer"
Write-Host "  version    : $Version"
Write-Host "  user       : $env:USERNAME"
Write-Host "  target     : $env:USERPROFILE\.mneme"
Write-Host "  models     : $(if ($NoModels) { 'SKIP (-NoModels)' } else { 'AUTO-DOWNLOAD' })"
Write-Host "  multimodal : $(if ($NoMultimodal) { 'SKIP (-NoMultimodal)' } else { 'AUTO-INSTALL' })"

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------
if ($PSVersionTable.PSVersion.Major -lt 5) {
    Fail "PowerShell 5.1+ required (you have $($PSVersionTable.PSVersion))."
}
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$releaseBase = "https://github.com/omanishay-cyber/mneme/releases/download/$Version"

function Get-Asset {
    param(
        [string]$Name,
        [string]$Dest,
        [int]$RetryCount = 3
    )
    $url = "$releaseBase/$Name"
    for ($attempt = 1; $attempt -le $RetryCount; $attempt++) {
        try {
            Step "Fetching $Name (attempt $attempt/$RetryCount)"
            Invoke-WebRequest -Uri $url -OutFile $Dest -UseBasicParsing
            $sz = (Get-Item $Dest).Length
            if ($sz -lt 100) { throw "downloaded file too small ($sz bytes) — likely a 404 HTML page" }
            $mb = [math]::Round($sz / 1MB, 2)
            OK "downloaded $Name ($mb MB)"
            return
        } catch {
            WarnLine "attempt $attempt failed: $_"
            if ($attempt -eq $RetryCount) { throw $_ }
            Start-Sleep -Seconds (2 * $attempt)
        }
    }
}

# ---------------------------------------------------------------------------
# Step 1: Download the release zip
# ---------------------------------------------------------------------------
Section "Download release zip"
$zipName = "mneme-$Version-windows-x64.zip"
$tmpDir = Join-Path $env:TEMP "mneme-bootstrap-$Version"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
$localZip = Join-Path $tmpDir $zipName
Get-Asset -Name $zipName -Dest $localZip

# ---------------------------------------------------------------------------
# Step 2: Stop any running mneme processes (in case of re-install)
# ---------------------------------------------------------------------------
Section "Stop existing mneme processes (if any)"
$names = @('mneme', 'mneme-daemon', 'mneme-store', 'mneme-parsers', 'mneme-scanners',
           'mneme-brain', 'mneme-livebus', 'mneme-md-ingest', 'mneme-multimodal')
$killed = 0
foreach ($n in $names) {
    Get-Process -Name $n -ErrorAction SilentlyContinue | ForEach-Object {
        try { Stop-Process -Id $_.Id -Force; $killed += 1 } catch { }
    }
}
OK "stopped $killed process(es)"

# ---------------------------------------------------------------------------
# Step 3: Extract zip into ~/.mneme
# ---------------------------------------------------------------------------
Section "Expand to ~/.mneme"
$mnemeDir = Join-Path $env:USERPROFILE '.mneme'
if (-not (Test-Path $mnemeDir)) {
    New-Item -ItemType Directory -Force -Path $mnemeDir | Out-Null
}
Step "Expand-Archive -Force -DestinationPath $mnemeDir"
Expand-Archive -Path $localZip -DestinationPath $mnemeDir -Force

$mnemeExe = Join-Path $mnemeDir 'bin\mneme.exe'
if (-not (Test-Path $mnemeExe)) {
    Fail "post-extract sanity check failed: $mnemeExe missing"
}
OK ("extracted (mneme.exe present at $mnemeExe)")

# ---------------------------------------------------------------------------
# Step 4: Run the inner installer (registers MCP, hooks, PATH, Defender, daemon)
# ---------------------------------------------------------------------------
Section "Run inner installer (scripts/install.ps1)"
$inner = Join-Path $mnemeDir 'scripts\install.ps1'
if (-not (Test-Path $inner)) {
    Fail "inner installer missing: $inner"
}

$innerArgs = @('-LocalZip', $localZip)
if ($NoToolchain)   { $innerArgs += '-NoToolchain' }
if (-not $NoMultimodal) { $innerArgs += '-WithMultimodal' }

Step "powershell -ExecutionPolicy Bypass -File $inner $($innerArgs -join ' ')"
& powershell -NoProfile -ExecutionPolicy Bypass -File $inner @innerArgs
if ($LASTEXITCODE -ne 0) {
    Fail "inner installer failed with exit code $LASTEXITCODE"
}

# ---------------------------------------------------------------------------
# Step 5: Download + install model assets (B-020 fix, 2026-04-30)
# ---------------------------------------------------------------------------
if ($NoModels) {
    Section "Models — SKIPPED (-NoModels)"
    WarnLine "Smart-search will use the hashing-trick fallback (lower recall)."
    WarnLine "Local LLM summaries will fall back to signature-only text."
    WarnLine "Run later:  mneme models install --from-path <download-folder>"
} else {
    Section "Download + install model assets"
    $modelsDir = Join-Path $tmpDir 'models'
    New-Item -ItemType Directory -Force -Path $modelsDir | Out-Null

    # Asset list — names must match release artifacts exactly. Each is
    # tagged with `Required`: a missing required asset aborts the model
    # install but doesn't fail the whole bootstrap (mneme runtime works
    # without models, just degraded).
    $assets = @(
        @{ Name = 'bge-small-en-v1.5.onnx';   Required = $true  },
        @{ Name = 'tokenizer.json';            Required = $true  },
        @{ Name = 'qwen-embed-0.5b.gguf';      Required = $false },
        @{ Name = 'qwen-coder-0.5b.gguf';      Required = $false },
        @{ Name = 'phi-3-mini-4k.gguf.part00'; Required = $false },
        @{ Name = 'phi-3-mini-4k.gguf.part01'; Required = $false },
        @{ Name = 'merge-phi3-parts.ps1';      Required = $false }
    )

    $modelDownloads = 0
    $modelFailures  = @()
    foreach ($a in $assets) {
        $dest = Join-Path $modelsDir $a.Name
        try {
            Get-Asset -Name $a.Name -Dest $dest -RetryCount 3
            $modelDownloads += 1
        } catch {
            $modelFailures += $a.Name
            if ($a.Required) {
                WarnLine "REQUIRED asset $($a.Name) failed — smart embeddings will be unavailable"
            } else {
                WarnLine "optional asset $($a.Name) failed — corresponding capability disabled"
            }
        }
    }
    OK "downloaded $modelDownloads / $($assets.Count) model assets ($(($modelFailures | Measure-Object).Count) failed)"

    # Merge phi-3 parts if both present + merge script downloaded
    $mergeScript = Join-Path $modelsDir 'merge-phi3-parts.ps1'
    $part00      = Join-Path $modelsDir 'phi-3-mini-4k.gguf.part00'
    $part01      = Join-Path $modelsDir 'phi-3-mini-4k.gguf.part01'
    if ((Test-Path $mergeScript) -and (Test-Path $part00) -and (Test-Path $part01)) {
        Step "merging phi-3 parts via $mergeScript"
        try {
            & powershell -NoProfile -ExecutionPolicy Bypass -File $mergeScript -PartsDir $modelsDir
            if ($LASTEXITCODE -eq 0) {
                OK "phi-3 parts merged"
            } else {
                WarnLine "phi-3 merge exited with code $LASTEXITCODE — continuing without phi-3"
            }
        } catch {
            WarnLine "phi-3 merge threw: $_  (continuing)"
        }
    }

    # Hand the directory to mneme — it handles validation + placement
    if ($modelDownloads -gt 0) {
        Step "mneme models install --from-path $modelsDir"
        try {
            & $mnemeExe models install --from-path $modelsDir
            if ($LASTEXITCODE -eq 0) {
                OK "models installed under ~/.mneme/models"
            } else {
                WarnLine "mneme models install exited with code $LASTEXITCODE"
            }
        } catch {
            WarnLine "mneme models install threw: $_"
        }
    }
}

# ---------------------------------------------------------------------------
# Step 6: Cleanup (keep download if requested)
# ---------------------------------------------------------------------------
if (-not $KeepDownload) {
    Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
Section "DONE"
Write-Host "  Mneme $Version installed."
Write-Host ""
Write-Host "  Verify:" -ForegroundColor Yellow
Write-Host "    mneme --version           # should print $($Version.TrimStart('v'))"
Write-Host "    mneme doctor              # health check"
Write-Host "    claude mcp list           # should show: mneme: Connected"
Write-Host ""
if ($NoModels) {
    Write-Host "  You skipped models. To install later:" -ForegroundColor Yellow
    Write-Host "    iex (irm https://github.com/omanishay-cyber/mneme/releases/download/$Version/bootstrap-install.ps1)"
}
Write-Host "  Restart Claude Code so it picks up the new MCP server." -ForegroundColor Yellow
