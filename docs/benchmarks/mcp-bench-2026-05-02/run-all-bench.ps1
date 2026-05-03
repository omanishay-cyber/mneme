param(
    [string]$BenchDir = 'C:\Users\Anish\Desktop\temp\mcp-bench-2026-05-02',
    [string]$ProjectDir = 'D:\Mneme Dome\Mneme-Home-Handoff-2026-04-30-2027\source',
    [int]$TimeoutSec = 240
)

$OutputDir = Join-Path $BenchDir 'results'
$RunnerPath = Join-Path $BenchDir 'run-query.ps1'
$ProgressLog = Join-Path $BenchDir 'progress.log'

$mcps = @(
    @{ name = 'mneme';             config = (Join-Path $BenchDir 'mcp-mneme-only.json') },
    @{ name = 'tree-sitter';       config = (Join-Path $BenchDir 'mcp-treesitter-only.json') },
    @{ name = 'code-review-graph'; config = (Join-Path $BenchDir 'mcp-crg-only.json') },
    @{ name = 'graphify';          config = (Join-Path $BenchDir 'mcp-graphify-only.json') }
)

$queriesObj = Get-Content (Join-Path $BenchDir 'queries.json') -Raw | ConvertFrom-Json
$queries = $queriesObj.queries

if ($env:BENCH_MCPS) {
    $allowed = $env:BENCH_MCPS -split ','
    $mcps = $mcps | Where-Object { $_.name -in $allowed }
    Write-Host "Filtered MCPs by BENCH_MCPS: $($mcps.name -join ', ')"
}
if ($env:BENCH_QUERIES) {
    $qAllowed = $env:BENCH_QUERIES -split ','
    $queries = $queries | Where-Object { $_.id -in $qAllowed }
    Write-Host "Filtered queries by BENCH_QUERIES: $(($queries | ForEach-Object { $_.id }) -join ', ')"
}

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

$summary = $results | ConvertTo-Json -Depth 10
Set-Content -Path (Join-Path $OutputDir 'summary.json') -Value $summary -Encoding utf8
"--- BENCH RUN END $(Get-Date -Format o) ---" | Add-Content -Path $ProgressLog
Write-Host "DONE. Summary at $OutputDir\summary.json"
