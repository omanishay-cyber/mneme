param(
    [Parameter(Mandatory=$true)][string]$McpName,
    [Parameter(Mandatory=$true)][string]$McpConfigPath,
    [Parameter(Mandatory=$true)][string]$QueryId,
    [Parameter(Mandatory=$true)][string]$Prompt,
    [Parameter(Mandatory=$true)][string]$ProjectDir,
    [Parameter(Mandatory=$true)][string]$OutputDir,
    [int]$TimeoutSec = 600
)

# Setup PATH (User + Machine, plus python scripts dir for graphify/CRG)
$env:Path = [System.Environment]::GetEnvironmentVariable('Path','User') + ';' + [System.Environment]::GetEnvironmentVariable('Path','Machine') + ';C:\Users\Anish\AppData\Roaming\Python\Python314\Scripts;C:\Users\Anish\AppData\Local\Microsoft\WinGet\Links'

Set-Location -LiteralPath $ProjectDir
$outFile = Join-Path $OutputDir "$McpName-$QueryId.json"
$errFile = Join-Path $OutputDir "$McpName-$QueryId.err.txt"

# Augment prompt to force MCP-only answers
$augmentedPrompt = "$Prompt`n`nCRITICAL CONSTRAINT: You MUST answer this using ONLY the MCP server tools available to you (their names start with mcp__). Do NOT use the built-in Bash, Read, Grep, Glob tools. If the MCP server cannot answer, say so explicitly. The current working directory is the project root."

$sw = [System.Diagnostics.Stopwatch]::StartNew()

# Resolve direct path to claude.exe
$claudeExe = "$env:APPDATA\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe"
if (-not (Test-Path $claudeExe)) {
    $claudeCmd = Get-Command claude.cmd -ErrorAction SilentlyContinue
    if ($claudeCmd) {
        $cmdContent = Get-Content $claudeCmd.Source -Raw
        $matchObj = [regex]::Match($cmdContent, '"([^"]+claude\.exe)"')
        if ($matchObj.Success) { $claudeExe = $matchObj.Groups[1].Value -replace '%dp0%', (Split-Path $claudeCmd.Source) }
    }
}
if (-not (Test-Path $claudeExe)) {
    $claudeCmd2 = Get-Command claude -ErrorAction SilentlyContinue
    if ($claudeCmd2) { $claudeExe = $claudeCmd2.Source }
}
if (-not (Test-Path $claudeExe)) { throw "claude.exe not found (tried $claudeExe)" }

$stdoutFile = [System.IO.Path]::GetTempFileName()
$stderrFile = [System.IO.Path]::GetTempFileName()
$stdinFile  = [System.IO.Path]::GetTempFileName()

[System.IO.File]::WriteAllBytes($stdinFile, [System.Text.Encoding]::UTF8.GetBytes($augmentedPrompt))

$promptDumpFile = Join-Path $OutputDir "$McpName-$QueryId.prompt.txt"
[System.IO.File]::WriteAllBytes($promptDumpFile, [System.Text.Encoding]::UTF8.GetBytes($augmentedPrompt))

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

$proc = Start-Process -FilePath $claudeExe `
    -ArgumentList $argList `
    -NoNewWindow `
    -PassThru `
    -WorkingDirectory $ProjectDir `
    -RedirectStandardOutput $stdoutFile `
    -RedirectStandardError $stderrFile `
    -RedirectStandardInput $stdinFile
$completed = $proc.WaitForExit([int]($TimeoutSec * 1000))
if (-not $completed) {
    try { $proc.Kill() } catch {}
    $wallMs = [int]$sw.Elapsed.TotalMilliseconds
    Write-Output "MCP=$McpName QUERY=$QueryId STATUS=TIMEOUT_KILLED WALL_MS=$wallMs"
    # Capture measured envelope with the wall time and 0 markers.
    $stub = @{
        timeout_killed = $true
        duration_ms = $wallMs
        result = "TIMEOUT_AFTER_${TimeoutSec}S: MCP server did not return final answer in budget. Counted as 0 ground-truth markers."
        usage = @{ output_tokens = 0 }
        total_cost_usd = 0
        num_turns = 0
    } | ConvertTo-Json -Compress
    Set-Content -Path $outFile -Value $stub -Encoding utf8
    Remove-Item $stdoutFile, $stderrFile, $stdinFile -ErrorAction SilentlyContinue
    return
}
$sw.Stop()

$stdout = Get-Content -LiteralPath $stdoutFile -Raw -ErrorAction SilentlyContinue
$stderr = Get-Content -LiteralPath $stderrFile -Raw -ErrorAction SilentlyContinue

if (-not [string]::IsNullOrWhiteSpace($stdout)) {
    Set-Content -Path $outFile -Value $stdout -Encoding utf8
} else {
    $wallMs = [int]$sw.Elapsed.TotalMilliseconds
    $stub = @{
        empty_stdout = $true
        duration_ms = $wallMs
        result = "EMPTY_STDOUT_EXIT_$($proc.ExitCode): Claude returned no JSON envelope. Treated as 0 markers."
        usage = @{ output_tokens = 0 }
        total_cost_usd = 0
        num_turns = 0
    } | ConvertTo-Json -Compress
    Set-Content -Path $outFile -Value $stub -Encoding utf8
}
if (-not [string]::IsNullOrWhiteSpace($stderr)) {
    Set-Content -Path $errFile -Value $stderr -Encoding utf8
}

Remove-Item $stdoutFile, $stderrFile, $stdinFile -ErrorAction SilentlyContinue

$wallMs = [int]$sw.Elapsed.TotalMilliseconds
Write-Output "MCP=$McpName QUERY=$QueryId EXIT=$($proc.ExitCode) WALL_MS=$wallMs OUTPUT_FILE=$outFile"
