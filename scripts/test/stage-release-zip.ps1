# stage-release-zip.ps1 - assemble mneme-v0.3.2-windows-x64.zip
#
# Reads target/release/*.exe + mcp/ + vision/dist/ + scripts/ + plugin/
# from the source tree and produces a single self-contained zip that
# install.ps1 -LocalZip can consume.
#
# Layout produced (matches what install.ps1 expects to extract under
# ~/.mneme/):
#
#   bin/                      - all 9 mneme*.exe binaries (+ mneme-vision.exe if built)
#   mcp/                      - TS source + node_modules + dist (Bun runs from here at MCP startup)
#   static/vision/            - vision SPA dist (daemon serves via /api/graph/* + cached `/`)
#   scripts/install.ps1       - the canonical installer
#   scripts/install.sh        - POSIX equivalent
#   scripts/uninstall.ps1     - K19 standalone uninstaller
#   plugin/                   - plugin.json + skills/ + agents/ + commands/ + templates/
#   uninstall.ps1             - same K19 standalone, dropped at zip root for visibility
#   VERSION.txt               - "0.3.2" + git commit if present
#
# Author: Anish Trivedi.
# Apache-2.0.

[CmdletBinding()]
param(
    [string]$SourceRoot = "C:\Users\Anish\Desktop\New folder (2)\source",
    [string]$Version = "0.3.2",
    [string]$OutZip = "$env:USERPROFILE\Desktop\mneme-v0.3.2-windows-x64.zip",
    [string]$StageDir = "$env:USERPROFILE\Desktop\mneme-stage",
    [switch]$IncludeVisionTauri,
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
$target = Join-Path $SourceRoot "target\release"
if (-not (Test-Path $target)) {
    Fail "target/release/ not found at $target. Run 'cargo build --workspace --release' first."
}
$mcp = Join-Path $SourceRoot "mcp"
$visionDist = Join-Path $SourceRoot "vision\dist"
$scripts = Join-Path $SourceRoot "scripts"
$plugin = Join-Path $SourceRoot "plugin"

foreach ($p in @($mcp, $visionDist, $scripts, $plugin)) {
    if (-not (Test-Path $p)) { Fail "missing required dir: $p" }
}

# Required binaries (workspace).
$requiredBins = @(
    "mneme.exe",
    "mneme-daemon.exe",
    "mneme-store.exe",
    "mneme-parsers.exe",
    "mneme-scanners.exe",
    "mneme-brain.exe",
    "mneme-livebus.exe",
    "mneme-md-ingest.exe",
    "mneme-multimodal.exe"
)
$missing = @()
foreach ($bin in $requiredBins) {
    if (-not (Test-Path (Join-Path $target $bin))) { $missing += $bin }
}
if ($missing) {
    Fail ("missing release binaries: {0}" -f ($missing -join ', '))
}
OK ("all {0} required release binaries present" -f $requiredBins.Count)

# Optional: vision Tauri binary. The Cargo package name is `mneme-vision-tauri`
# so the binary lands as `mneme-vision-tauri.exe`. We rename it to
# `mneme-vision.exe` when staging into bin/ — that's the canonical install
# name (CLI's `mneme view` looks for ~/.mneme/bin/mneme-vision.exe; see
# `cli/src/commands/view.rs::resolve_vision_exe`).
$visionTauri = Join-Path $SourceRoot "vision\tauri\target\release\mneme-vision-tauri.exe"
$haveTauri = Test-Path $visionTauri
if ($IncludeVisionTauri -and -not $haveTauri) {
    Fail "Tauri exe missing at $visionTauri (run 'cd vision/tauri && cargo build --release' first), or omit -IncludeVisionTauri"
}

# ---------------------------------------------------------------------------
# Stage dir setup
# ---------------------------------------------------------------------------

Section "Stage dir"
if (Test-Path $StageDir) {
    if (-not $Force) {
        $reply = Read-Host "Stage dir exists: $StageDir. Overwrite? [y/N]"
        if ($reply -notmatch '^(y|yes)$') { Fail "user declined stage dir overwrite" }
    }
    Remove-Item -Recurse -Force $StageDir
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null
OK "fresh stage at $StageDir"

# ---------------------------------------------------------------------------
# Copy bin/
# ---------------------------------------------------------------------------

Section "Copy bin/"
$stageBin = Join-Path $StageDir "bin"
New-Item -ItemType Directory -Path $stageBin | Out-Null
foreach ($bin in $requiredBins) {
    Copy-Item (Join-Path $target $bin) $stageBin
    Step "+ $bin"
}
if ($haveTauri) {
    # Rename mneme-vision-tauri.exe -> mneme-vision.exe in the staged bin/.
    Copy-Item $visionTauri (Join-Path $stageBin "mneme-vision.exe")
    Step "+ mneme-vision.exe (from mneme-vision-tauri.exe)"
}

# B-011 (v0.3.2-v2-home): bundle onnxruntime.dll for the `ort` crate's
# `load-dynamic` feature. Without this DLL on PATH (or next to mneme.exe)
# the BGE-small-en-v1.5 embedder silently falls back to the hashing-trick
# backend and `mneme recall` quality drops dramatically. Windows ships
# its own `onnxruntime.dll` in System32, but it's typically older
# (1.17 era) than the API version `ort 2.0.0-rc.12 + api-24` requires
# (1.18+). Bundling ours next to mneme.exe overrides the system DLL
# because Windows DLL search order checks the executable's directory
# before System32. Vendored at `vendor/onnxruntime/onnxruntime.dll` in
# source so a fresh `cargo build` + stage produces a working bundle
# without manual download.
$ortDllSrc = Join-Path $SourceRoot "vendor\onnxruntime\onnxruntime.dll"
if (-not (Test-Path $ortDllSrc)) {
    # Fallback: also accept it sitting in target/release (some build
    # orchestrations drop it there).
    $ortDllSrc = Join-Path $target "onnxruntime.dll"
}
if (Test-Path $ortDllSrc) {
    Copy-Item $ortDllSrc (Join-Path $stageBin "onnxruntime.dll")
    $ortVer = (Get-Command (Join-Path $stageBin "onnxruntime.dll") -ErrorAction SilentlyContinue).FileVersionInfo.FileVersion
    Step ("+ onnxruntime.dll (B-011, version {0})" -f $ortVer)
} else {
    Write-Host "  WARN: onnxruntime.dll not found at $ortDllSrc - BGE will fall back to hashing-trick" -ForegroundColor Yellow
    Write-Host "        Download from https://github.com/microsoft/onnxruntime/releases (win-x64-1.20.x.zip)" -ForegroundColor Yellow
    Write-Host "        and copy lib/onnxruntime.dll to vendor/onnxruntime/onnxruntime.dll" -ForegroundColor Yellow
}

$binCount = (Get-ChildItem $stageBin | Measure-Object).Count
$binSize = ((Get-ChildItem $stageBin -File | Measure-Object Length -Sum).Sum / 1MB)
OK ("bin/ complete: {0} files, {1:N1} MB" -f $binCount, $binSize)

# ---------------------------------------------------------------------------
# Copy mcp/
# ---------------------------------------------------------------------------

Section "Copy mcp/ (TS source + node_modules + dist)"
$stageMcp = Join-Path $StageDir "mcp"

# B2 (2026-05-02): pre-stage validation gate. If source mcp/node_modules/
# is missing zod or @modelcontextprotocol/sdk, the staged zip will ship
# broken (POS install 2026-05-02 hit ENOENT for zod). Run bun install
# --frozen-lockfile to repopulate, then HARD-FAIL if the deps still
# aren't there. Belt + suspenders with B1 (install.ps1 also runs bun
# install at install time on the user's machine).
$zodPkgJson = Join-Path $mcp "node_modules\zod\package.json"
$sdkPkgJson = Join-Path $mcp "node_modules\@modelcontextprotocol\sdk\package.json"
if (-not (Test-Path $zodPkgJson) -or -not (Test-Path $sdkPkgJson)) {
    Write-Host "  -> mcp/node_modules incomplete, running 'bun install --frozen-lockfile' first..." -ForegroundColor Yellow
    Push-Location $mcp
    try {
        & bun install --frozen-lockfile 2>&1 | ForEach-Object { "     $_" }
        if ($LASTEXITCODE -ne 0) {
            Fail "bun install in mcp/ failed with exit $LASTEXITCODE - refusing to stage broken zip"
        }
    } finally { Pop-Location }
}
if (-not (Test-Path $zodPkgJson)) {
    Fail "mcp/node_modules/zod/package.json STILL missing after bun install - refusing to stage broken zip (B2 / 2026-05-02 POS install bug)"
}
if (-not (Test-Path $sdkPkgJson)) {
    Fail "mcp/node_modules/@modelcontextprotocol/sdk/package.json STILL missing after bun install - refusing to stage broken zip"
}
Write-Host "     OK: mcp/node_modules has zod + @modelcontextprotocol/sdk" -ForegroundColor Green

# robocopy is faster + smarter than Copy-Item for trees this size.
& robocopy $mcp $stageMcp /E /NFL /NDL /NJH /NJS /NP /MT:8 | Out-Null
$rc = $LASTEXITCODE
if ($rc -gt 7) { Fail ("robocopy mcp/ failed with code {0}" -f $rc) }

# B2 post-stage assertion: confirm robocopy actually included node_modules.
$stagedZodPkg = Join-Path $stageMcp "node_modules\zod\package.json"
if (-not (Test-Path $stagedZodPkg)) {
    Fail "post-stage: $stagedZodPkg missing - robocopy didn't include node_modules"
}

$mcpSize = ((Get-ChildItem $stageMcp -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
OK ("mcp/ complete: {0:N1} MB" -f $mcpSize)

# ---------------------------------------------------------------------------
# Copy vision/dist -> static/vision/
# ---------------------------------------------------------------------------

Section "Copy vision/dist -> static/vision/"
$stageVision = Join-Path $StageDir "static\vision"
New-Item -ItemType Directory -Path $stageVision -Force | Out-Null
& robocopy $visionDist $stageVision /E /NFL /NDL /NJH /NJS /NP | Out-Null
$rc = $LASTEXITCODE
if ($rc -gt 7) { Fail ("robocopy vision/dist failed with code {0}" -f $rc) }
$visionSize = ((Get-ChildItem $stageVision -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
$indexHtml = Join-Path $stageVision "index.html"
if (-not (Test-Path $indexHtml)) { Fail "static/vision/index.html missing after copy" }
$indexBytes = (Get-Item $indexHtml).Length
OK ("static/vision/ complete: {0:N2} MB, index.html={1} bytes" -f $visionSize, $indexBytes)

# ---------------------------------------------------------------------------
# Copy scripts/
# ---------------------------------------------------------------------------

Section "Copy scripts/"
$stageScripts = Join-Path $StageDir "scripts"
& robocopy $scripts $stageScripts /E /NFL /NDL /NJH /NJS /NP /XD test | Out-Null
$rc = $LASTEXITCODE
if ($rc -gt 7) { Fail ("robocopy scripts/ failed with code {0}" -f $rc) }
# Drop the test/ subtree on purpose — VM-test scripts aren't needed at install time.
OK "scripts/ complete (test/ excluded)"

# Drop the K19 standalone uninstaller at the zip root for visibility.
$standaloneSrc = Join-Path $stageScripts "uninstall.ps1"
if (Test-Path $standaloneSrc) {
    Copy-Item $standaloneSrc (Join-Path $StageDir "uninstall.ps1")
    OK "+ root uninstall.ps1 (K19 standalone)"
}

# ---------------------------------------------------------------------------
# Copy plugin/
# ---------------------------------------------------------------------------

Section "Copy plugin/"
$stagePlugin = Join-Path $StageDir "plugin"
& robocopy $plugin $stagePlugin /E /NFL /NDL /NJH /NJS /NP | Out-Null
$rc = $LASTEXITCODE
if ($rc -gt 7) { Fail ("robocopy plugin/ failed with code {0}" -f $rc) }
$pluginSize = ((Get-ChildItem $stagePlugin -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
OK ("plugin/ complete: {0:N1} MB" -f $pluginSize)

# ---------------------------------------------------------------------------
# VERSION.txt
# ---------------------------------------------------------------------------

Section "VERSION.txt"
$gitCommit = "unknown"
$gitBranch = "unknown"
try {
    Push-Location $SourceRoot
    $gitCommit = (& git rev-parse HEAD 2>$null) -join ''
    $gitBranch = (& git branch --show-current 2>$null) -join ''
    Pop-Location
} catch {}
$versionContent = @"
Mneme $Version
Built: $(Get-Date -Format 'yyyy-MM-ddTHH:mm:ssZ')
Source: $SourceRoot
Git commit: $gitCommit
Git branch: $gitBranch
Includes vision-tauri exe: $haveTauri
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
    Write-Host ("    {0,-30}  {1,8:N1} MB" -f $_.Name, $sub)
}

Section "Compress to zip"
if (Test-Path $OutZip) {
    if (-not $Force) {
        $reply = Read-Host "Output zip exists: $OutZip. Overwrite? [y/N]"
        if ($reply -notmatch '^(y|yes)$') { Fail "user declined zip overwrite" }
    }
    Remove-Item $OutZip -Force
}
$start = Get-Date
Compress-Archive -Path "$StageDir\*" -DestinationPath $OutZip -CompressionLevel Optimal -Force
$end = Get-Date
$zipSize = (Get-Item $OutZip).Length / 1MB
OK ("zip created: {0} ({1:N1} MB) in {2:N1}s" -f $OutZip, $zipSize, ($end - $start).TotalSeconds)

# ---------------------------------------------------------------------------
# Verify zip (smoke: re-extract to scratch + sanity-check)
# ---------------------------------------------------------------------------

Section "Verify zip integrity"
$smoke = Join-Path $env:TEMP "mneme-zip-smoke-$(Get-Random)"
New-Item -ItemType Directory -Path $smoke | Out-Null
try {
    Expand-Archive -Path $OutZip -DestinationPath $smoke -Force
    $smokeBin = Join-Path $smoke "bin\mneme.exe"
    if (-not (Test-Path $smokeBin)) { Fail "post-extract: bin/mneme.exe missing" }
    $smokeMcp = Join-Path $smoke "mcp\src\index.ts"
    if (-not (Test-Path $smokeMcp)) { Fail "post-extract: mcp/src/index.ts missing" }
    $smokeIndex = Join-Path $smoke "static\vision\index.html"
    if (-not (Test-Path $smokeIndex)) { Fail "post-extract: static/vision/index.html missing" }
    $smokeInst = Join-Path $smoke "scripts\install.ps1"
    if (-not (Test-Path $smokeInst)) { Fail "post-extract: scripts/install.ps1 missing" }
    OK "zip integrity verified (bin/mneme.exe, mcp/src, static/vision/index.html, scripts/install.ps1 all present)"
} finally {
    Remove-Item -Recurse -Force $smoke -ErrorAction SilentlyContinue
}

Section "DONE"
Write-Host "  Zip:   $OutZip" -ForegroundColor Green
Write-Host ("  Size:  {0:N1} MB" -f $zipSize) -ForegroundColor Green
Write-Host ""
Write-Host "Next: deploy via scripts/test/vm-deploy-and-test.ps1" -ForegroundColor Cyan
