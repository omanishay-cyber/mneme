param(
    [string]$ResultsDir = 'C:\Users\Anish\Desktop\temp\mcp-bench-2026-05-02\results'
)

$markersPattern = 'build_or_migrate|inject_file|Store::new|DbBuilder|PathManager|IncrementalParser|parse_file|index_files|build_pipeline|tree_to_nodes|paths::PathManager|use mneme_common::paths|mneme_home_override|MNEME_HOME|common/src/paths|Store::open|rusqlite|graph\.db|worker_ipc|livebus|parser_pool|migrate|r2d2|Mutex|Atomic|busy_timeout|RwLock|Singleton|Facade|Builder|build\.rs|builder\.rs|inject\.rs|paths\.rs|lib\.rs'

$mcps = @('mneme','tree-sitter','code-review-graph','graphify')
$queries = @('Q1','Q2','Q3','Q4','Q5')

$cells = @{}
$totals = @{}
foreach ($mcp in $mcps) {
    $cells[$mcp] = @{}
    $totals[$mcp] = @{ wall = 0; out = 0; cost = 0.0; score_sum = 0; n = 0 }
}

foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $f = Join-Path $ResultsDir "$mcp-$q.json"
        $cell = @{ wall = 0; out = 0; cost = 0.0; score = 0; note = '' }
        if (-not (Test-Path $f)) {
            $cell.note = 'no_file'
            $cells[$mcp][$q] = $cell
            $totals[$mcp].n += 1
            continue
        }
        $raw = Get-Content $f -Raw -ErrorAction SilentlyContinue
        try {
            $j = $raw | ConvertFrom-Json
            $cell.wall = if ($j.duration_ms) { [int]($j.duration_ms / 1000) } else { 0 }
            $cell.out = if ($j.usage.output_tokens) { [int]$j.usage.output_tokens } else { 0 }
            $cell.cost = if ($j.total_cost_usd) { [math]::Round([double]$j.total_cost_usd, 4) } else { 0.0 }
            $resultText = $j.result -as [string]
            if (-not $resultText) {
                $cell.score = 0
                $cell.note = 'no_text'
            } elseif ($resultText -match '(?i)(cannot answer|not.+been built|no data|shard not found|empty results|no graph|TIMEOUT_AFTER|EMPTY_STDOUT)') {
                $cell.score = 0
                $cell.note = 'no answer'
            } else {
                $hits = ([regex]::Matches($resultText, "(?i)$markersPattern") | Measure-Object).Count
                if ($hits -ge 12) { $cell.score = 9 }
                elseif ($hits -ge 8) { $cell.score = 8 }
                elseif ($hits -ge 5) { $cell.score = 7 }
                elseif ($hits -ge 3) { $cell.score = 5 }
                elseif ($hits -ge 1) { $cell.score = 4 }
                else { $cell.score = 2 }
                $cell.note = "$hits markers"
            }
            $totals[$mcp].wall += $cell.wall
            $totals[$mcp].out += $cell.out
            $totals[$mcp].cost += $cell.cost
            $totals[$mcp].score_sum += $cell.score
            $totals[$mcp].n += 1
        } catch {
            $cell.note = "parse_error: $_"
            $cell.score = 0
            $totals[$mcp].n += 1
        }
        $cells[$mcp][$q] = $cell
    }
}

Write-Output ""
Write-Output "| Query | mneme v0.3.2 | tree-sitter v0.7.0 | CRG v2.3.2 | graphify v0.3.0 |"
Write-Output "|---|---|---|---|---|"
foreach ($q in $queries) {
    $row = "| $q | "
    foreach ($mcp in $mcps) {
        $c = $cells[$mcp][$q]
        $row += "$($c.wall)s / $($c.out)t / `$$($c.cost) / **$($c.score)**/10 | "
    }
    Write-Output ($row.TrimEnd(' ').TrimEnd('|') + ' |')
}

$avgs = @{}
foreach ($mcp in $mcps) {
    $n = [math]::Max(1, $totals[$mcp].n)
    $avgs[$mcp] = [int]($totals[$mcp].score_sum / $n)
}

$totalRow = "| **Totals** |"
foreach ($mcp in $mcps) {
    $totalRow += " $($totals[$mcp].wall)s / $($totals[$mcp].out)t / `$$([math]::Round($totals[$mcp].cost,4)) / **$($avgs[$mcp])**/10 avg |"
}
Write-Output $totalRow

Write-Output ""
Write-Output "## Per-cell notes"
foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $c = $cells[$mcp][$q]
        Write-Output "- $mcp/$q : score=$($c.score) wall=$($c.wall)s tokens=$($c.out) cost=`$$($c.cost) note=`"$($c.note)`""
    }
}
