param(
    [string]$ProjectDir = '<project root>',
    [string]$OutputDir = '<home>\Desktop\mcp-bench-results',
    [string]$RunnerPath = '<home>\Desktop\run-query.ps1',
    [string]$ProgressLog = '<home>\Desktop\mcp-bench-progress.log',
    [int]$TimeoutSec = 240
)

# Configure
$mcps = @(
    @{ name = 'mneme';             config = '<home>/Desktop/mcp-mneme-only.json' },
    @{ name = 'tree-sitter';       config = '<home>/Desktop/mcp-treesitter-only.json' },
    @{ name = 'code-review-graph'; config = '<home>/Desktop/mcp-crg-only.json' },
    @{ name = 'graphify';          config = '<home>/Desktop/mcp-graphify-only.json' }
)

$queries = @(
    @{ id = 'Q1'; prompt = 'Find all functions related to authentication in this project. List each function with its file path and a 1-line description. Cite the actual files you used.' },
    @{ id = 'Q2'; prompt = 'What is the blast radius of changing src/utils/auth.ts in this project? List every file that would need to be re-tested or updated if its public API changed. Cite each file with reasoning.' },
    @{ id = 'Q3'; prompt = 'Show me the call graph for the login flow in this internal-app app. Start from the LoginPage component and follow every function/IPC/store call until you reach the data layer. Output as an indented tree.' },
    @{ id = 'Q4'; prompt = 'What design patterns are used in this project? For each pattern, name it and cite at least one concrete file/function where it appears.' },
    @{ id = 'Q5'; prompt = 'Find any security issues in the auth implementation of this Electron + React + TypeScript app. For each issue cite the exact file:line and explain the vulnerability concretely (no generic advice).' }
)

# Filter MCPs by env var: BENCH_MCPS = comma-separated list (e.g. "tree-sitter,code-review-graph,graphify")
if ($env:BENCH_MCPS) {
    $allowed = $env:BENCH_MCPS -split ','
    $mcps = $mcps | Where-Object { $_.name -in $allowed }
    Write-Host "Filtered MCPs by BENCH_MCPS: $($mcps.name -join ', ')"
}
# Filter queries by env var: BENCH_QUERIES = comma-separated list (e.g. "Q1,Q3,Q5")
if ($env:BENCH_QUERIES) {
    $qAllowed = $env:BENCH_QUERIES -split ','
    $queries = $queries | Where-Object { $_.id -in $qAllowed }
    Write-Host "Filtered queries by BENCH_QUERIES: $($queries.id -join ', ')"
}

# Reset progress log
"--- BENCH RUN START $(Get-Date -Format o) ---" | Set-Content -Path $ProgressLog
$total = $mcps.Count * $queries.Count
$counter = 0
$results = @()

foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $counter++
        $line = "[$counter/$total] $($mcp.name) $($q.id)  start=$(Get-Date -Format o)"
        $line | Add-Content -Path $ProgressLog
        Write-Host $line
        try {
            $sw = [System.Diagnostics.Stopwatch]::StartNew()
            $out = & $RunnerPath -McpName $mcp.name -McpConfigPath $mcp.config -QueryId $q.id -Prompt $q.prompt -ProjectDir $ProjectDir -OutputDir $OutputDir -TimeoutSec $TimeoutSec 2>&1 | Out-String
            $sw.Stop()
            $line2 = "[$counter/$total] $($mcp.name) $($q.id)  done in $([int]$sw.Elapsed.TotalSeconds)s  -> $out"
            $line2 | Add-Content -Path $ProgressLog
            Write-Host $line2
            $results += [PSCustomObject]@{
                mcp = $mcp.name
                query = $q.id
                wall_sec = [int]$sw.Elapsed.TotalSeconds
                output_file = (Join-Path $OutputDir "$($mcp.name)-$($q.id).json")
            }
        } catch {
            $errLine = "[$counter/$total] $($mcp.name) $($q.id)  FAILED: $_"
            $errLine | Add-Content -Path $ProgressLog
            Write-Host $errLine
            $results += [PSCustomObject]@{
                mcp = $mcp.name
                query = $q.id
                wall_sec = -1
                output_file = ''
                error = $_.ToString()
            }
        }
    }
}

# Write summary
$summary = $results | ConvertTo-Json -Depth 10
Set-Content -Path (Join-Path $OutputDir 'summary.json') -Value $summary -Encoding utf8
"--- BENCH RUN END $(Get-Date -Format o) ---" | Add-Content -Path $ProgressLog
Write-Host "DONE. Summary at $OutputDir\summary.json"
