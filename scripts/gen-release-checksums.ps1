#requires -Version 5.1
# scripts/gen-release-checksums.ps1
# ----------------------------------
# Generate the `release-checksums.json` sidecar consumed by:
#   - release/bootstrap-install.ps1     (Windows)
#   - release/install-mac.sh            (macOS)
#   - release/install-linux.sh          (Linux)
#   - release/lib-common.sh::load_hash_manifest
#
# A7-001 (2026-05-04): added so the v0.3.2 hotfix re-ship has integrity
# verification on every download. The shipped bootstrap previously had
# an empty `$ExpectedHashes = @{}` placeholder, leaving every download
# unverified.
#
# Usage:
#   .\scripts\gen-release-checksums.ps1 `
#       -StageDir "C:\Users\Anish\Desktop\release-stage" `
#       -OutFile  "C:\Users\Anish\Desktop\release-stage\release-checksums.json" `
#       -Version  "v0.3.2"
#
# Or with an explicit file list (useful when models live in a sibling dir):
#   .\scripts\gen-release-checksums.ps1 `
#       -OutFile "release-checksums.json" `
#       -Version "v0.3.2" `
#       -Files @("zips\mneme-v0.3.2-windows-x64.zip", "models\bge-small-en-v1.5.onnx", ...)
#
# Manifest format (consumed by all three install scripts):
#   {
#     "version":   "v0.3.2",
#     "generated": "<ISO-8601 UTC>",
#     "files": {
#       "<asset-name>": "<UPPERCASE-SHA256-HEX>",
#       ...
#     }
#   }
#
# Asset name keys are file basenames (without directory) so the bash
# parser doesn't have to deal with platform path separators. Hex is
# uppercase to match `Get-FileHash` output and PowerShell's case-folding
# expectations in `bootstrap-install.ps1::Get-Asset`.
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

[CmdletBinding(DefaultParameterSetName = 'StageDir')]
param(
    [Parameter(ParameterSetName = 'StageDir', Mandatory = $true)]
    [string]$StageDir,

    [Parameter(ParameterSetName = 'FileList', Mandatory = $true)]
    [string[]]$Files,

    [Parameter(Mandatory = $true)]
    [string]$OutFile,

    [Parameter(Mandatory = $true)]
    [string]$Version,

    # Optional filename glob filter when in StageDir mode. Defaults match
    # the canonical mneme release artifact set: zip + tar.gz binaries +
    # model files. Adjust if a release ships extras.
    [string[]]$IncludePatterns = @(
        'mneme-*-*.zip',
        'mneme-*-*.tar.gz',
        '*.onnx',
        'tokenizer.json',
        '*.gguf',
        '*.gguf.part*'
    )
)

$ErrorActionPreference = 'Stop'

function Section($name) { Write-Host "" -NoNewline; Write-Host ("== $name ==") -ForegroundColor Cyan }
function OK($msg)       { Write-Host "  OK: $msg" -ForegroundColor Green }
function Step($msg)     { Write-Host "  -> $msg" -ForegroundColor Yellow }
function WarnLine($msg) { Write-Host "  WARN: $msg" -ForegroundColor DarkYellow }
function Fail($msg)     { Write-Host "  FAIL: $msg" -ForegroundColor Red; throw $msg }

Section "gen-release-checksums.ps1"
Write-Host "  version : $Version"
Write-Host "  output  : $OutFile"

# Build the file list either from -StageDir + filters, or directly from -Files.
$resolved = @()
if ($PSCmdlet.ParameterSetName -eq 'StageDir') {
    if (-not (Test-Path -LiteralPath $StageDir -PathType Container)) {
        Fail "StageDir does not exist: $StageDir"
    }
    Step "Scanning $StageDir for release artifacts"
    foreach ($pat in $IncludePatterns) {
        $matches = @(Get-ChildItem -LiteralPath $StageDir -File -Filter $pat -ErrorAction SilentlyContinue)
        $resolved += $matches
    }
    # De-duplicate by full path (a file may match multiple patterns).
    $resolved = $resolved | Sort-Object -Property FullName -Unique
} else {
    Step "Hashing explicit -Files list ({0} entries)" -f $Files.Count
    foreach ($f in $Files) {
        if (-not (Test-Path -LiteralPath $f -PathType Leaf)) {
            Fail "file not found: $f"
        }
        $resolved += Get-Item -LiteralPath $f
    }
}

if ($resolved.Count -eq 0) {
    Fail "no files matched. In StageDir mode, IncludePatterns may be too narrow; in Files mode, the list was empty."
}

# Compute SHA-256 per file and collect into an ordered hashtable so
# ConvertTo-Json preserves stable key order.
$files = [ordered]@{}
foreach ($f in $resolved) {
    Step ("hashing {0} ({1:N1} MB)" -f $f.Name, ($f.Length / 1MB))
    $h = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash.ToUpper()
    if ($files.Contains($f.Name)) {
        Fail ("duplicate file basename: {0} (keys must be unique; rename one)" -f $f.Name)
    }
    $files[$f.Name] = $h
    OK ("  {0,-50} {1}" -f $f.Name, $h)
}

$manifest = [ordered]@{
    version   = $Version
    generated = (Get-Date).ToUniversalTime().ToString("o")
    files     = $files
}

# Ensure parent dir exists.
$outDir = Split-Path -Parent $OutFile
if ($outDir -and -not (Test-Path -LiteralPath $outDir)) {
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
}

# Write atomically: temp file + Move-Item (NTFS atomic on same volume).
$tmp = "$OutFile.tmp"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $tmp -Encoding UTF8 -Force
Move-Item -LiteralPath $tmp -Destination $OutFile -Force

OK ("wrote manifest: {0} ({1} files)" -f $OutFile, $files.Count)
Section "done"
