param(
    [Parameter(Mandatory=$true)][string]$McpName,
    [Parameter(Mandatory=$true)][string]$McpConfigPath,
    [Parameter(Mandatory=$true)][string]$QueryId,
    [Parameter(Mandatory=$true)][string]$Prompt,
    [Parameter(Mandatory=$true)][string]$ProjectDir,
    [Parameter(Mandatory=$true)][string]$OutputDir,
    [int]$TimeoutSec = 240
)

# Setup PATH
$env:Path = [System.Environment]::GetEnvironmentVariable('Path','User') + ';' + [System.Environment]::GetEnvironmentVariable('Path','Machine')

Set-Location -LiteralPath $ProjectDir
$outFile = Join-Path $OutputDir "$McpName-$QueryId.json"
$errFile = Join-Path $OutputDir "$McpName-$QueryId.err.txt"

# Augment prompt to force MCP-only answers
$augmentedPrompt = "$Prompt`n`nCRITICAL CONSTRAINT: You MUST answer this using ONLY the MCP server tools available to you (their names start with mcp__). Do NOT use the built-in Bash, Read, Grep, Glob tools. If the MCP server cannot answer, say so explicitly. The current working directory is the project root."

$sw = [System.Diagnostics.Stopwatch]::StartNew()

# Resolve direct path to claude.exe (claude.cmd uses %* which mangles spaces)
$claudeExe = "$env:APPDATA\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe"
if (-not (Test-Path $claudeExe)) {
    $claudeCmd = Get-Command claude.cmd -ErrorAction SilentlyContinue
    if ($claudeCmd) {
        $cmdContent = Get-Content $claudeCmd.Source -Raw
        $matchObj = [regex]::Match($cmdContent, '"([^"]+claude\.exe)"')
        if ($matchObj.Success) { $claudeExe = $matchObj.Groups[1].Value -replace '%dp0%', (Split-Path $claudeCmd.Source) }
    }
}
if (-not (Test-Path $claudeExe)) { throw "claude.exe not found (tried $claudeExe)" }
$claudeCmd = @{ Source = $claudeExe }

# Capture stdout & stderr separately (UNIQUE files per call)
$stdoutFile = [System.IO.Path]::GetTempFileName()
$stderrFile = [System.IO.Path]::GetTempFileName()
$stdinFile  = [System.IO.Path]::GetTempFileName()

# Use [System.IO.File]::WriteAllBytes for fully deterministic single-shot write (no encoding BOM)
[System.IO.File]::WriteAllBytes($stdinFile, [System.Text.Encoding]::UTF8.GetBytes($augmentedPrompt))

# Debug: also save the actual prompt so we can verify what reached stdin
$promptDumpFile = Join-Path $OutputDir "$McpName-$QueryId.prompt.txt"
[System.IO.File]::WriteAllBytes($promptDumpFile, [System.Text.Encoding]::UTF8.GetBytes($augmentedPrompt))

# Build minimal arg list. Note: even without -p, claude reads prompt from stdin when --print is used.
# --no-session-persistence: each query is a fresh conversation (no carry-over from prior runs)
# --bare: skip hooks, plugin sync, attribution, auto-memory, CLAUDE.md auto-discovery -- prevents mneme's
# auto-trigger hooks from injecting cross-query context that would taint per-MCP measurements.
$freshUuid = [guid]::NewGuid().ToString()
$argList = @(
    '--print',
    '--input-format', 'text',
    '--strict-mcp-config',
    '--mcp-config', $McpConfigPath,
    '--output-format', 'json',
    '--dangerously-skip-permissions',
    '--no-session-persistence',
    '--session-id', $freshUuid,
    '--setting-sources', 'user',
    '--add-dir', $ProjectDir
)

$proc = Start-Process -FilePath $claudeCmd.Source `
    -ArgumentList $argList `
    -NoNewWindow `
    -PassThru `
    -WorkingDirectory $ProjectDir `
    -RedirectStandardOutput $stdoutFile `
    -RedirectStandardError $stderrFile `
    -RedirectStandardInput $stdinFile
$completed = $proc.WaitForExit([int]($TimeoutSec * 1000))
if (-not $completed) {
    $proc.Kill()
    Write-Output "MCP=$McpName QUERY=$QueryId STATUS=TIMEOUT WALL_MS=$([int]$sw.Elapsed.TotalMilliseconds)"
    Set-Content -Path $outFile -Value '{"timeout":true}' -Encoding utf8
    return
}
$sw.Stop()

$stdout = Get-Content -LiteralPath $stdoutFile -Raw -ErrorAction SilentlyContinue
$stderr = Get-Content -LiteralPath $stderrFile -Raw -ErrorAction SilentlyContinue

if (-not [string]::IsNullOrWhiteSpace($stdout)) {
    Set-Content -Path $outFile -Value $stdout -Encoding utf8
} else {
    Set-Content -Path $outFile -Value '{"empty":true}' -Encoding utf8
}
if (-not [string]::IsNullOrWhiteSpace($stderr)) {
    Set-Content -Path $errFile -Value $stderr -Encoding utf8
}

Remove-Item $stdoutFile, $stderrFile, $stdinFile -ErrorAction SilentlyContinue

$wallMs = [int]$sw.Elapsed.TotalMilliseconds
Write-Output "MCP=$McpName QUERY=$QueryId EXIT=$($proc.ExitCode) WALL_MS=$wallMs OUTPUT_FILE=$outFile"
