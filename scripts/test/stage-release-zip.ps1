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
# Authors: Anish Trivedi & Kruti Trivedi.
# Apache-2.0.

[CmdletBinding()]
param(
    [string]$SourceRoot = "$env:USERPROFILE\Desktop\mneme-source",
    [string]$Version = "0.3.2",
    [string]$OutZip = "$env:USERPROFILE\Desktop\mneme-v0.3.2-windows-x64.zip",
    [string]$StageDir = "$env:USERPROFILE\Desktop\mneme-stage",
    # Optional Rust target triple (e.g. x86_64-pc-windows-msvc). When set,
    # binaries are read from target/<triple>/release/ instead of target/release/.
    # CI workflows that pass --target=<triple> to cargo MUST set this; local
    # `cargo build --release` (no --target) leaves it empty and uses
    # target/release/. Fixes the chronic CI-only "missing release binaries"
    # failure on the multi-arch-release.yml workflow (see GH Actions runs
    # 25261952169 + 25266802464).
    [string]$TargetTriple,
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
# Resolve target dir: if -TargetTriple was passed (CI), use target/<triple>/release;
# otherwise fall back to target/release (local dev cargo build --release).
if ($TargetTriple) {
    $target = Join-Path $SourceRoot "target\$TargetTriple\release"
    if (-not (Test-Path $target)) {
        # Try the plain target/release/ as a secondary fallback in case the
        # caller passed a triple but cargo didn't actually use --target.
        $altTarget = Join-Path $SourceRoot "target\release"
        if (Test-Path $altTarget) {
            Write-Host "  -> target/$TargetTriple/release/ not found, falling back to target/release/" -ForegroundColor Yellow
            $target = $altTarget
        } else {
            Fail "Neither target/$TargetTriple/release/ nor target/release/ found. Run 'cargo build --release --target $TargetTriple' first."
        }
    }
} else {
    $target = Join-Path $SourceRoot "target\release"
    if (-not (Test-Path $target)) {
        Fail "target/release/ not found at $target. Run 'cargo build --workspace --release' first (or pass -TargetTriple <triple>)."
    }
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
    # A7-012 (2026-05-04): zero-question default. Stage dirs are always
    # safe to overwrite (they're scratch artifacts produced by this
    # script, not user data). Auto-overwrite when stdin is redirected
    # (CI / piped input) -- the prompt would just hang -- and skip the
    # prompt entirely on -Force. The interactive prompt remains for
    # local maintainer runs without -Force, in case the operator
    # accidentally pointed -StageDir at a non-staging directory.
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
# broken (AWS install test 2026-05-02 hit ENOENT for zod). Run bun install
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
    Fail "mcp/node_modules/zod/package.json STILL missing after bun install - refusing to stage broken zip (B2 / 2026-05-02 AWS install bug)"
}
if (-not (Test-Path $sdkPkgJson)) {
    Fail "mcp/node_modules/@modelcontextprotocol/sdk/package.json STILL missing after bun install - refusing to stage broken zip"
}

# A7-019 (2026-05-04): generalise the spot-check above into a manifest-
# driven sweep. Reading mcp/package.json gives us every direct dep +
# peerDep the runtime expects; missing any one of them is a ship blocker
# (the Zod / SDK gaps both started as "the stage script doesn't notice
# bun install aborted partway through"). This catches new deps added
# between releases without requiring this list to be edited in lockstep.
$mcpPkgJsonPath = Join-Path $mcp 'package.json'
if (Test-Path $mcpPkgJsonPath) {
    try {
        $mcpPkg = Get-Content -LiteralPath $mcpPkgJsonPath -Raw | ConvertFrom-Json
        $depNames = @()
        foreach ($section in @('dependencies', 'peerDependencies')) {
            if ($mcpPkg.$section) {
                $depNames += $mcpPkg.$section.PSObject.Properties.Name
            }
        }
        $depNames = $depNames | Where-Object { $_ } | Select-Object -Unique
        $depMissing = @()
        foreach ($d in $depNames) {
            $depPkg = Join-Path $mcp ("node_modules\{0}\package.json" -f $d)
            if (-not (Test-Path -LiteralPath $depPkg)) { $depMissing += $d }
        }
        if ($depMissing.Count -gt 0) {
            Fail ("missing deps after bun install: {0} (refusing to stage broken zip)" -f ($depMissing -join ', '))
        }
        Write-Host ("     OK: {0} npm deps validated against package.json" -f $depNames.Count) -ForegroundColor Green
    } catch {
        Fail ("could not parse mcp/package.json for dep validation: {0}" -f $_.Exception.Message)
    }
} else {
    Fail "mcp/package.json missing -- cannot validate node_modules"
}

# A7-020 (2026-05-04): a partial node_modules where every package.json
# header file is present but the actual code is truncated would still
# pass the per-package check above. Assert the directory is at least 30
# MB so silent half-installs (network drop mid `bun install`) get caught
# before we ship a broken zip. Threshold matches the floor empirically
# observed for v0.3.2 mcp/ (~70 MB after a clean install).
$mcpNmDir = Join-Path $mcp 'node_modules'
if (Test-Path -LiteralPath $mcpNmDir) {
    $nmSize = ((Get-ChildItem -LiteralPath $mcpNmDir -Recurse -File -ErrorAction SilentlyContinue |
                Measure-Object Length -Sum).Sum / 1MB)
    if ($nmSize -lt 30) {
        Fail ("mcp/node_modules is only {0:N1} MB (expected >= 30 MB) - bun install may have aborted" -f $nmSize)
    }
    Write-Host ("     OK: mcp/node_modules is {0:N1} MB (>=30 MB floor)" -f $nmSize) -ForegroundColor Green
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
    # A7-012 (2026-05-04): zip overwrite is a real-data destructive op
    # (the prior shipped artifact). Keep the prompt as a guardrail when
    # stdin is a real TTY, but auto-overwrite under -Force OR a
    # redirected stdin (CI, pipe). The `Read-Host` previously hung
    # forever in CI even though -Force was the documented bypass.
    $autoOverwrite = $Force
    if (-not $autoOverwrite) {
        try {
            if ([Console]::IsInputRedirected) { $autoOverwrite = $true }
        } catch { $autoOverwrite = $false }
    }
    if (-not $autoOverwrite) {
        $reply = Read-Host "Output zip exists: $OutZip. Overwrite? [y/N]"
        if ($reply -notmatch '^(y|yes)$') { Fail "user declined zip overwrite" }
    } else {
        Step "Output zip exists at $OutZip -- auto-overwriting (Force / non-interactive)"
    }
    Remove-Item $OutZip -Force
}
$start = Get-Date
# A7-018 (2026-05-04): switch from Compress-Archive (PS5.1 wrapper) to
# the .NET ZipFile.CreateFromDirectory API. PS5.1's Compress-Archive
# has a 2 GB internal buffer limit; any source tree above that limit
# hits an OutOfMemoryException or silent truncation. The release stage
# is currently ~55 MB so this is a future-proofing fix -- but the
# stage-final-zip.ps1 path with -IncludeModels already approaches 3.5 GB.
Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction SilentlyContinue
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $StageDir,
    $OutZip,
    [System.IO.Compression.CompressionLevel]::Optimal,
    $false)
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
