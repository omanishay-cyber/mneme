# stage-final-zip.ps1 - assemble $env:USERPROFILE\Desktop\mneme final.zip
#
# This is the Phase E final deliverable. Bundles:
#   - source/                         (the full source tree, this bundle)
#   - release/mneme-v0.3.2-windows-x64.zip   (the binary release ZIP, tested on VM)
#   - models/README.md                (placeholder + how to run mneme models install --from-path)
#   - docs/                           (curated copies of key docs)
#   - INSTALL.md                      (canonical install instructions)
#   - CHANGELOG.md                    (history with Wave 2 entries)
#   - PLAN-2026-04-29-mneme-final-zip.md  (this work session's plan)
#   - VERIFIED.md                     (test results from Phase D, written by VM orchestrator)
#   - VERSION.txt                     (0.3.2 + build metadata)
#
# Output: $env:USERPROFILE\Desktop\mneme final.zip
#
# Authors: Anish Trivedi & Kruti Trivedi.
# ASCII-only - PowerShell 5.1 cp1252 safe.

[CmdletBinding()]
param(
    [string]$SourceRoot = "$env:USERPROFILE\Desktop\mneme-source",
    [string]$ReleaseZip = "$env:USERPROFILE\Desktop\mneme-v0.3.2-windows-x64.zip",
    [string]$VerifiedMd = "$env:USERPROFILE\Desktop\VERIFIED.md",
    [string]$VmResultsJson = "$env:USERPROFILE\Desktop\vm-test-results-2026-04-29.json",
    [string]$OutZip = "$env:USERPROFILE\Desktop\mneme final.zip",
    [string]$StageDir = "$env:USERPROFILE\Desktop\mneme-final-stage",
    # Bundle real model weights into the final.zip. Looks for them at
    # ..\models relative to $SourceRoot.
    # Adds ~3.5 GB to the zip but lets the user `mneme models install --from-path`
    # straight from the extracted final/ folder.
    [switch]$IncludeModels,
    # Use Fastest compression. Models are already compressed (gguf/onnx);
    # Optimal would burn 5-10 min for zero size win.
    [switch]$Fastest,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

function Section($name) {
    Write-Host ""
    Write-Host ("== $name ==") -ForegroundColor Cyan
}
function Step($msg) { Write-Host "  -> $msg" -ForegroundColor Yellow }
function OK($msg)   { Write-Host "     OK: $msg" -ForegroundColor Green }
function Fail($msg) { Write-Host "     FAIL: $msg" -ForegroundColor Red; throw $msg }

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------

Section "Pre-flight"
if (-not (Test-Path $SourceRoot)) { Fail "source root missing: $SourceRoot" }
if (-not (Test-Path $ReleaseZip)) { Fail "release zip missing: $ReleaseZip - run stage-release-zip.ps1 first" }
if (-not (Test-Path $VerifiedMd)) {
    Write-Host "  WARN: $VerifiedMd missing - Phase E will ship without VM-verified results section" -ForegroundColor Yellow
}
OK "all required inputs found"

# ---------------------------------------------------------------------------
# Stage dir
# ---------------------------------------------------------------------------

Section "Stage dir"
if (Test-Path $StageDir) {
    # A7-012 (2026-05-04): zero-question default. Stage dirs are scratch
    # artifacts -- safe to auto-overwrite under -Force OR when stdin is
    # redirected (CI / piped). The interactive prompt remains for local
    # maintainer runs without -Force.
    $autoOverwrite = $Force
    if (-not $autoOverwrite) {
        try {
            if ([Console]::IsInputRedirected) { $autoOverwrite = $true }
        } catch { $autoOverwrite = $false }
    }
    if (-not $autoOverwrite) {
        $reply = Read-Host "Stage dir exists: $StageDir. Overwrite? [y/N]"
        if ($reply -notmatch '^(y|yes)$') { Fail "user declined stage dir overwrite" }
    } else {
        Step "Stage dir exists at $StageDir -- auto-overwriting (Force / non-interactive)"
    }
    Remove-Item -Recurse -Force $StageDir
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null
OK "fresh stage at $StageDir"

# ---------------------------------------------------------------------------
# source/ - the full source tree (excluding target/, node_modules/, dist/)
# ---------------------------------------------------------------------------

Section "Copy source/ (excluding build artifacts)"
$sourceDest = Join-Path $StageDir "source"
& robocopy $SourceRoot $sourceDest /E /NFL /NDL /NJH /NJS /NP /MT:8 `
    /XD target node_modules dist .git | Out-Null
$rc = $LASTEXITCODE
if ($rc -gt 7) { Fail ("robocopy source failed code=$rc") }
$srcSize = ((Get-ChildItem $sourceDest -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
OK ("source/ complete: {0:N1} MB" -f $srcSize)

# ---------------------------------------------------------------------------
# release/ - the binary release ZIP
# ---------------------------------------------------------------------------

Section "Copy release/"
$releaseDest = Join-Path $StageDir "release"
New-Item -ItemType Directory -Path $releaseDest | Out-Null
Copy-Item $ReleaseZip $releaseDest
$relSize = (Get-Item $ReleaseZip).Length / 1MB
OK ("release/{0} ({1:N1} MB)" -f (Split-Path -Leaf $ReleaseZip), $relSize)

# ---------------------------------------------------------------------------
# models/ - placeholder + install hint
# ---------------------------------------------------------------------------

Section "models/"
$modelsDest = Join-Path $StageDir "models"
New-Item -ItemType Directory -Path $modelsDest | Out-Null

# Where the real model weights live on host: ..\models relative to source.
$srcModels = Join-Path (Split-Path -Parent $SourceRoot) "models"

if ($IncludeModels -and (Test-Path $srcModels)) {
    Step "copying real model weights from $srcModels"
    & robocopy $srcModels $modelsDest /E /NFL /NDL /NJH /NJS /NP /MT:8 | Out-Null
    $rc = $LASTEXITCODE
    if ($rc -gt 7) { Fail ("robocopy models failed code=$rc") }
    $msz = ((Get-ChildItem $modelsDest -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
    $mcnt = (Get-ChildItem $modelsDest -Recurse -File -ErrorAction SilentlyContinue | Measure-Object).Count
    OK ("models/ complete: {0:N0} files, {1:N1} MB" -f $mcnt, $msz)
} else {
    if ($IncludeModels) { Write-Host "  WARN: -IncludeModels set but $srcModels not found" -ForegroundColor Yellow }
}

# Always drop a README explaining usage (whether or not real models are bundled).
$modelsReadme = @"
# Mneme - models/ directory

This directory contains the local model weights mneme uses for semantic recall
and (optionally) LLM-backed code summaries. Mneme NEVER downloads model files
over the network - the local-only invariant forbids it. To activate the models,
you point mneme at this directory ONCE after install.

## Bundled files (if -IncludeModels was set when this zip was built)

| File | Size | Purpose |
|---|---|---|
| bge-small-en-v1.5.onnx | ~127 MB | BGE-small-en-v1.5 ONNX embedder (semantic recall, 384-dim) |
| tokenizer.json | ~725 KB | Hugging Face tokenizer paired with BGE-small |
| phi-3-mini-4k.gguf | ~2.28 GB | Phi-3-mini 4k context LLM (quality summaries) |
| qwen-coder-0.5b.gguf | ~469 MB | Qwen2.5 Coder 0.5B (fast coding LLM) |
| qwen-embed-0.5b.gguf | ~609 MB | Qwen2.5 Embed 0.5B (alt embedder) |

If the directory is empty or only this README is here, the build that produced
this final.zip did NOT bundle weights - either build-time `-IncludeModels`
wasn't set, OR you're looking at a slim ship intended for travel.

## How to install (one command)

After running install.ps1 to set up mneme itself, run this in PowerShell:

    mneme models install --from-path "<path-to-this-models-dir>"

For example, if you extracted final.zip to `C:\Users\<you>\Desktop\mneme-final\`,
the command is:

    mneme models install --from-path "C:\Users\<you>\Desktop\mneme-final\models"

Mneme copies the files into `~/.mneme/models/` and registers them with the
brain crate. Verify with:

    mneme doctor

You should see embedder=bge-small under "models" in the doctor output.

## Does it install automatically? No.

`mneme install` (the binary installer) ONLY installs the binaries + MCP
registration. It does NOT touch model weights. This is by design:

- Local-only: mneme will not download multi-GB files behind your back.
- User control: different machines / users may want different model
  combinations (e.g. coder LLM only, no Phi-3).
- Speed: a fresh install takes seconds without models.

## Falling back without models

If you SKIP `mneme models install`, semantic recall falls back to a pure-Rust
hashing-trick embedder (384-dim, deterministic, zero native deps). It works
out-of-the-box but with lower recall quality than BGE. LLM-summary tools
(refactor_suggest, why) degrade to signature-only output.

## Why opt-in / why not auto

The local-only invariant is in `CLAUDE.md` "Hard rules" section 4. Section 22
of the design doc forbids any feature that contemplates network access without
explicit user opt-in. Auto-downloading model weights would violate that.
"@
Set-Content -Path (Join-Path $modelsDest "README.md") -Value $modelsReadme -Encoding UTF8
OK "models/README.md written"

# ---------------------------------------------------------------------------
# docs/ - curated copies (the most important top-level docs)
# ---------------------------------------------------------------------------

Section "docs/"
$docsDest = Join-Path $StageDir "docs"
New-Item -ItemType Directory -Path $docsDest | Out-Null
$keyDocs = @(
    "CLAUDE.md",
    "ARCHITECTURE.md",
    "BENCHMARKS.md",
    "CHANGELOG.md",
    "INSTALL.md",
    "NEXT-PATH.md",
    "IDEAS.md",
    "CONTRIBUTING.md",
    "CODE_OF_CONDUCT.md"
)
foreach ($d in $keyDocs) {
    $src = Join-Path $SourceRoot $d
    if (Test-Path $src) {
        Copy-Item $src $docsDest
    }
}
# The whole source\docs\ folder
$srcDocs = Join-Path $SourceRoot "docs"
if (Test-Path $srcDocs) {
    & robocopy $srcDocs (Join-Path $docsDest "extra") /E /NFL /NDL /NJH /NJS /NP | Out-Null
}
$docsSize = ((Get-ChildItem $docsDest -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1KB)
OK ("docs/ complete: {0:N0} KB" -f $docsSize)

# ---------------------------------------------------------------------------
# Top-level files (INSTALL.md, CHANGELOG.md duplicates for visibility)
# ---------------------------------------------------------------------------

Section "Top-level files"
foreach ($d in @("INSTALL.md", "CHANGELOG.md", "CLAUDE.md")) {
    $src = Join-Path $SourceRoot $d
    if (Test-Path $src) {
        Copy-Item $src $StageDir
        OK "+ $d"
    }
}

# Plan + verified.md
$planDoc = Join-Path $SourceRoot "docs\PLAN-2026-04-29-mneme-final-zip.md"
if (Test-Path $planDoc) {
    Copy-Item $planDoc $StageDir
    OK "+ PLAN-2026-04-29-mneme-final-zip.md"
}

if (Test-Path $VerifiedMd) {
    Copy-Item $VerifiedMd $StageDir
    OK "+ VERIFIED.md"
} else {
    # Fall back: write a stub from the json results if available
    $stub = "# Mneme Final - Verification Notes`n`n"
    if (Test-Path $VmResultsJson) {
        $stub += "VM test results JSON at:`n  $VmResultsJson`n`n"
        $j = Get-Content $VmResultsJson -Raw | ConvertFrom-Json
        $stub += "VM IP: $($j.vm_ip)`n"
        $stub += "Started at: $($j.started_at)`n"
        $stub += "Completed at: $($j.completed_at)`n`n"
        $stub += "Phases:`n"
        foreach ($k in $j.phases.PSObject.Properties.Name) {
            $stub += "- $k`n"
        }
    } else {
        $stub += "(VM tests not yet run; populate from scripts/test/vm-deploy-and-test.ps1 output)`n"
    }
    Set-Content -Path (Join-Path $StageDir "VERIFIED.md") -Value $stub -Encoding UTF8
    OK "+ VERIFIED.md (stub fallback written)"
}

# vm-test-results-2026-04-29.json (raw orchestrator JSON for traceability)
if (Test-Path $VmResultsJson) {
    Copy-Item $VmResultsJson $StageDir
    OK "+ vm-test-results-2026-04-29.json"
}

# VERSION.txt
$gitCommit = "unknown"
$gitBranch = "unknown"
try {
    Push-Location $SourceRoot
    $gitCommit = (& git rev-parse HEAD 2>$null) -join ''
    $gitBranch = (& git branch --show-current 2>$null) -join ''
    Pop-Location
} catch {}
$versionContent = @"
Mneme 0.3.2 - final.zip deliverable
Built: $(Get-Date -Format 'yyyy-MM-ddTHH:mm:ssZ')
Source: $SourceRoot
Git commit: $gitCommit
Git branch: $gitBranch

Contents:
  source/                       - full source tree (no target/, no node_modules/, no dist/)
  release/                      - built mneme-v0.3.2-windows-x64.zip
  models/README.md              - model install instructions
  docs/                         - curated docs
  INSTALL.md, CHANGELOG.md, CLAUDE.md
  VERIFIED.md                   - VM test results
  vm-test-results-2026-04-29.json - raw orchestrator output
  PLAN-2026-04-29-mneme-final-zip.md - work session plan
"@
Set-Content -Path (Join-Path $StageDir "VERSION.txt") -Value $versionContent -Encoding UTF8
OK "VERSION.txt written"

# ---------------------------------------------------------------------------
# Summary + zip
# ---------------------------------------------------------------------------

Section "Stage summary"
$totalSize = ((Get-ChildItem $StageDir -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
$totalFiles = (Get-ChildItem $StageDir -Recurse -File -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host ("  total: {0:N1} MB across {1} files" -f $totalSize, $totalFiles)
Get-ChildItem $StageDir | ForEach-Object {
    $sub = if ($_.PSIsContainer) {
        ((Get-ChildItem $_.FullName -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
    } else { ($_.Length / 1MB) }
    Write-Host ("    {0,-50}  {1,8:N1} MB" -f $_.Name, $sub)
}

Section "Compress to zip"
if (Test-Path $OutZip) {
    # A7-012 (2026-05-04): zip overwrite is destructive (prior shipped
    # artifact). Auto-overwrite under -Force OR when stdin is redirected
    # (CI). Otherwise keep the prompt as a TTY guardrail.
    $autoOverwrite = $Force
    if (-not $autoOverwrite) {
        try {
            if ([Console]::IsInputRedirected) { $autoOverwrite = $true }
        } catch { $autoOverwrite = $false }
    }
    if (-not $autoOverwrite) {
        $reply = Read-Host "Output exists: $OutZip. Overwrite? [y/N]"
        if ($reply -notmatch '^(y|yes)$') { Fail "user declined overwrite" }
    } else {
        Step "Output exists at $OutZip -- auto-overwriting (Force / non-interactive)"
    }
    Remove-Item $OutZip -Force
}
$start = Get-Date
$cl = if ($Fastest) { 'Fastest' } else { 'Optimal' }
Step "compressing with -CompressionLevel $cl"
# A7-018 (2026-05-04): switch from PS5.1 Compress-Archive to .NET
# ZipFile.CreateFromDirectory because final.zip with -IncludeModels
# crosses 3.5 GB which trips Compress-Archive's 2 GB internal buffer
# limit (silent OutOfMemoryException, half-written zip). The .NET API
# streams entries and has no such cap. Compression-level mapping:
#   Optimal | Fastest | NoCompression  -- enum lives in CompressionLevel.
Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction SilentlyContinue
$compLevel = if ($Fastest) {
    [System.IO.Compression.CompressionLevel]::Fastest
} else {
    [System.IO.Compression.CompressionLevel]::Optimal
}
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $StageDir,
    $OutZip,
    $compLevel,
    $false)
$end = Get-Date
$zipSize = (Get-Item $OutZip).Length / 1MB
OK ("zip: {0} ({1:N1} MB) in {2:N1}s" -f $OutZip, $zipSize, ($end - $start).TotalSeconds)

# Hash for integrity
$hash = Get-FileHash -Path $OutZip -Algorithm SHA256
Set-Content -Path "$OutZip.sha256" -Value ("{0}  {1}" -f $hash.Hash, (Split-Path -Leaf $OutZip)) -Encoding ASCII
OK "sha256: $($hash.Hash)"

Section "DONE - final deliverable assembled"
Write-Host ""
Write-Host "  Final ZIP:    $OutZip" -ForegroundColor Green
Write-Host ("  Size:         {0:N1} MB" -f $zipSize) -ForegroundColor Green
Write-Host ("  SHA256:       {0}" -f $hash.Hash) -ForegroundColor Green
Write-Host ""
Write-Host "  Hand off to user. If verification was clean, this is the ship." -ForegroundColor Cyan
