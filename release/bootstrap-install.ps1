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
#   5. Downloads model assets (bge, qwen-embed, qwen-coder, phi-3) from the
#      Hugging Face mirror (https://huggingface.co/aaditya4u/mneme-models --
#      Cloudflare-backed, ~5x faster than GitHub Releases, no 2 GB asset
#      cap so phi-3 ships as one 2.4 GB file from HF). GitHub Release is
#      a transparent fallback if HF is unreachable; phi-3 is asymmetric
#      because GitHub's 2 GB asset cap forces a split-parts upload there
#      (Get-Phi3-PartsFallback downloads `.part00` + `.part01` and merges
#      them locally). Then installs everything via
#      `mneme models install --from-path`.
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

# A7-004 (2026-05-04): Force UTF-8 console encoding so child mneme.exe
# Unicode glyphs (>=, ok-tick, arrow) render correctly instead of
# mojibake (CP437: GammaEpsilon, Gamma-pound, Gamma-arrow). Wrapped in
# try/catch because some hosts (ISE legacy, ConstrainedLanguageMode)
# reject mutating Console.OutputEncoding at runtime.
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding            = [System.Text.Encoding]::UTF8
} catch { }

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

# A7-013 (2026-05-04): 32-bit Windows refusal upfront.
# lib-common.sh refuses i386/i686 cleanly on Linux/macOS, but bootstrap-install.ps1
# previously had no architecture check. A 32-bit user would download 58 MB of
# x64 binaries then crash with BadImageFormatException at first launch.
# PROCESSOR_ARCHITEW6432 is set on a WoW64 32-bit shell running on a 64-bit OS;
# we treat that as 64-bit (the parent process is 64-bit, the user can launch a
# native 64-bit shell). Pure 32-bit OS gets PROCESSOR_ARCHITECTURE=x86 and
# PROCESSOR_ARCHITEW6432 unset -- the case we refuse.
$procArch  = $env:PROCESSOR_ARCHITECTURE
$procArchW = $env:PROCESSOR_ARCHITEW6432
$is64 = ($procArch -eq 'AMD64') -or ($procArch -eq 'ARM64') -or `
        ($procArchW -eq 'AMD64') -or ($procArchW -eq 'ARM64')
if (-not $is64) {
    Fail ("32-bit Windows is not supported (PROCESSOR_ARCHITECTURE={0}). " -f $procArch +
          "Mneme ships x64 and arm64 binaries only -- upgrade to 64-bit Windows.")
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$releaseBase = "https://github.com/omanishay-cyber/mneme/releases/download/$Version"

# Bug G-14 / SEC-3 (2026-05-01): SHA-256 verification table.
# A7-001 (2026-05-04): replaced inline placeholder hashtable with a
# sidecar `release-checksums.json` fetched from the GH Release alongside
# the binary zip. The maintainer's release pipeline now generates the
# sidecar via `scripts/gen-release-checksums.ps1` after every re-upload,
# eliminating the "edit two files in lockstep" failure mode that left
# the original hashtable empty in shipped v0.3.2.
#
# Without integrity checking, a CDN compromise or interrupted download
# silently delivers garbage that the installer copies to disk and the
# user runs. Files in the manifest MUST match the downloaded bytes --
# mismatch is a hard fail with the file removed. Files NOT in the
# manifest fall through to a "warn-but-continue" path so we don't block
# on assets added between releases.
#
# Manifest format:
#   {
#     "version": "v0.3.2",
#     "generated": "2026-05-04T05:00:00Z",
#     "files": { "<asset-name>": "<sha256-hex>", ... }
#   }
#
# To skip verification entirely (for a hand-cut beta zip), pass
# `-SkipHashCheck` to bootstrap-install.ps1 OR set MNEME_SKIP_HASH_CHECK=1.
$ExpectedHashes = @{}
try {
    $manifestUrl = "$releaseBase/release-checksums.json"
    Step "Fetching SHA-256 manifest: $manifestUrl"
    $prevPP = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        $manifestRaw = Invoke-WebRequest -Uri $manifestUrl -UseBasicParsing -TimeoutSec 10 -ErrorAction Stop
    } finally {
        $ProgressPreference = $prevPP
    }
    $manifest = $manifestRaw.Content | ConvertFrom-Json
    if ($manifest -and $manifest.files) {
        foreach ($prop in $manifest.files.PSObject.Properties) {
            $ExpectedHashes[$prop.Name] = ([string]$prop.Value).ToUpper()
        }
    }
    OK ("loaded SHA-256 manifest: {0} pinned files" -f $ExpectedHashes.Count)
} catch {
    WarnLine "release-checksums.json not available for $Version (continuing with unverified downloads)"
}

function Get-Asset {
    # Wave 6 / 2026-05-02: dual-source download (Hugging Face primary,
    # GitHub Release fallback). For legacy callers that don't pass
    # explicit URLs, $PrimaryUrl auto-derives to the GitHub release
    # path (preserving the old `$releaseBase/$Name` behavior used for
    # the installer zip itself).
    #
    # Wave 6 follow-up / 2026-05-02: phi-3 cannot use the auto-derived
    # GitHub fallback because the merged file is 2.28 GB and GitHub
    # Releases caps individual assets at 2 GB. For phi-3 the caller
    # passes `-NoAutoFallback` so HF stays the only single-file source;
    # the parts-based GitHub fallback runs separately via
    # `Get-Phi3-PartsFallback` after the main asset loop.
    param(
        [string]$Name,
        [string]$Dest,
        [int]$RetryCount = 3,
        [string]$PrimaryUrl = $null,
        [string]$FallbackUrl = $null,
        [switch]$NoAutoFallback
    )

    # B5: silence Invoke-WebRequest "Writing web request" progress chatter.
    # Local scope so this only affects per-call IWRs in this function,
    # without polluting the caller's $ProgressPreference.
    $ProgressPreference = 'SilentlyContinue'

    # Default $PrimaryUrl to the GitHub release URL for backward
    # compatibility (used by the release-zip download in Step 1).
    if (-not $PrimaryUrl) { $PrimaryUrl = "$releaseBase/$Name" }

    # Default $FallbackUrl to the GitHub release URL when the caller
    # supplied a $PrimaryUrl that isn't already the release URL --
    # i.e., HF primary + GitHub fallback for the model downloads.
    # Skip auto-derive when the caller explicitly opts out via
    # -NoAutoFallback (used for phi-3, where the GitHub asset is split
    # into two parts and downloaded via Get-Phi3-PartsFallback instead).
    if (-not $FallbackUrl -and -not $NoAutoFallback -and $PrimaryUrl -ne "$releaseBase/$Name") {
        $FallbackUrl = "$releaseBase/$Name"
    }

    # Build the source list: primary always, fallback only if distinct.
    $sources = @(
        @{ Url = $PrimaryUrl; Label = 'primary' }
    )
    if ($FallbackUrl -and $FallbackUrl -ne $PrimaryUrl) {
        $sources += @{ Url = $FallbackUrl; Label = 'fallback' }
    }

    foreach ($src in $sources) {
        $url = $src.Url
        $label = $src.Label
        for ($attempt = 1; $attempt -le $RetryCount; $attempt++) {
            try {
                Step "Fetching $Name from $label (attempt $attempt/$RetryCount): $url"
                # A7-014 (2026-05-04): -PassThru returns the response object
                # so we can read the Content-Length header and gate the
                # download on an exact-size match. The original >=100-byte
                # sanity check passes any partial download >=100 bytes
                # (e.g. a CDN drop mid-stream of the 58 MB zip), which
                # then hands a corrupt zip to Expand-Archive and silently
                # produces a half-installed ~/.mneme. With Content-Length
                # available on every GitHub Release + HF asset, an exact
                # match closes that window. Falls through to the legacy
                # >=100-byte sanity check if Content-Length is absent
                # (e.g. transfer-encoding: chunked).
                $resp = Invoke-WebRequest -Uri $url -OutFile $Dest -UseBasicParsing -PassThru
                $sz = (Get-Item $Dest).Length
                $expectedLen = -1
                try {
                    $cl = $null
                    if ($resp -and $resp.Headers) {
                        # On PS5.1 the Headers dict can be case-sensitive;
                        # PS7 returns string[] for each header. Try both.
                        $cl = $resp.Headers['Content-Length']
                        if (-not $cl) { $cl = $resp.Headers['content-length'] }
                    }
                    if ($cl) {
                        $clStr = if ($cl -is [array]) { $cl[0] } else { [string]$cl }
                        $expectedLen = [int64]$clStr
                    }
                } catch { $expectedLen = -1 }
                if ($expectedLen -gt 0 -and $sz -ne $expectedLen) {
                    Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
                    throw "size mismatch for $Name (expected $expectedLen bytes, got $sz) -- truncated download"
                }
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
                OK "downloaded $Name ($mb MB) from $label"
                return
            } catch {
                WarnLine "attempt $attempt ($label) failed: $_"
                if ($attempt -eq $RetryCount) {
                    # Remove any partial file so the next source / next
                    # call doesn't trust a half-finished download.
                    Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
                    if ($src -eq $sources[-1]) {
                        # Last source exhausted -- bubble up.
                        throw $_
                    } else {
                        WarnLine "$label exhausted -- trying fallback source"
                        break
                    }
                }
                Start-Sleep -Seconds (2 * $attempt)
            }
        }
    }
}

function Get-Phi3-PartsFallback {
    # Wave 6 follow-up / 2026-05-02: phi-3 GitHub fallback path.
    #
    # GitHub Releases caps an individual asset at 2 GB. The merged
    # phi-3-mini-4k.gguf is 2.28 GB, so it cannot be uploaded as a
    # single file -- the v0.3.2 release ships it as two equal halves
    # named `phi-3-mini-4k.gguf.part00` and `phi-3-mini-4k.gguf.part01`
    # (1196615536 bytes each, byte-symmetric split). HF does NOT have
    # this cap so the primary path stays a single-file download; this
    # helper only runs when HF is unreachable.
    #
    # Why concatenate here instead of letting `mneme models install
    # --from-path` see the parts directly? `install_from_path_to_root`
    # in cli/src/commands/models.rs DOES merge `.part0N` glob patterns
    # at install time, but doing the concat here keeps the failure
    # mode obvious (one file present at $Dest = one asset present)
    # and matches what the HF primary path produces, so the rest of
    # the bootstrap doesn't need to special-case how phi-3 arrived.
    param(
        [string]$Dest,
        [int]$RetryCount = 3
    )

    $tmp = Split-Path -Parent $Dest
    $p00 = Join-Path $tmp 'phi-3-mini-4k.gguf.part00'
    $p01 = Join-Path $tmp 'phi-3-mini-4k.gguf.part01'

    Step "phi-3 GitHub fallback: downloading split parts (HF single-file unreachable)"

    # Download both parts WITH -NoAutoFallback so Get-Asset doesn't
    # try to derive a (nonexistent) HF URL for the parts -- the parts
    # only live on GitHub Releases, by design.
    Get-Asset -Name 'phi-3-mini-4k.gguf.part00' `
              -Dest $p00 -RetryCount $RetryCount `
              -PrimaryUrl "$releaseBase/phi-3-mini-4k.gguf.part00" `
              -NoAutoFallback
    Get-Asset -Name 'phi-3-mini-4k.gguf.part01' `
              -Dest $p01 -RetryCount $RetryCount `
              -PrimaryUrl "$releaseBase/phi-3-mini-4k.gguf.part01" `
              -NoAutoFallback

    # Concatenate part00 + part01 -> $Dest. We use raw FileStream I/O
    # rather than `Get-Content -Raw + Set-Content -Raw` because the
    # latter buffers the whole file (~2.4 GB) in memory and OOMs on
    # 8 GB / 16 GB laptops.
    Step "phi-3 GitHub fallback: merging parts -> $Dest"
    $expectedTotal = 2393231072
    $out = [System.IO.File]::Create($Dest)
    $buf = New-Object byte[] 1048576
    try {
        foreach ($p in @($p00, $p01)) {
            $in = [System.IO.File]::OpenRead($p)
            try {
                while ($true) {
                    $n = $in.Read($buf, 0, $buf.Length)
                    if ($n -le 0) { break }
                    $out.Write($buf, 0, $n)
                }
            } finally {
                $in.Close()
            }
        }
    } finally {
        $out.Close()
    }

    $actual = (Get-Item $Dest).Length
    if ($actual -ne $expectedTotal) {
        Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
        throw "phi-3 GitHub fallback: merged file size mismatch (expected $expectedTotal, got $actual)"
    }

    # Tidy up parts on disk -- they're not needed once merged. The
    # merged file at $Dest is what `mneme models install --from-path`
    # consumes; keeping the parts wastes ~2.3 GB of $env:TEMP.
    Remove-Item -LiteralPath $p00 -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $p01 -Force -ErrorAction SilentlyContinue

    OK ("phi-3 merged from GitHub parts ({0:N0} bytes)" -f $actual)
}

# ---------------------------------------------------------------------------
# Step 1: Download the release zip
# ---------------------------------------------------------------------------
# A7-023 (2026-05-04): wrap the body of the installer (Steps 1-5) in
# try/finally so the temp dir is cleaned up on ANY failure path -- not
# just the happy path. Previously a failure mid-download or mid-extract
# left ~3.5 GB of partial models + zip in $env:TEMP\mneme-bootstrap-* on
# every aborted run; across many failed install attempts this filled
# the user's disk silently.
Section "Download release zip"
$zipName = "mneme-$Version-windows-x64.zip"
$tmpDir = Join-Path $env:TEMP "mneme-bootstrap-$Version"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
try {
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
    # A7-003 (2026-05-04): clear orphaned `.partNN` (or any other model
    # leftovers) from a previous failed install. New-Item -Force is
    # idempotent on the directory but does NOT touch existing files
    # inside it, so a leftover phi-3-mini-4k.gguf.part00 from a prior
    # crashed run survives and triggers the cosmetic "only 1 part(s);
    # expected >=2" warning when `mneme models install --from-path`
    # later globs the dir. Clearing the dir is safe -- the assets
    # listed below are re-fetched fresh in this same step.
    Get-ChildItem -LiteralPath $modelsDir -Force -ErrorAction SilentlyContinue |
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

    # Asset list -- names must match what `mneme models install
    # --from-path` expects on disk. Each entry has:
    #   Name        - on-disk filename (also the install-time identifier)
    #   Required    - if download fails for a required asset, smart
    #                 embeddings are degraded but bootstrap continues
    #   PrimaryUrl  - Hugging Face mirror (Cloudflare-backed, fast,
    #                 free unlimited public bandwidth, no 2 GB cap)
    #   FallbackUrl - if $null, Get-Asset auto-derives the GitHub
    #                 release URL ($releaseBase/$Name) as a fallback
    #                 for resilience when HF is unreachable
    #
    # Wave 6 / 2026-05-02: model downloads switched from GitHub
    # Releases (Azure Blob, 5-50 MB/s) to Hugging Face Hub
    # (Cloudflare, 50-200 MB/s, ~5x faster). HF has no 2 GB asset
    # cap, so phi-3-mini-4k.gguf ships as one ~2.4 GB file from HF.
    # GitHub Releases DOES cap individual assets at 2 GB, so on
    # GitHub phi-3 ships as two ~1.14 GB parts (`.part00` + `.part01`)
    # which `Get-Phi3-PartsFallback` downloads + concatenates. The
    # Rust-side part-merge logic in
    # `cli/src/commands/models.rs::install_from_path_to_root` is
    # retained for v0.3.2 backwards-compat (it gracefully no-ops when
    # the input is already a single file).
    #
    # Why asymmetric (HF=single file, GitHub=parts) instead of using
    # parts on both? It saves a one-time 2.4 GB upload to HF, keeps
    # the HF code path identical to the other 4 assets, and matches
    # how the v0.3.2 release was actually shipped.
    $assets = @(
        @{
            Name = 'bge-small-en-v1.5.onnx';
            Required = $true;
            PrimaryUrl = 'https://huggingface.co/aaditya4u/mneme-models/resolve/main/bge-small-en-v1.5.onnx';
            FallbackUrl = $null;
            NoAutoFallback = $false;
            PartsFallback = $false
        },
        @{
            Name = 'tokenizer.json';
            Required = $true;
            PrimaryUrl = 'https://huggingface.co/aaditya4u/mneme-models/resolve/main/tokenizer.json';
            FallbackUrl = $null;
            NoAutoFallback = $false;
            PartsFallback = $false
        },
        @{
            Name = 'qwen-embed-0.5b.gguf';
            Required = $false;
            PrimaryUrl = 'https://huggingface.co/aaditya4u/mneme-models/resolve/main/qwen-embed-0.5b.gguf';
            FallbackUrl = $null;
            NoAutoFallback = $false;
            PartsFallback = $false
        },
        @{
            Name = 'qwen-coder-0.5b.gguf';
            Required = $false;
            PrimaryUrl = 'https://huggingface.co/aaditya4u/mneme-models/resolve/main/qwen-coder-0.5b.gguf';
            FallbackUrl = $null;
            NoAutoFallback = $false;
            PartsFallback = $false
        },
        @{
            # phi-3: HF single-file primary; GitHub split-parts
            # fallback is handled by Get-Phi3-PartsFallback (NOT by
            # Get-Asset's auto-derived release URL, because that URL
            # 404s -- the merged 2.28 GB file exceeds GitHub's 2 GB
            # asset cap, so only `.part00` + `.part01` exist there).
            Name = 'phi-3-mini-4k.gguf';
            Required = $false;
            PrimaryUrl = 'https://huggingface.co/aaditya4u/mneme-models/resolve/main/phi-3-mini-4k.gguf';
            FallbackUrl = $null;
            NoAutoFallback = $true;
            PartsFallback = $true
        }
    )

    $modelDownloads = 0
    $modelFailures  = @()
    foreach ($a in $assets) {
        $dest = Join-Path $modelsDir $a.Name
        try {
            $callArgs = @{
                Name = $a.Name
                Dest = $dest
                RetryCount = 3
                PrimaryUrl = $a.PrimaryUrl
                FallbackUrl = $a.FallbackUrl
            }
            if ($a.NoAutoFallback) { $callArgs['NoAutoFallback'] = $true }
            Get-Asset @callArgs
            $modelDownloads += 1
        } catch {
            # phi-3 has a dedicated GitHub parts fallback path. Try
            # it before recording the failure -- if it succeeds, the
            # asset is still present at $dest and the rest of the
            # install proceeds normally.
            if ($a.PartsFallback) {
                try {
                    Get-Phi3-PartsFallback -Dest $dest -RetryCount 3
                    $modelDownloads += 1
                    continue
                } catch {
                    WarnLine "phi-3 GitHub parts fallback also failed: $_"
                }
            }
            $modelFailures += $a.Name
            if ($a.Required) {
                WarnLine "REQUIRED asset $($a.Name) failed -- smart embeddings will be unavailable"
            } else {
                WarnLine "optional asset $($a.Name) failed -- corresponding capability disabled"
            }
        }
    }
    OK "downloaded $modelDownloads / $($assets.Count) model assets ($(($modelFailures | Measure-Object).Count) failed)"

    # NOTE (Wave 6 follow-up, 2026-05-02): phi-3 ships asymmetrically
    # -- one 2.4 GB file on HF (the fast primary path), and two ~1.14
    # GB split parts on GitHub Releases (the resilient fallback path,
    # since GitHub's 2 GB asset cap rules out the merged file). The
    # GitHub path goes through Get-Phi3-PartsFallback above, which
    # downloads both parts + concatenates them at $dest before
    # `mneme models install --from-path` runs. The merge code in
    # `cli/src/commands/models.rs::install_from_path_to_root` no-ops
    # on already-merged input so it's safe either way.

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
        # Bug-2026-05-02 (AWS install regression cycle): same root cause
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
} finally {
    # A7-023: always cleanup unless user opted in to -KeepDownload.
    # Runs on the happy path AND on every Fail/throw above so partial
    # downloads do NOT accumulate in $env:TEMP across retried installs.
    if (-not $KeepDownload) {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
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

# B11 (2026-05-02): make the PATH-just-applied warning the LAST visible
# block. The bootstrap installer stuffed a one-line yellow note among
# half a dozen other yellow OK / WARN lines and users skim past it,
# then hit "mneme: command not found" running `mneme doctor` in the
# same shell. Boxed banner survives the skim.
Write-Host ""
Write-Host "  +---------------------------------------------------------+" -ForegroundColor Yellow
Write-Host "  |  IMPORTANT: open a NEW PowerShell terminal before       |" -ForegroundColor Yellow
Write-Host "  |  running 'mneme doctor' or 'mneme build' -- the PATH    |" -ForegroundColor Yellow
Write-Host "  |  change just applied is not visible in this session.    |" -ForegroundColor Yellow
Write-Host "  +---------------------------------------------------------+" -ForegroundColor Yellow
Write-Host ""
