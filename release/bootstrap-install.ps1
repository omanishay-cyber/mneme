# bootstrap-install.ps1
# ----------------------
# One-liner Windows installer for mneme -- TRULY one-command, all included.
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
#   -SkipHashCheck   skip SHA-256 verification of downloaded assets
#                    (Bug G-14 -- only use for hand-cut beta zips that
#                    aren't yet listed in $ExpectedHashes)
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

# NOTE: this script is invoked via `iex (irm <url>)`. Invoke-Expression
# evaluates the input as STATEMENTS in the calling scope -- NOT as a
# script file. That means a top-level `param()` block does NOT work
# (verified on PS 5.1 + PS 7: `param()` is parsed as a literal call to
# a non-existent `param` cmdlet, and the `[switch]` defaults
# concatenate into the next variable). We therefore read every
# "parameter" from environment variables instead.
#
# To override defaults, set env vars BEFORE the iex line:
#   $env:MNEME_VERSION = 'v0.3.3'
#   $env:MNEME_NO_MULTIMODAL = '1'
#   iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
#
# Or pass flags via the scriptblock pattern (rare):
#   $sb = [scriptblock]::Create((irm <url>))
#   & $sb
$Version = if ($env:MNEME_VERSION) { $env:MNEME_VERSION } else { 'v0.3.2' }
$NoToolchain   = [bool]$env:MNEME_NO_TOOLCHAIN
$NoMultimodal  = [bool]$env:MNEME_NO_MULTIMODAL
$NoModels      = [bool]$env:MNEME_NO_MODELS
$KeepDownload  = [bool]$env:MNEME_KEEP_DOWNLOAD
$SkipHashCheck = [bool]$env:MNEME_SKIP_HASH_CHECK

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

# Bug G-14 / SEC-3 (2026-05-01): SHA-256 verification table.
#
# Without integrity checking, a CDN compromise or interrupted download
# silently delivers garbage that the installer copies to disk and the
# user runs. Each entry below pins one release artifact to its
# canonical SHA-256. Files NOT in this table fall through to a "warn-
# but-continue" path so we don't block on assets we don't yet pin
# (e.g., new model files added between releases). Files IN this table
# MUST match -- mismatch is a hard fail.
#
# To regenerate a hash:
#   Get-FileHash <file> -Algorithm SHA256 | Select-Object Hash
# (uppercase hex, no separators)
#
# To skip verification entirely (for a hand-cut beta zip), pass
# `-SkipHashCheck` to bootstrap-install.ps1.
$ExpectedHashes = @{
    # Mneme release artifacts -- populate per-release before tagging.
    # Example placeholders (NOT the real hashes for v0.3.2):
    # 'mneme-v0.3.2-windows-x64.zip' = '0123456789ABCDEF...';
    # 'bge-small-en-v1.5.onnx'        = '...';
    # 'tokenizer.json'                = '...';
    # 'qwen-embed-0.5b.gguf'          = '...';
    # 'qwen-coder-0.5b.gguf'          = '...';
    # 'phi-3-mini-4k.gguf.part00'     = '...';
    # 'phi-3-mini-4k.gguf.part01'     = '...';
}

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
            if ($sz -lt 100) { throw "downloaded file too small ($sz bytes) -- likely a 404 HTML page" }

            # Bug G-14 / SEC-3 (2026-05-01): SHA-256 verification.
            # If the file is in our pinned-hash table, compute its
            # SHA-256 and compare. Mismatch = fail loud (the file
            # could be tampered with or partially downloaded). If the
            # file is NOT in the table, log a one-line WARN so it's
            # visible in the install log without blocking new assets.
            if (-not $SkipHashCheck) {
                if ($ExpectedHashes.ContainsKey($Name)) {
                    $expected = $ExpectedHashes[$Name].ToUpper()
                    $actual = (Get-FileHash -Path $Dest -Algorithm SHA256).Hash.ToUpper()
                    if ($actual -ne $expected) {
                        # Remove the corrupt file so a retry doesn't trust the cached copy.
                        Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
                        throw "SHA-256 mismatch for $Name`n  expected: $expected`n  actual:   $actual`n  (likely corrupt download or tampered file -- refusing to install)"
                    }
                    OK "SHA-256 verified for $Name"
                } else {
                    WarnLine "no pinned SHA-256 for $Name (continuing without integrity check)"
                }
            }

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
# Bug G-7 (2026-05-01): the empty `catch { }` swallowed every
# Stop-Process failure (Access denied, zombie, locked exe). The
# subsequent extract step would then race the still-alive process
# and produce corrupt files in ~/.mneme/bin. Now we surface failures.
$killed = 0
$failed = 0
foreach ($n in $names) {
    Get-Process -Name $n -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            Stop-Process -Id $_.Id -Force
            $killed += 1
        } catch {
            $failed += 1
            WarnLine ("could not stop ${n} (PID $($_.Id)): $($_.Exception.Message) -- extract may fail if exe is still locked")
        }
    }
}
OK "stopped $killed process(es)$(if ($failed -gt 0) { "  ($failed failed)" })"

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
    Section "Models -- SKIPPED (-NoModels)"
    WarnLine "Smart-search will use the hashing-trick fallback (lower recall)."
    WarnLine "Local LLM summaries will fall back to signature-only text."
    WarnLine "Run later:  mneme models install --from-path <download-folder>"
} else {
    Section "Download + install model assets"
    $modelsDir = Join-Path $tmpDir 'models'
    New-Item -ItemType Directory -Force -Path $modelsDir | Out-Null

    # Asset list -- names must match release artifacts exactly. Each is
    # tagged with `Required`: a missing required asset aborts the model
    # install but doesn't fail the whole bootstrap (mneme runtime works
    # without models, just degraded).
    #
    # Bug D-1a/D-1b/D-1c (2026-05-01): `merge-phi3-parts.ps1` removed.
    # Native part-merge now lives in `cli/src/commands/models.rs`
    # (`parse_split_part` + the merge block in `install_from_path_to_root`).
    # The PowerShell script was not in source control, was called with
    # the wrong parameter name, and silently dropped 2.28 GB of phi-3
    # parts on every install. Rust handles it now: download the parts,
    # `mneme models install --from-path` concatenates them.
    $assets = @(
        @{ Name = 'bge-small-en-v1.5.onnx';   Required = $true  },
        @{ Name = 'tokenizer.json';            Required = $true  },
        @{ Name = 'qwen-embed-0.5b.gguf';      Required = $false },
        @{ Name = 'qwen-coder-0.5b.gguf';      Required = $false },
        @{ Name = 'phi-3-mini-4k.gguf.part00'; Required = $false },
        @{ Name = 'phi-3-mini-4k.gguf.part01'; Required = $false }
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
                WarnLine "REQUIRED asset $($a.Name) failed -- smart embeddings will be unavailable"
            } else {
                WarnLine "optional asset $($a.Name) failed -- corresponding capability disabled"
            }
        }
    }
    OK "downloaded $modelDownloads / $($assets.Count) model assets ($(($modelFailures | Measure-Object).Count) failed)"

    # NOTE: phi-3 parts are merged natively by `mneme models install
    # --from-path` (the next step). No external PowerShell merge script
    # is needed or invoked. See Bug D-1c fix in models.rs.

    # Hand the directory to mneme -- it handles validation + placement.
    #
    # Bug G-6 part B (2026-05-01): non-zero exit from `mneme models
    # install` is now FATAL. Previously this was a `WarnLine` + continue
    # which let the bootstrap report SUCCESS even when models had been
    # downloaded but never registered. Combined with the (now-fixed)
    # phi-3 silent drop, the user could end up with 1.2 GB of model
    # files on disk and zero of them registered, with a "DONE" message
    # printed at the end. Models are the value-add -- if registration
    # fails, that's a real failure the user must see and act on.
    if ($modelDownloads -gt 0) {
        Step "mneme models install --from-path $modelsDir"
        # Bug-2026-05-02 (store PC POS install cycle): same root cause
        # as the schtasks fix in scripts/install.ps1 step 6. With the
        # script-global $ErrorActionPreference='Stop' (line 56), PS5.1
        # wraps any stderr line from `mneme models install` (the merge
        # progress message `merged 2 parts -> ... bytes` is printed via
        # eprintln! in cli/src/commands/models.rs) as a NativeCommandError
        # object, which Stop turns into a TERMINATING exception BEFORE
        # the LASTEXITCODE-eq-0 success branch runs. Result: every
        # bootstrap reported "INSTALL EXCEPTION: mneme models install
        # threw: merged 2 parts -> ..." and aborted at the FINAL step,
        # even though models were already merged + installed correctly.
        # Fix: do the invocation under a local Continue pref so exit
        # code drives the success/failure branch, not exception flow.
        try {
            $prevEAP = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            $modelsOut = & $mnemeExe models install --from-path $modelsDir 2>&1
            $modelsExit = $LASTEXITCODE
        } catch {
            # An ACTUAL exception (e.g. mneme.exe missing or unreachable)
            # - distinct from the cosmetic stderr-as-error case Stop
            # triggers when the binary writes progress to stderr.
            $modelsOut = $_.Exception.Message
            $modelsExit = 99
        } finally {
            $ErrorActionPreference = $prevEAP
        }
        # Echo what mneme printed (merge progress, registration result,
        # any genuine warnings). One line per item, indented for the
        # visual grouping the rest of the script uses.
        if ($modelsOut) { $modelsOut | ForEach-Object { Write-Host "    $_" } }
        if ($modelsExit -eq 0) {
            OK "models installed under ~/.mneme/models"
        } else {
            throw "mneme models install exited with code $modelsExit -- bootstrap aborted (models are required for smart recall)"
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
